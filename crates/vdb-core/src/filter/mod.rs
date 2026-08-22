//! Metadata filters.
//!
//! A small typed expression tree, not a query language: there is no parser, no planner worth
//! the name, and no SQL. That is a deliberate limit from
//! `docs/architecture/01-scope.md` §1.4 — a query language is a large, permanently-supported
//! surface, and the workload here is "narrow a vector search by a handful of fields".
//!
//! # Filters cannot fail
//!
//! Evaluation is **total**. A filter is validated once when it is built — depth, node count,
//! nothing else — and after that every combination of filter and document yields `true` or
//! `false`, never an error. Comparing a string to a number is `false`. Descending into a scalar
//! is `false`. A field no document has is simply absent.
//!
//! This matters more than it looks. A filter that can fail at evaluation time turns one bad
//! document into a failed search over an entire collection, and the caller cannot fix it —
//! their query was fine. Making the semantics total means a filter's behaviour is a property of
//! the filter, not of the data it happens to meet.
//!
//! The precise rules are in `docs/api/filters.md` and pinned by the tests in this module.

mod eval;

pub use eval::matches;

use crate::error::{Result, ValidationError};
use crate::metadata::Value;
use crate::validation::limits;

/// A path to a metadata field, `.` descending into nested maps.
///
/// `"user.plan"` finds `plan` inside the map at `user`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Field(String);

impl Field {
    /// A field path.
    pub fn new(path: impl Into<String>) -> Self {
        Self(path.into())
    }

    /// The path as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Field {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<String> for Field {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

/// A predicate over a document's metadata.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Filter {
    /// Every child must match. An empty `And` matches everything.
    And(Vec<Filter>),
    /// At least one child must match. An empty `Or` matches nothing.
    Or(Vec<Filter>),
    /// The child must not match.
    Not(Box<Filter>),

    /// The field equals the value.
    Eq(Field, Value),
    /// The field does not equal the value. The exact negation of [`Filter::Eq`].
    Ne(Field, Value),

    /// The field is greater than the value.
    Gt(Field, Value),
    /// The field is greater than or equal to the value.
    Gte(Field, Value),
    /// The field is less than the value.
    Lt(Field, Value),
    /// The field is less than or equal to the value.
    Lte(Field, Value),

    /// The field equals one of the values. An empty list matches nothing.
    In(Field, Vec<Value>),
    /// The field equals none of the values. An empty list matches everything.
    Nin(Field, Vec<Value>),

    /// The field is present, whatever its value — including an explicit null.
    Exists(Field),
    /// The field is absent, or present and null.
    IsNull(Field),

    /// The field is a string beginning with this prefix.
    StartsWith(Field, String),
    /// The field is an array containing this value.
    ///
    /// Array membership only. Substring matching would be a different operation wearing the
    /// same name, and conflating them makes both harder to reason about.
    Contains(Field, Value),
}

impl Filter {
    /// `field == value`.
    pub fn eq(field: impl Into<Field>, value: Value) -> Self {
        Self::Eq(field.into(), value)
    }

    /// `field != value`.
    pub fn ne(field: impl Into<Field>, value: Value) -> Self {
        Self::Ne(field.into(), value)
    }

    /// `field > value`.
    pub fn gt(field: impl Into<Field>, value: Value) -> Self {
        Self::Gt(field.into(), value)
    }

    /// `field >= value`.
    pub fn gte(field: impl Into<Field>, value: Value) -> Self {
        Self::Gte(field.into(), value)
    }

    /// `field < value`.
    pub fn lt(field: impl Into<Field>, value: Value) -> Self {
        Self::Lt(field.into(), value)
    }

    /// `field <= value`.
    pub fn lte(field: impl Into<Field>, value: Value) -> Self {
        Self::Lte(field.into(), value)
    }

    /// `field` is one of `values`.
    pub fn in_values(field: impl Into<Field>, values: Vec<Value>) -> Self {
        Self::In(field.into(), values)
    }

