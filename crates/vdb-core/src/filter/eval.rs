//! Filter evaluation, and the type rules that make it total.
//!
//! Every function here returns `bool`. None can fail. The rules below are the whole semantics,
//! and each one is pinned by a test — because a filter's behaviour on unexpected data is the
//! part users actually hit, and the part that is easiest to leave undefined by accident.

use core::cmp::Ordering;

use crate::filter::{Field, Filter};
use crate::metadata::{Metadata, Value};

/// Whether a document's metadata satisfies a filter.
///
/// Assumes the filter has been validated; an over-deep tree would recurse without bound. The
/// public API validates on the way in, so this is an internal invariant rather than a hazard.
pub fn matches(filter: &Filter, metadata: &Metadata) -> bool {
    match filter {
        // An empty `And` matches everything and an empty `Or` matches nothing — the identity
        // elements of each operation. Anything else makes `all(vec![])` behave differently from
        // an absent filter, which surprises callers building filters programmatically.
        Filter::And(children) => children.iter().all(|c| matches(c, metadata)),
        Filter::Or(children) => children.iter().any(|c| matches(c, metadata)),
        Filter::Not(child) => !matches(child, metadata),

        Filter::Eq(field, value) => equals(resolve(metadata, field), value),
        Filter::Ne(field, value) => !equals(resolve(metadata, field), value),

        Filter::Gt(field, value) => ordered(metadata, field, value, |o| o == Ordering::Greater),
        Filter::Gte(field, value) => ordered(metadata, field, value, |o| o != Ordering::Less),
        Filter::Lt(field, value) => ordered(metadata, field, value, |o| o == Ordering::Less),
        Filter::Lte(field, value) => ordered(metadata, field, value, |o| o != Ordering::Greater),

        Filter::In(field, values) => {
            let actual = resolve(metadata, field);
            values.iter().any(|v| equals(actual, v))
        }
        Filter::Nin(field, values) => {
            let actual = resolve(metadata, field);
            !values.iter().any(|v| equals(actual, v))
        }

        Filter::Exists(field) => resolve(metadata, field).is_some(),
        Filter::IsNull(field) => matches!(resolve(metadata, field), None | Some(Value::Null)),

        Filter::StartsWith(field, prefix) => match resolve(metadata, field) {
            Some(Value::Str(s)) => s.starts_with(prefix.as_str()),
            _ => false,
        },
        Filter::Contains(field, value) => match resolve(metadata, field) {
            Some(Value::Array(items)) => items.iter().any(|item| same(item, value)),
            _ => false,
        },
    }
    // No catch-all arm on purpose. `Filter` is `#[non_exhaustive]` to downstream crates, but
    // within this one every variant is visible — so adding an operator without teaching the
    // evaluator about it is a compile error rather than a silent `false`.
}

/// Look a field up, `None` meaning the path does not resolve.
fn resolve<'a>(metadata: &'a Metadata, field: &Field) -> Option<&'a Value> {
    metadata.get_path(field.as_str())
}

/// Equality between a resolved field and a filter's value.
///
/// An absent field equals `Null`. That is the rule that makes `Eq(field, Null)` mean "this
/// document has no meaningful value here", which is what people write it for — as opposed to
/// the pedantic reading where absent and null are distinct and neither is queryable.
/// [`Filter::Exists`] is there for callers who need the distinction.
fn equals(actual: Option<&Value>, expected: &Value) -> bool {
    match (actual, expected) {
        (None, Value::Null) => true,
        (None, _) => false,
        (Some(a), b) => same(a, b),
    }
}

/// Equality between two values.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        // Integers and floats compare numerically. A caller who wrote `Eq("count", F64(3.0))`
        // against a document holding `I64(3)` means the same thing, and a database that
        // disagreed would be technically defensible and practically infuriating.
        (Value::I64(x), Value::I64(y)) => x == y,
        (Value::F64(x), Value::F64(y)) => x == y,
        (Value::I64(x), Value::F64(y)) | (Value::F64(y), Value::I64(x)) => (*x as f64) == *y,
        (Value::Str(x), Value::Str(y)) => x == y,
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q))
        }
        (Value::Map(x), Value::Map(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|((xk, xv), (yk, yv))| xk == yk && same(xv, yv))
        }
        // Mismatched types are unequal rather than an error. There is no useful sense in which
        // a string might equal a number, and raising here would let one odd document fail an
        // entire search.
        _ => false,
    }
}

/// Apply an ordering comparison, false wherever an ordering is not defined.
fn ordered(
    metadata: &Metadata,
    field: &Field,
    expected: &Value,
    accept: impl Fn(Ordering) -> bool,
) -> bool {
    match resolve(metadata, field).and_then(|actual| compare(actual, expected)) {
        Some(ordering) => accept(ordering),
        // An absent field, a type mismatch, or a type with no natural order: all false. Note
        // that this makes `Gt` and `Lte` both false for such a document, so they are *not*
        // negations of each other. That is intentional and matches SQL's three-valued logic in
        // effect, without the third value leaking into the API.
        None => false,
    }
}

