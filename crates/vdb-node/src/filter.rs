//! Turning a JavaScript object into a filter.
//!
//! Node binds the engine directly rather than through the C ABI, so it does not need the
//! postfix builder the C bindings use — it can take the shape a JavaScript developer would
//! write anyway:
//!
//! ```js
//! { category: 'tools', price: { $lt: 50 } }              // both must hold
//! { $or: [{ category: 'toys' }, { price: { $gt: 50 } }] }
//! { $not: { archived: true } }
//! { tags: { $contains: 'sharp' } }
//! { price: { $exists: true } }
//! ```
//!
//! The query-object convention is borrowed deliberately: it is what a JavaScript developer
//! expects from a database, and inventing a different one would cost familiarity for nothing.
//! What is *not* borrowed is MongoDB's semantics — the type rules are vdb's, and they are total.
//! `docs/api/filters.md` is the reference, including the three rules that surprise people.
//!
//! # Bare keys mean equality, and several keys mean conjunction
//!
//! `{ a: 1, b: 2 }` is `a == 1 && b == 2`. That is what the shape looks like it means, and a
//! filter language whose obvious reading is wrong is worse than one with no shorthand at all.

use napi::bindgen_prelude::{Object, Result};
use napi::{Error as NapiError, JsString, JsUnknown, ValueType};

use vdb_core::filter::Filter;
use vdb_core::metadata::Value;

/// Deepest nesting accepted, matching the engine's own limit.
///
/// Checked on the way down rather than after building: this walk recurses, and a deeply nested
/// object from an untrusted source would otherwise overflow the stack, which aborts rather than
/// returning an error anyone can handle.
const MAX_DEPTH: usize = 32;

/// Parse a filter object.
///
/// No `Env` parameter: `Object` carries its own, and taking one changed how napi-rs mapped the
/// JavaScript arguments — the filter silently arrived as `None` and every filter was ignored.
pub(crate) fn parse(object: Object) -> Result<Filter> {
    parse_object(object, 1)
}

fn parse_object(object: Object, depth: usize) -> Result<Filter> {
    if depth > MAX_DEPTH {
        return Err(NapiError::from_reason(format!(
            "filter nested deeper than {MAX_DEPTH}"
        )));
    }
    let names = object.get_property_names()?;
    let count = names.get_array_length()?;
    let mut clauses = Vec::with_capacity(count as usize);

    for index in 0..count {
        let key = property_name(&names, index)?;
        let value: JsUnknown = object.get_named_property(&key)?;

        let clause = match key.as_str() {
            "$and" => Filter::all(parse_list(value, depth)?),
            "$or" => Filter::any(parse_list(value, depth)?),
            "$not" => {
                if value.get_type()? != ValueType::Object {
                    return Err(NapiError::from_reason("$not takes a filter object"));
                }
                // SAFETY: the type was checked immediately above.
                let inner: Object = unsafe { value.cast() };
                Filter::negate(parse_object(inner, depth + 1)?)
            }
            other if other.starts_with('$') => {
                return Err(NapiError::from_reason(format!(
                    "unknown filter operator {other:?}; expected $and, $or or $not at the top level"
                )))
            }
            field => parse_field(field, value, depth)?,
        };
        clauses.push(clause);
    }

    // Several keys in one object mean conjunction; one means itself. An empty object matches
    // everything, which is the identity of `and` and what `{}` visibly ought to mean.
    Ok(match clauses.len() {
        1 => clauses
            .into_iter()
            .next()
            .unwrap_or_else(|| Filter::all(vec![])),
        _ => Filter::all(clauses),
    })
}

/// One property name, as a Rust string.
///
/// Property names arrive as JavaScript strings rather than Rust ones; the conversion is here
/// so the two walkers below do not each spell it out.
fn property_name(names: &Object, index: u32) -> Result<String> {
    let raw: JsString = names.get_element(index)?;
    raw.into_utf8()?.into_owned()
}

fn parse_list(value: JsUnknown, depth: usize) -> Result<Vec<Filter>> {
    if value.get_type()? != ValueType::Object {
        return Err(NapiError::from_reason(
            "$and and $or take an array of filters",
        ));
    }
    // SAFETY: the type was checked immediately above.
    let array: Object = unsafe { value.cast() };
    if !array.is_array()? {
        return Err(NapiError::from_reason(
            "$and and $or take an array of filters",
        ));
    }
    let length = array.get_array_length()?;
    let mut out = Vec::with_capacity(length as usize);
    for index in 0..length {
        let element: Object = array.get_element(index)?;
        out.push(parse_object(element, depth + 1)?);
    }
    Ok(out)
}