    /// `field` is none of `values`.
    pub fn not_in(field: impl Into<Field>, values: Vec<Value>) -> Self {
        Self::Nin(field.into(), values)
    }

    /// `field` is present.
    pub fn exists(field: impl Into<Field>) -> Self {
        Self::Exists(field.into())
    }

    /// `field` is absent or null.
    pub fn is_null(field: impl Into<Field>) -> Self {
        Self::IsNull(field.into())
    }

    /// `field` starts with `prefix`.
    pub fn starts_with(field: impl Into<Field>, prefix: impl Into<String>) -> Self {
        Self::StartsWith(field.into(), prefix.into())
    }

    /// `field` is an array containing `value`.
    pub fn contains(field: impl Into<Field>, value: Value) -> Self {
        Self::Contains(field.into(), value)
    }

    /// Conjunction.
    pub fn all(filters: Vec<Filter>) -> Self {
        Self::And(filters)
    }

    /// Disjunction.
    pub fn any(filters: Vec<Filter>) -> Self {
        Self::Or(filters)
    }

    /// Negation. Also available as the `!` operator.
    pub fn negate(filter: Filter) -> Self {
        Self::Not(Box::new(filter))
    }

    /// Combine with another filter, requiring both.
    #[must_use]
    pub fn and(self, other: Filter) -> Self {
        match self {
            Self::And(mut filters) => {
                filters.push(other);
                Self::And(filters)
            }
            first => Self::And(vec![first, other]),
        }
    }

    /// Combine with another filter, requiring either.
    #[must_use]
    pub fn or(self, other: Filter) -> Self {
        match self {
            Self::Or(mut filters) => {
                filters.push(other);
                Self::Or(filters)
            }
            first => Self::Or(vec![first, other]),
        }
    }

    /// Total nodes in the tree.
    pub fn node_count(&self) -> usize {
        match self {
            Self::And(children) | Self::Or(children) => {
                1 + children.iter().map(Self::node_count).sum::<usize>()
            }
            Self::Not(child) => 1 + child.node_count(),
            _ => 1,
        }
    }

    /// Deepest nesting, a leaf being depth 1.
    pub fn depth(&self) -> usize {
        match self {
            Self::And(children) | Self::Or(children) => {
                1 + children.iter().map(Self::depth).max().unwrap_or(0)
            }
            Self::Not(child) => 1 + child.depth(),
            _ => 1,
        }
    }

    /// Every field the filter reads, sorted and deduplicated.
    ///
    /// Not used yet. It is what a future planner needs to decide whether a secondary index can
    /// answer the filter without decoding each document's metadata, and it is cheap to expose
    /// now while the tree is small.
    pub fn referenced_fields(&self) -> Vec<&str> {
        let mut out = Vec::new();
        self.collect_fields(&mut out);
        out.sort_unstable();
        out.dedup();
        out
    }

    fn collect_fields<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Self::And(children) | Self::Or(children) => {
                for child in children {
                    child.collect_fields(out);
                }
            }
            Self::Not(child) => child.collect_fields(out),
            Self::Eq(f, _)
            | Self::Ne(f, _)
            | Self::Gt(f, _)
            | Self::Gte(f, _)
            | Self::Lt(f, _)
            | Self::Lte(f, _)
            | Self::In(f, _)
            | Self::Nin(f, _)
            | Self::Exists(f)
            | Self::IsNull(f)
            | Self::StartsWith(f, _)
            | Self::Contains(f, _) => out.push(f.as_str()),
        }
    }

    /// Check the filter against the engine's limits.
    ///
    /// The only thing that can ever reject a filter, and it happens once, before evaluation.
    /// Everything after this point is total.
    ///
    /// # Errors
    /// [`ValidationError::FilterTooComplex`] if the tree has too many nodes or nests too deeply.
    /// Both bounds exist because evaluation recurses, and unbounded recursion over an
    /// attacker-supplied tree overflows the stack — which aborts rather than returning an error.
    pub fn validate(&self) -> Result<()> {
        let nodes = self.node_count();
        let depth = self.depth();
        if nodes > limits::MAX_FILTER_NODES || depth > limits::MAX_FILTER_DEPTH {
            return Err(ValidationError::FilterTooComplex {
                nodes,
                depth,
                max_nodes: limits::MAX_FILTER_NODES,
                max_depth: limits::MAX_FILTER_DEPTH,
            }
            .into());
        }
        Ok(())
    }
}

