//! Typed, filterable metadata attached to a document.
//!
//! A thin wrapper over the format's [`Value`] model, adding the engine's limits and the
//! dotted-path lookup that filters use. The value model itself lives with the format, because
//! it is the persisted shape and its encoding is a contract with data already on disk.

use std::collections::BTreeMap;

use crate::error::{Result, ValidationError};
use crate::validation::limits;

pub use isha_vector_db_format::Value;

/// A document's metadata: a string-keyed map of typed values.
///
/// Backed by a `BTreeMap`, so iteration order — and therefore the encoded bytes — is a
/// deterministic function of the contents rather than of insertion order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Metadata(BTreeMap<String, Value>);

impl Metadata {
    /// Empty metadata.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of top-level fields.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no fields.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Set a field, returning any previous value.
    pub fn insert(&mut self, key: impl Into<String>, value: Value) -> Option<Value> {
        self.0.insert(key.into(), value)
    }

    /// Remove a field.
    pub fn remove(&mut self, key: &str) -> Option<Value> {
        self.0.remove(key)
    }

    /// Look up a top-level field.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    /// Look up a field by dotted path, descending into nested maps.
    ///
    /// `"user.plan"` finds `plan` inside the map at `user`. A missing field, or a path that
    /// tries to descend into a scalar, is `None` rather than an error — filters must be total
    /// (see `docs/api/filters.md`), so there is no failure mode here to report.
    pub fn get_path(&self, path: &str) -> Option<&Value> {
        let mut segments = path.split('.');
        let mut current = self.0.get(segments.next()?)?;
        for segment in segments {
            match current {
                Value::Map(m) => current = m.get(segment)?,
                _ => return None,
            }
        }
        Some(current)
    }

    /// Iterate the top-level fields in key order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &Value)> {
        self.0.iter()
    }

    /// The underlying map.
    pub fn as_map(&self) -> &BTreeMap<String, Value> {
        &self.0
    }

    /// Build from a map.
    pub fn from_map(map: BTreeMap<String, Value>) -> Self {
        Self(map)
    }

    /// Nesting depth: 0 when empty, 1 for a flat map of scalars.
    pub fn depth(&self) -> usize {
        if self.0.is_empty() {
            return 0;
        }
        1 + self.0.values().map(Value::depth).max().unwrap_or(0) - 1
    }

    /// Encode to the persisted representation.
    ///
    /// # Errors
    /// [`ValidationError::MetadataTooLarge`] or [`ValidationError::MetadataDepthExceeded`], or a
    /// format error if a value cannot be encoded.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let value = Value::Map(self.0.clone());
        value.encode().map_err(crate::error::from_format)
    }

    /// Decode from the persisted representation.
    ///
    /// # Errors
    /// A corruption error if the bytes are not a valid encoded map.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            return Ok(Self::new());
        }
        match Value::decode(bytes).map_err(crate::error::from_format)? {
            Value::Map(m) => Ok(Self(m)),
            other => Err(crate::internal_error!(
                "metadata decoded as {} rather than a map",
                other.type_name()
            )),
        }
    }

    /// Check the engine's limits.
    ///
    /// # Errors
    /// [`ValidationError::MetadataDepthExceeded`] or [`ValidationError::MetadataTooLarge`],
    /// naming the field responsible where one can be identified.
    pub fn validate(&self) -> Result<()> {
        let depth = self.depth();
        if depth > limits::MAX_METADATA_DEPTH {
            return Err(ValidationError::MetadataDepthExceeded {
                depth,
                max: limits::MAX_METADATA_DEPTH,
            }
            .into());
        }
        // Size is measured on the encoded form, because that is what actually consumes storage
        // — and because measuring the in-memory form would give a limit that moves with
        // allocator behaviour.
        let value = Value::Map(self.0.clone());
        let encoded = value.encode().map_err(crate::error::from_format)?;
        if encoded.len() > limits::MAX_METADATA_BYTES {
            // Name the largest field: with a 64 KiB budget it is nearly always one field that
            // blew it, and "your metadata is too big" without saying which part is unhelpful.
            let culprit = self
                .0
                .iter()
                .max_by_key(|(_, v)| v.encode().map(|b| b.len()).unwrap_or(0))
                .map(|(k, _)| k.clone())
                .unwrap_or_else(|| "<document>".to_owned());
            return Err(ValidationError::MetadataTooLarge {
                field: culprit,
                size: encoded.len(),
                max: limits::MAX_METADATA_BYTES,
            }
            .into());
        }
        Ok(())
    }
}