/// One field's clause: a bare value means equality, an object means operators.
fn parse_field(field: &str, value: JsUnknown, depth: usize) -> Result<Filter> {
    if value.get_type()? == ValueType::Object {
        // SAFETY: the type was checked immediately above.
        let operators: Object = unsafe { value.cast() };
        // An array as a field value would be ambiguous — equality with the array, or a set
        // membership test? — so it is refused rather than guessed at.
        if operators.is_array()? {
            return Err(NapiError::from_reason(format!(
                "field {field:?}: an array is not a filter value; use {{ $contains: … }} for \
                 array membership"
            )));
        }
        return parse_operators(field, operators, depth);
    }
    Ok(Filter::eq(field, scalar(value, field)?))
}

fn parse_operators(field: &str, operators: Object, depth: usize) -> Result<Filter> {
    let _ = depth;
    let names = operators.get_property_names()?;
    let count = names.get_array_length()?;
    let mut clauses = Vec::with_capacity(count as usize);

    for index in 0..count {
        let op = property_name(&names, index)?;
        let raw: JsUnknown = operators.get_named_property(&op)?;

        let clause = match op.as_str() {
            "$eq" => Filter::eq(field, scalar(raw, field)?),
            "$ne" => Filter::ne(field, scalar(raw, field)?),
            "$gt" => Filter::gt(field, scalar(raw, field)?),
            "$gte" => Filter::gte(field, scalar(raw, field)?),
            "$lt" => Filter::lt(field, scalar(raw, field)?),
            "$lte" => Filter::lte(field, scalar(raw, field)?),
            "$contains" => Filter::contains(field, scalar(raw, field)?),
            "$startsWith" => match scalar(raw, field)? {
                Value::Str(prefix) => Filter::starts_with(field, prefix),
                _ => {
                    return Err(NapiError::from_reason(format!(
                        "field {field:?}: $startsWith takes a string"
                    )))
                }
            },
            "$in" => Filter::in_values(field, scalar_list(raw, field)?),
            "$nin" => Filter::not_in(field, scalar_list(raw, field)?),
            "$exists" => {
                let present: bool = raw.coerce_to_bool()?.get_value()?;
                if present {
                    Filter::exists(field)
                } else {
                    Filter::is_null(field)
                }
            }
            other => {
                return Err(NapiError::from_reason(format!(
                    "field {field:?}: unknown operator {other:?}"
                )))
            }
        };
        clauses.push(clause);
    }

    Ok(match clauses.len() {
        1 => clauses
            .into_iter()
            .next()
            .unwrap_or_else(|| Filter::all(vec![])),
        _ => Filter::all(clauses),
    })
}

fn scalar_list(value: JsUnknown, field: &str) -> Result<Vec<Value>> {
    if value.get_type()? != ValueType::Object {
        return Err(NapiError::from_reason(format!(
            "field {field:?}: $in and $nin take an array"
        )));
    }
    // SAFETY: the type was checked immediately above.
    let array: Object = unsafe { value.cast() };
    if !array.is_array()? {
        return Err(NapiError::from_reason(format!(
            "field {field:?}: $in and $nin take an array"
        )));
    }
    let length = array.get_array_length()?;
    let mut out = Vec::with_capacity(length as usize);
    for index in 0..length {
        let element: JsUnknown = array.get_element(index)?;
        out.push(scalar(element, field)?);
    }
    Ok(out)
}

/// Convert one JavaScript value.
fn scalar(value: JsUnknown, field: &str) -> Result<Value> {
    Ok(match value.get_type()? {
        ValueType::String => Value::Str(value.coerce_to_string()?.into_utf8()?.into_owned()?),
        ValueType::Boolean => Value::Bool(value.coerce_to_bool()?.get_value()?),
        ValueType::Null | ValueType::Undefined => Value::Null,
        ValueType::Number => {
            let n: f64 = value.coerce_to_number()?.get_double()?;
            // Integral numbers become integers, matching how metadata is stored — otherwise
            // `{ count: 3 }` would compare a float against a stored integer and, although the
            // engine handles that correctly, the asymmetry would be a trap waiting for the day
            // it does not.
            if n.fract() == 0.0 && n.abs() < 9e15 {
                Value::I64(n as i64)
            } else {
                Value::F64(n)
            }
        }
        other => {
            return Err(NapiError::from_reason(format!(
                "field {field:?}: filter values must be strings, numbers, booleans or null; \
                 got {other:?}"
            )))
        }
    })
}
