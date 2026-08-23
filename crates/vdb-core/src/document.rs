//! Documents, and the two kinds of identifier the engine uses.
//!
//! There are deliberately two: [`DocId`] is what the user supplies and sees, and [`RowId`] is
//! the dense internal handle that indexes, bitmaps and search results use. Keeping them
//! separate is what lets the flat index address rows with a `u64` and a shift instead of
//! hashing a string on every candidate.

use crate::error::{IdRejection, Result, ValidationError};
use crate::metadata::Metadata;
use crate::validation::limits;
use crate::vector::VectorView;
use core::fmt;

pub use vdb_format::IdKind;

/// A user-supplied document identifier, unique within its collection.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DocId {
    /// A UTF-8 string id.
    Str(String),
    /// A 64-bit integer id. Cheaper per document in the in-memory id map, which is the
    /// engine's dominant per-document overhead.
    U64(u64),
}

impl DocId {
    /// Which kind this is.
    pub fn kind(&self) -> IdKindTag {
        match self {
            Self::Str(_) => IdKindTag::Str,
            Self::U64(_) => IdKindTag::U64,
        }
    }

    /// The persisted byte form: UTF-8 for strings, eight little-endian bytes for integers.
    ///
    /// The collection's `IdKind` says how to read them back, so no per-document tag is stored.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::Str(s) => s.as_bytes().to_vec(),
            Self::U64(v) => v.to_le_bytes().to_vec(),
        }
    }

    /// Rebuild from the persisted form.
    ///
    /// # Errors
    /// [`ValidationError::InvalidDocumentId`] if the bytes do not match the collection's kind.
    pub fn from_bytes(kind: IdKind, bytes: &[u8]) -> Result<Self> {
        match kind {
            IdKind::Str { .. } => {
                let s = core::str::from_utf8(bytes).map_err(|_| {
                    ValidationError::InvalidDocumentId {
                        reason: IdRejection::NotUtf8,
                        len: bytes.len(),
                        max: limits::MAX_DOC_ID_LEN,
                    }
                })?;
                Ok(Self::Str(s.to_owned()))
            }
            IdKind::U64 => {
                let arr =
                    <[u8; 8]>::try_from(bytes).map_err(|_| ValidationError::InvalidDocumentId {
                        reason: IdRejection::IllegalCharacter,
                        len: bytes.len(),
                        max: 8,
                    })?;
                Ok(Self::U64(u64::from_le_bytes(arr)))
            }
            // `IdKind` is #[non_exhaustive]: a kind this build does not know cannot be decoded,
            // and guessing would hand back the wrong id rather than an error.
            _ => Err(reject(
                IdRejection::IllegalCharacter,
                bytes.len(),
                limits::MAX_DOC_ID_LEN,
            )),
        }
    }

    /// Check the id against the engine's limits and the collection's kind.
    ///
    /// # Errors
    /// [`ValidationError::InvalidDocumentId`], naming why.
    pub fn validate(&self, kind: IdKind) -> Result<()> {
        match (self, kind) {
            (Self::Str(s), IdKind::Str { max_len }) => {
                let max = (max_len as usize).min(limits::MAX_DOC_ID_LEN);
                if s.is_empty() {
                    return Err(reject(IdRejection::Empty, 0, max));
                }
                if s.len() > max {
                    return Err(reject(IdRejection::TooLong, s.len(), max));
                }
                // Control characters would make ids unprintable in logs and error messages, and
                // a NUL would truncate them at any C boundary the bindings cross.
                if s.chars().any(|c| c.is_control()) {
                    return Err(reject(IdRejection::IllegalCharacter, s.len(), max));
                }
                Ok(())
            }
            (Self::U64(_), IdKind::U64) => Ok(()),
            (id, _) => Err(reject(
                IdRejection::IllegalCharacter,
                id.to_bytes().len(),
                limits::MAX_DOC_ID_LEN,
            )),
        }
    }

    /// A displayable form, for error messages.
    pub fn display(&self) -> String {
        match self {
            Self::Str(s) => s.clone(),
            Self::U64(v) => v.to_string(),
        }
    }
}

fn reject(reason: IdRejection, len: usize, max: usize) -> crate::DbError {
    ValidationError::InvalidDocumentId { reason, len, max }.into()
}

/// Printed as the id itself: `note-1`, or `42`.
///
/// Worth having because the first thing anyone does with a search result is print it, and
/// without this they get `Str("note-1")` from the `Debug` formatter — the quotes and the variant
/// name are noise in a log line, and working around it means a match on the enum in every
/// caller.
impl fmt::Display for DocId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Str(s) => f.write_str(s),
            Self::U64(n) => write!(f, "{n}"),
        }
    }
}

impl From<&str> for DocId {
    fn from(s: &str) -> Self {
        Self::Str(s.to_owned())
    }
}

impl From<String> for DocId {
    fn from(s: String) -> Self {
        Self::Str(s)
    }
}

impl From<u64> for DocId {
    fn from(v: u64) -> Self {
        Self::U64(v)
    }
}

/// Which representation a [`DocId`] uses, without its payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKindTag {
    /// A string id.
    Str,
    /// An integer id.
    U64,
}