impl FromIterator<(String, Value)> for Metadata {
    fn from_iter<T: IntoIterator<Item = (String, Value)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested() -> Metadata {
        let mut inner = BTreeMap::new();
        inner.insert("plan".to_owned(), Value::Str("pro".into()));
        inner.insert("seats".to_owned(), Value::I64(5));

        let mut m = Metadata::new();
        m.insert("category", Value::Str("tools".into()));
        m.insert("user", Value::Map(inner));
        m
    }

    #[test]
    fn round_trips_through_the_persisted_form() {
        let m = nested();
        assert_eq!(Metadata::decode(&m.encode().unwrap()).unwrap(), m);
    }

    #[test]
    fn empty_metadata_round_trips_and_decodes_from_nothing() {
        let m = Metadata::new();
        assert!(m.is_empty());
        assert_eq!(Metadata::decode(&m.encode().unwrap()).unwrap(), m);
        assert_eq!(Metadata::decode(&[]).unwrap(), m);
    }

    #[test]
    fn dotted_paths_descend_into_nested_maps() {
        let m = nested();
        assert_eq!(m.get_path("category"), Some(&Value::Str("tools".into())));
        assert_eq!(m.get_path("user.plan"), Some(&Value::Str("pro".into())));
        assert_eq!(m.get_path("user.seats"), Some(&Value::I64(5)));
    }

    /// Filters must be total: a path that cannot resolve is absent, never an error.
    #[test]
    fn an_unresolvable_path_is_absent_rather_than_an_error() {
        let m = nested();
        assert_eq!(m.get_path("missing"), None);
        assert_eq!(m.get_path("user.missing"), None);
        assert_eq!(
            m.get_path("category.nope"),
            None,
            "cannot descend into a scalar"
        );
        assert_eq!(m.get_path(""), None);
        assert_eq!(m.get_path("user.plan.deeper"), None);
    }

    #[test]
    fn depth_counts_nesting_from_zero() {
        assert_eq!(Metadata::new().depth(), 0);

        let mut flat = Metadata::new();
        flat.insert("a", Value::I64(1));
        assert_eq!(flat.depth(), 1);

        assert_eq!(nested().depth(), 2);
    }

    #[test]
    fn metadata_nested_past_the_limit_is_rejected() {
        let mut v = Value::Null;
        for _ in 0..limits::MAX_METADATA_DEPTH + 2 {
            let mut m = BTreeMap::new();
            m.insert("x".to_owned(), v);
            v = Value::Map(m);
        }
        let mut meta = Metadata::new();
        meta.insert("deep", v);
        assert!(meta.validate().is_err());
    }

    #[test]
    fn oversized_metadata_names_the_field_responsible() {
        let mut m = Metadata::new();
        m.insert("small", Value::Str("x".into()));
        m.insert(
            "enormous",
            Value::Bytes(vec![0u8; limits::MAX_METADATA_BYTES + 1]),
        );

        match m.validate() {
            Err(crate::DbError::Validation(ValidationError::MetadataTooLarge {
                field,
                size,
                max,
            })) => {
                assert_eq!(field, "enormous", "the error should name the largest field");
                assert!(size > max);
                assert_eq!(max, limits::MAX_METADATA_BYTES);
            }
            other => panic!("expected MetadataTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn metadata_just_under_the_limit_is_accepted() {
        let mut m = Metadata::new();
        // Leave room for the map framing and the key.
        m.insert(
            "payload",
            Value::Bytes(vec![0u8; limits::MAX_METADATA_BYTES - 64]),
        );
        m.validate().unwrap();
    }

    #[test]
    fn encoding_is_independent_of_insertion_order() {
        let mut a = Metadata::new();
        a.insert("zebra", Value::I64(1));
        a.insert("apple", Value::I64(2));

        let mut b = Metadata::new();
        b.insert("apple", Value::I64(2));
        b.insert("zebra", Value::I64(1));

        assert_eq!(a.encode().unwrap(), b.encode().unwrap());
        assert_eq!(a, b);
    }

    #[test]
    fn insert_and_remove_behave_like_a_map() {
        let mut m = Metadata::new();
        assert_eq!(m.insert("k", Value::I64(1)), None);
        assert_eq!(m.insert("k", Value::I64(2)), Some(Value::I64(1)));
        assert_eq!(m.len(), 1);
        assert_eq!(m.get("k"), Some(&Value::I64(2)));
        assert_eq!(m.remove("k"), Some(Value::I64(2)));
        assert!(m.is_empty());
    }
}