/// Order two values, `None` where no ordering is defined.
fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::I64(x), Value::I64(y)) => Some(x.cmp(y)),
        (Value::F64(x), Value::F64(y)) => x.partial_cmp(y),
        (Value::I64(x), Value::F64(y)) => (*x as f64).partial_cmp(y),
        (Value::F64(x), Value::I64(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        (Value::Bytes(x), Value::Bytes(y)) => Some(x.cmp(y)),
        // Null, arrays and maps have no ordering anyone would agree on, so they have none here.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn doc() -> Metadata {
        let mut inner = BTreeMap::new();
        inner.insert("plan".to_owned(), Value::Str("pro".into()));
        inner.insert("seats".to_owned(), Value::I64(5));

        let mut m = Metadata::new();
        m.insert("category", Value::Str("tools".into()));
        m.insert("price", Value::F64(19.99));
        m.insert("count", Value::I64(3));
        m.insert("active", Value::Bool(true));
        m.insert("nothing", Value::Null);
        m.insert(
            "tags",
            Value::Array(vec![Value::Str("hand".into()), Value::Str("metal".into())]),
        );
        m.insert("user", Value::Map(inner));
        m
    }

    fn m(filter: Filter) -> bool {
        matches(&filter, &doc())
    }

    #[test]
    fn equality_on_each_scalar_type() {
        assert!(m(Filter::eq("category", Value::Str("tools".into()))));
        assert!(!m(Filter::eq("category", Value::Str("toys".into()))));
        assert!(m(Filter::eq("count", Value::I64(3))));
        assert!(m(Filter::eq("active", Value::Bool(true))));
        assert!(!m(Filter::eq("active", Value::Bool(false))));
    }

    /// A caller who writes `3.0` against a document holding `3` means the same thing.
    #[test]
    fn integers_and_floats_compare_numerically_across_the_boundary() {
        assert!(m(Filter::eq("count", Value::F64(3.0))));
        assert!(m(Filter::gte("count", Value::F64(2.5))));
        assert!(m(Filter::lt("count", Value::F64(3.5))));
        assert!(m(Filter::gt("price", Value::I64(19))));
        assert!(m(Filter::lt("price", Value::I64(20))));
    }

    #[test]
    fn ordering_on_strings_and_bools() {
        assert!(m(Filter::gt("category", Value::Str("apple".into()))));
        assert!(m(Filter::lt("category", Value::Str("zebra".into()))));
        assert!(m(Filter::gt("active", Value::Bool(false))));
    }

    /// The central rule: a type mismatch is `false`, never an error.
    #[test]
    fn comparing_mismatched_types_is_false_rather_than_an_error() {
        assert!(!m(Filter::eq("category", Value::I64(1))));
        assert!(!m(Filter::gt("category", Value::I64(1))));
        assert!(!m(Filter::lt("count", Value::Str("x".into()))));
        assert!(!m(Filter::gte("active", Value::F64(1.0))));
    }

    /// `Gt` and `Lte` are both false where no ordering exists, so they are not negations of one
    /// another. Pinned deliberately: it looks like a bug until you know it is the rule.
    #[test]
    fn ordering_comparisons_are_not_negations_of_each_other() {
        let f = Filter::gt("category", Value::I64(1));
        let g = Filter::lte("category", Value::I64(1));
        assert!(!m(f));
        assert!(!m(g));
        // Whereas Ne genuinely is the negation of Eq.
        assert_eq!(
            m(Filter::ne("category", Value::I64(1))),
            !m(Filter::eq("category", Value::I64(1)))
        );
    }

    #[test]
    fn a_missing_field_behaves_predictably() {
        assert!(!m(Filter::eq("absent", Value::I64(1))));
        assert!(
            m(Filter::ne("absent", Value::I64(1))),
            "Ne is the exact negation of Eq"
        );
        assert!(!m(Filter::gt("absent", Value::I64(1))));
        assert!(!m(Filter::exists("absent")));
        assert!(m(Filter::is_null("absent")));
        assert!(
            m(Filter::eq("absent", Value::Null)),
            "an absent field equals null"
        );
    }

    #[test]
    fn an_explicit_null_is_present_but_null() {
        assert!(
            m(Filter::exists("nothing")),
            "an explicit null is still present"
        );
        assert!(m(Filter::is_null("nothing")));
        assert!(m(Filter::eq("nothing", Value::Null)));
        assert!(!m(Filter::eq("nothing", Value::I64(0))));
    }

    #[test]
    fn dotted_paths_reach_nested_fields() {
        assert!(m(Filter::eq("user.plan", Value::Str("pro".into()))));
        assert!(m(Filter::gt("user.seats", Value::I64(3))));
        assert!(!m(Filter::exists("user.missing")));
        assert!(
            !m(Filter::exists("category.nope")),
            "cannot descend into a scalar"
        );
    }

    #[test]
    fn in_and_nin() {
        assert!(m(Filter::in_values(
            "category",
            vec![Value::Str("toys".into()), Value::Str("tools".into())]
        )));
        assert!(!m(Filter::in_values(
            "category",
            vec![Value::Str("toys".into())]
        )));
        assert!(m(Filter::not_in(
            "category",
            vec![Value::Str("toys".into())]
        )));

        // The identity cases.
        assert!(
            !m(Filter::in_values("category", vec![])),
            "in nothing matches nothing"
        );
        assert!(
            m(Filter::not_in("category", vec![])),
            "in none of nothing matches everything"
        );
    }

    #[test]
    fn starts_with_applies_only_to_strings() {
        assert!(m(Filter::starts_with("category", "too")));
        assert!(!m(Filter::starts_with("category", "xyz")));
        assert!(
            m(Filter::starts_with("category", "")),
            "every string starts with nothing"
        );
        assert!(
            !m(Filter::starts_with("count", "3")),
            "a number is not a string"
        );
        assert!(!m(Filter::starts_with("absent", "")));
    }

    #[test]
    fn contains_is_array_membership() {
        assert!(m(Filter::contains("tags", Value::Str("metal".into()))));
        assert!(!m(Filter::contains("tags", Value::Str("wood".into()))));
        assert!(
            !m(Filter::contains("category", Value::Str("too".into()))),
            "not a substring test"
        );
        assert!(!m(Filter::contains("absent", Value::Null)));
    }

    #[test]
    fn boolean_combinators() {
        assert!(m(Filter::all(vec![
            Filter::eq("category", Value::Str("tools".into())),
            Filter::gt("price", Value::F64(10.0)),
        ])));
        assert!(!m(Filter::all(vec![
            Filter::eq("category", Value::Str("tools".into())),
            Filter::gt("price", Value::F64(100.0)),
        ])));
        assert!(m(Filter::any(vec![
            Filter::eq("category", Value::Str("toys".into())),
            Filter::gt("price", Value::F64(10.0)),
        ])));
        assert!(m(Filter::negate(Filter::exists("absent"))));
    }

    /// The identity elements. Anything else makes `all(vec![])` behave differently from no
    /// filter at all, which surprises callers building filters programmatically.
    #[test]
    fn empty_combinators_are_the_identity_of_their_operation() {
        assert!(
            m(Filter::all(vec![])),
            "an empty conjunction matches everything"
        );
        assert!(
            !m(Filter::any(vec![])),
            "an empty disjunction matches nothing"
        );
    }

    #[test]
    fn de_morgan_holds() {
        let a = Filter::eq("category", Value::Str("tools".into()));
        let b = Filter::gt("price", Value::F64(100.0));
        let left = Filter::negate(Filter::all(vec![a.clone(), b.clone()]));
        let right = Filter::any(vec![Filter::negate(a), Filter::negate(b)]);
        assert_eq!(m(left), m(right));
    }

    #[test]
    fn evaluation_against_empty_metadata_is_total() {
        let empty = Metadata::new();
        for filter in [
            Filter::eq("a", Value::I64(1)),
            Filter::ne("a", Value::I64(1)),
            Filter::gt("a", Value::I64(1)),
            Filter::in_values("a", vec![Value::I64(1)]),
            Filter::exists("a"),
            Filter::is_null("a"),
            Filter::starts_with("a", "x"),
            Filter::contains("a", Value::I64(1)),
            Filter::all(vec![]),
            Filter::any(vec![]),
        ] {
            // The assertion is simply that this returns rather than panicking or erroring.
            let _ = matches(&filter, &empty);
        }
        assert!(!matches(&Filter::exists("a"), &empty));
        assert!(matches(&Filter::is_null("a"), &empty));
    }

    #[test]
    fn arrays_and_maps_compare_structurally_but_have_no_order() {
        let mut m1 = Metadata::new();
        m1.insert("list", Value::Array(vec![Value::I64(1), Value::I64(2)]));

        assert!(matches(
            &Filter::eq("list", Value::Array(vec![Value::I64(1), Value::I64(2)])),
            &m1
        ));
        assert!(!matches(
            &Filter::eq("list", Value::Array(vec![Value::I64(2), Value::I64(1)])),
            &m1
        ));
        assert!(
            !matches(&Filter::gt("list", Value::Array(vec![])), &m1),
            "arrays have no ordering"
        );
    }
}