/// The engine's internal row handle: a segment id and a row index, packed.
///
/// Packed rather than a struct so it fits in a register, sorts naturally by segment then row,
/// and can key a bitmap or a heap entry without a hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId(u64);

impl RowId {
    /// Build from a segment id and a row index within that segment.
    pub const fn new(segment: u32, row: u32) -> Self {
        Self(((segment as u64) << 32) | row as u64)
    }

    /// The segment this row lives in.
    pub const fn segment(self) -> u32 {
        (self.0 >> 32) as u32
    }

    /// The row's index within its segment.
    pub const fn row(self) -> u32 {
        self.0 as u32
    }

    /// The packed value, for serialization and for index structures.
    pub const fn as_u64(self) -> u64 {
        self.0
    }

    /// Rebuild from a packed value.
    pub const fn from_u64(v: u64) -> Self {
        Self(v)
    }
}

/// A document being written.
///
/// Borrows its vector and content so nothing is copied before the write-ahead log.
#[derive(Debug, Clone)]
pub struct DocumentInput<'a> {
    /// The document's id.
    pub id: DocId,
    /// Its vector.
    pub vector: VectorView<'a>,
    /// Optional metadata.
    pub metadata: Option<Metadata>,
    /// Optional opaque payload, such as the text the vector was derived from.
    pub content: Option<&'a [u8]>,
}

impl<'a> DocumentInput<'a> {
    /// A document with just an id and a vector.
    pub fn new(id: impl Into<DocId>, vector: VectorView<'a>) -> Self {
        Self {
            id: id.into(),
            vector,
            metadata: None,
            content: None,
        }
    }

    /// Attach metadata.
    #[must_use]
    pub fn with_metadata(mut self, metadata: Metadata) -> Self {
        self.metadata = Some(metadata);
        self
    }

    /// Attach content.
    #[must_use]
    pub fn with_content(mut self, content: &'a [u8]) -> Self {
        self.content = Some(content);
        self
    }

    /// Validate everything about this document against a collection.
    ///
    /// One call, so no write path can validate a subset by accident.
    ///
    /// # Errors
    /// Any [`ValidationError`].
    pub fn validate(&self, collection: &str, dimension: u32, id_kind: IdKind) -> Result<()> {
        self.id.validate(id_kind)?;
        self.vector.validate(collection, dimension)?;
        if let Some(m) = &self.metadata {
            m.validate()?;
        }
        if let Some(c) = self.content {
            if c.len() > limits::MAX_CONTENT_BYTES {
                return Err(ValidationError::MetadataTooLarge {
                    field: "<content>".to_owned(),
                    size: c.len(),
                    max: limits::MAX_CONTENT_BYTES,
                }
                .into());
            }
        }
        Ok(())
    }
}

/// A document read back out of the database.
#[derive(Debug, Clone, PartialEq)]
pub struct Document {
    /// The document's id.
    pub id: DocId,
    /// Its vector, present only when the read asked for it — it is by far the largest field.
    pub vector: Option<Vec<f32>>,
    /// Its metadata, empty when it has none.
    pub metadata: Metadata,
    /// Its content, if it has any and the read asked for it.
    pub content: Option<Vec<u8>>,
}

/// Which parts of a document a read should return.
///
/// Vectors dominate the cost of returning results — at 768 dimensions a hit carries 3 KB of
/// floats — so they are opt-in rather than opt-out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Include {
    /// Return the vector.
    pub vector: bool,
    /// Return the metadata.
    pub metadata: bool,
    /// Return the content.
    pub content: bool,
}

impl Include {
    /// Metadata only: the default, and what most searches actually need.
    pub const METADATA: Self = Self {
        vector: false,
        metadata: true,
        content: false,
    };
    /// Nothing but ids and scores.
    pub const NONE: Self = Self {
        vector: false,
        metadata: false,
        content: false,
    };
    /// Everything.
    pub const ALL: Self = Self {
        vector: true,
        metadata: true,
        content: true,
    };
}