impl core::ops::Not for Filter {
    type Output = Filter;

    fn not(self) -> Filter {
        Filter::negate(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_count_and_depth_describe_the_tree() {
        let leaf = Filter::eq("a", Value::I64(1));
        assert_eq!(leaf.node_count(), 1);
        assert_eq!(leaf.depth(), 1);

        let tree = Filter::all(vec![
            Filter::eq("a", Value::I64(1)),
            Filter::any(vec![
                Filter::eq("b", Value::I64(2)),
                Filter::negate(Filter::exists("c")),
            ]),
        ]);
        assert_eq!(tree.node_count(), 6);
        assert_eq!(tree.depth(), 4);
    }

    #[test]
    fn combinators_flatten_rather_than_nesting() {
        let f = Filter::eq("a", Value::I64(1))
            .and(Filter::eq("b", Value::I64(2)))
            .and(Filter::eq("c", Value::I64(3)));
        // Three conjuncts under one And, not three levels of nesting — which keeps a chain of
        // `.and()` calls from hitting the depth limit.
        assert_eq!(f.depth(), 2);
        assert_eq!(f.node_count(), 4);

        let f = Filter::eq("a", Value::I64(1))
            .or(Filter::eq("b", Value::I64(2)))
            .or(Filter::eq("c", Value::I64(3)));
        assert_eq!(f.depth(), 2);
    }

    #[test]
    fn a_reasonable_filter_validates() {
        let f = Filter::all(vec![
            Filter::eq("category", Value::Str("tools".into())),
            Filter::gte("price", Value::F64(10.0)),
            Filter::negate(Filter::exists("archived")),
        ]);
        f.validate().unwrap();
    }

    /// Evaluation recurses, so an unbounded tree would overflow the stack — an abort, not a
    /// catchable error.
    #[test]
    fn an_over_deep_filter_is_rejected() {
        let mut f = Filter::exists("a");
        for _ in 0..limits::MAX_FILTER_DEPTH + 2 {
            f = Filter::negate(f);
        }
        match f.validate() {
            Err(crate::DbError::Validation(ValidationError::FilterTooComplex {
                depth,
                max_depth,
                ..
            })) => {
                assert!(depth > max_depth);
            }
            other => panic!("expected FilterTooComplex, got {other:?}"),
        }
    }

    #[test]
    fn an_over_wide_filter_is_rejected() {
        let children: Vec<Filter> = (0..limits::MAX_FILTER_NODES + 1)
            .map(|i| Filter::eq(format!("f{i}"), Value::I64(1)))
            .collect();
        assert!(Filter::all(children).validate().is_err());
    }

    #[test]
    fn a_filter_at_exactly_the_limits_is_accepted() {
        let children: Vec<Filter> = (0..limits::MAX_FILTER_NODES - 1)
            .map(|i| Filter::eq(format!("f{i}"), Value::I64(1)))
            .collect();
        let f = Filter::all(children);
        assert_eq!(f.node_count(), limits::MAX_FILTER_NODES);
        f.validate().unwrap();
    }

    #[test]
    fn the_not_operator_is_the_same_as_the_constructor() {
        let f = Filter::exists("a");
        assert_eq!(!f.clone(), Filter::negate(f));
    }

    #[test]
    fn referenced_fields_are_sorted_and_deduplicated() {
        let f = Filter::all(vec![
            Filter::eq("zebra", Value::I64(1)),
            Filter::any(vec![
                Filter::eq("apple", Value::I64(2)),
                Filter::exists("zebra"),
            ]),
            Filter::negate(Filter::is_null("user.plan")),
        ]);
        assert_eq!(f.referenced_fields(), vec!["apple", "user.plan", "zebra"]);
    }
}