impl Default for Include {
    fn default() -> Self {
        Self::METADATA
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::DbError;

    #[test]
    fn row_ids_pack_and_unpack() {
        for (segment, row) in [
            (0u32, 0u32),
            (1, 0),
            (0, 1),
            (7, 12_345),
            (u32::MAX, u32::MAX),
        ] {
            let id = RowId::new(segment, row);
            assert_eq!(id.segment(), segment, "segment for ({segment}, {row})");
            assert_eq!(id.row(), row, "row for ({segment}, {row})");
            assert_eq!(RowId::from_u64(id.as_u64()), id);
        }
    }

    /// Row ids must sort by segment and then by row, so a scan in id order is a sequential read.
    #[test]
    fn row_ids_sort_by_segment_then_row() {
        let mut ids = vec![
            RowId::new(2, 0),
            RowId::new(0, 5),
            RowId::new(1, 100),
            RowId::new(0, 1),
            RowId::new(1, 2),
        ];
        ids.sort_unstable();
        assert_eq!(
            ids,
            vec![
                RowId::new(0, 1),
                RowId::new(0, 5),
                RowId::new(1, 2),
                RowId::new(1, 100),
                RowId::new(2, 0),
            ]
        );
    }

    #[test]
    fn doc_ids_round_trip_through_bytes() {
        let s = DocId::Str("doc-1".into());
        assert_eq!(
            DocId::from_bytes(IdKind::Str { max_len: 512 }, &s.to_bytes()).unwrap(),
            s
        );

        let n = DocId::U64(u64::MAX);
        assert_eq!(DocId::from_bytes(IdKind::U64, &n.to_bytes()).unwrap(), n);
        assert_eq!(n.to_bytes().len(), 8);
    }

    #[test]
    fn invalid_utf8_is_not_a_string_id() {
        assert!(matches!(
            DocId::from_bytes(IdKind::Str { max_len: 512 }, &[0xFF, 0xFE]),
            Err(DbError::Validation(ValidationError::InvalidDocumentId {
                reason: IdRejection::NotUtf8,
                ..
            }))
        ));
    }

    #[test]
    fn a_u64_id_must_be_exactly_eight_bytes() {
        assert!(DocId::from_bytes(IdKind::U64, &[0u8; 7]).is_err());
        assert!(DocId::from_bytes(IdKind::U64, &[0u8; 9]).is_err());
        assert!(DocId::from_bytes(IdKind::U64, &[0u8; 8]).is_ok());
    }

    #[test]
    fn empty_and_overlong_string_ids_are_rejected() {
        let kind = IdKind::Str { max_len: 512 };
        assert!(matches!(
            DocId::Str(String::new()).validate(kind),
            Err(DbError::Validation(ValidationError::InvalidDocumentId {
                reason: IdRejection::Empty,
                ..
            }))
        ));

        let at_limit = DocId::Str("x".repeat(512));
        assert!(at_limit.validate(kind).is_ok());

        let over = DocId::Str("x".repeat(513));
        assert!(matches!(
            over.validate(kind),
            Err(DbError::Validation(ValidationError::InvalidDocumentId {
                reason: IdRejection::TooLong,
                ..
            }))
        ));
    }

    /// A NUL would truncate the id at every C boundary the bindings cross, and a newline would
    /// corrupt any log line it appears in.
    #[test]
    fn control_characters_are_rejected_in_ids() {
        let kind = IdKind::Str { max_len: 512 };
        for bad in ["with\0nul", "with\nnewline", "with\ttab", "\u{7f}"] {
            assert!(
                DocId::Str(bad.to_owned()).validate(kind).is_err(),
                "{bad:?} should be rejected"
            );
        }
        // Ordinary non-ASCII is fine.
        DocId::Str("ünïcödé-🧭".into()).validate(kind).unwrap();
    }

    #[test]
    fn the_collections_max_len_narrows_the_global_limit_but_cannot_widen_it() {
        let strict = IdKind::Str { max_len: 8 };
        assert!(DocId::Str("12345678".into()).validate(strict).is_ok());
        assert!(DocId::Str("123456789".into()).validate(strict).is_err());

        let attempted_widening = IdKind::Str { max_len: u32::MAX };
        let too_long = DocId::Str("x".repeat(limits::MAX_DOC_ID_LEN + 1));
        assert!(
            too_long.validate(attempted_widening).is_err(),
            "a collection must not be able to exceed the engine's own limit"
        );
    }

    #[test]
    fn an_id_of_the_wrong_kind_for_the_collection_is_rejected() {
        assert!(DocId::U64(1).validate(IdKind::Str { max_len: 64 }).is_err());
        assert!(DocId::Str("1".into()).validate(IdKind::U64).is_err());
    }

    #[test]
    fn document_validation_covers_every_field() {
        let values = [1.0f32, 2.0];
        let kind = IdKind::Str { max_len: 64 };

        let ok = DocumentInput::new("doc", VectorView::f32(&values));
        ok.validate("c", 2, kind).unwrap();

        // Wrong dimension.
        assert!(DocumentInput::new("doc", VectorView::f32(&values))
            .validate("c", 3, kind)
            .is_err());
        // Bad id.
        assert!(DocumentInput::new("", VectorView::f32(&values))
            .validate("c", 2, kind)
            .is_err());
        // Non-finite component.
        let bad = [1.0f32, f32::NAN];
        assert!(DocumentInput::new("doc", VectorView::f32(&bad))
            .validate("c", 2, kind)
            .is_err());
        // Oversized content.
        let big = vec![0u8; limits::MAX_CONTENT_BYTES + 1];
        assert!(DocumentInput::new("doc", VectorView::f32(&values))
            .with_content(&big)
            .validate("c", 2, kind)
            .is_err());
        // Content exactly at the limit is fine.
        let at_limit = vec![0u8; limits::MAX_CONTENT_BYTES];
        DocumentInput::new("doc", VectorView::f32(&values))
            .with_content(&at_limit)
            .validate("c", 2, kind)
            .unwrap();
    }

    #[test]
    fn include_defaults_to_metadata_only() {
        let d = Include::default();
        assert!(
            !d.vector,
            "vectors are the largest field and must be opt-in"
        );
        assert!(d.metadata);
        assert!(!d.content);
    }
}
