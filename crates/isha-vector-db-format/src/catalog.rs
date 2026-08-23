//! A collection's immutable specification.
//!
//! Written once when the collection is created and never modified. Dimension and metric are
//! fixed here for the collection's lifetime: making them per-document would force every kernel
//! and every index to branch on them, for a use case (mixing embedding models in one
//! collection) that two collections serve better.

use crate::block::{decode_block, encode_block};
use crate::cursor::{Reader, Writer};
use crate::error::{FormatError, MalformedKind, Result};
use crate::header::FileKind;

/// Largest vector dimension the format can describe.
pub const MAX_DIMENSION: u32 = 65_536;

/// How similarity is measured in a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Metric {
    /// Cosine similarity.
    Cosine,
    /// Euclidean distance, computed and ranked as negated squared L2.
    L2,
    /// Inner product.
    Dot,
}

impl Metric {
    const fn code(self) -> u8 {
        match self {
            Self::Cosine => 1,
            Self::L2 => 2,
            Self::Dot => 3,
        }
    }

    fn from_code(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Cosine,
            2 => Self::L2,
            3 => Self::Dot,
            _ => return None,
        })
    }

    /// Every metric, for exhaustive tests and for `inspect`.
    pub const ALL: [Metric; 3] = [Self::Cosine, Self::L2, Self::Dot];

    /// A stable lowercase name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cosine => "cosine",
            Self::L2 => "l2",
            Self::Dot => "dot",
        }
    }
}

/// How vector components are stored.
///
/// Only `F32` exists in v1. The enum exists so `F16`, `I8` and binary vectors are additive
/// later rather than a breaking change to every signature that mentions a vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VectorDType {
    /// 32-bit IEEE-754 floats.
    F32,
}

impl VectorDType {
    const fn code(self) -> u8 {
        match self {
            Self::F32 => 1,
        }
    }

    fn from_code(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::F32),
            _ => None,
        }
    }

    /// Bytes per component.
    pub const fn component_size(self) -> usize {
        match self {
            Self::F32 => 4,
        }
    }

    /// Bytes one vector of `dimension` components occupies.
    pub const fn row_stride(self, dimension: u32) -> usize {
        self.component_size().saturating_mul(dimension as usize)
    }
}

/// What kind of document id a collection uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IdKind {
    /// UTF-8 string ids, at most `max_len` bytes.
    Str {
        /// Longest permitted id, in bytes.
        max_len: u32,
    },
    /// Unsigned 64-bit integer ids. Cheaper in memory, which matters at scale: the in-memory
    /// id map is the engine's dominant per-document overhead.
    U64,
}

/// Which index a collection uses.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum IndexSpec {
    /// Exact brute-force search.
    Flat,
}

impl IndexSpec {
    const fn code(&self) -> u8 {
        match self {
            Self::Flat => 1,
        }
    }

    /// A stable lowercase name.
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Flat => "flat",
        }
    }
}

/// A collection's immutable specification.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    /// The collection's name, as validated by the engine before it became a path component.
    pub name: String,
    /// Vector dimension. Fixed for the collection's lifetime.
    pub dimension: u32,
    /// Similarity metric. Fixed for the collection's lifetime.
    pub metric: Metric,
    /// Component type.
    pub dtype: VectorDType,
    /// Document id representation.
    pub id_kind: IdKind,
    /// Index configuration.
    pub index: IndexSpec,
    /// Creation time, in milliseconds since the Unix epoch, from the engine's injected clock.
    pub created_at_ms: u64,
}

impl Catalog {
    /// Serialize to a complete, checksummed `CATALOG` file.
    ///
    /// # Errors
    /// [`MalformedKind::ZeroNotAllowed`] or an out-of-range dimension.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut w = Writer::new();
        w.string(&self.name)
            .u32(self.dimension)
            .u8(self.metric.code())
            .u8(self.dtype.code());
        match self.id_kind {
            IdKind::Str { max_len } => {
                w.u8(1).u32(max_len);
            }
            IdKind::U64 => {
                w.u8(2).u32(0);
            }
        }
        // The index parameters are a length-delimited blob so a future index kind with its own
        // parameters does not change the surrounding layout.
        w.u8(self.index.code()).blob(&[]);
        w.u64(self.created_at_ms);
        w.reserved(8);
        Ok(encode_block(FileKind::Catalog, w.as_slice()))
    }

    /// Parse a `CATALOG` file.
    ///
    /// # Errors
    /// Any [`FormatError`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let payload = decode_block(bytes, FileKind::Catalog)?;
        let mut r = Reader::new(payload);

        let name = r.string()?.to_owned();
        let dim_at = r.offset();
        let dimension = r.u32()?;
        let metric_at = r.offset();
        let metric = Metric::from_code(r.u8()?).ok_or(FormatError::Malformed {
            offset: metric_at,
            kind: MalformedKind::UnknownDiscriminant {
                field: "metric",
                value: 0,
            },
        })?;
        let dtype_at = r.offset();
        let dtype_code = r.u8()?;
        let dtype = VectorDType::from_code(dtype_code).ok_or(FormatError::Malformed {
            offset: dtype_at,
            kind: MalformedKind::UnknownDiscriminant {
                field: "dtype",
                value: dtype_code,
            },
        })?;

        let id_at = r.offset();
        let id_code = r.u8()?;
        let max_len = r.u32()?;
        let id_kind = match id_code {
            1 => IdKind::Str { max_len },
            2 => IdKind::U64,
            other => {
                return Err(FormatError::Malformed {
                    offset: id_at,
                    kind: MalformedKind::UnknownDiscriminant {
                        field: "id_kind",
                        value: other,
                    },
                })
            }
        };

        let index_at = r.offset();
        let index_code = r.u8()?;
        let _params = r.blob()?;
        let index = match index_code {
            1 => IndexSpec::Flat,
            other => {
                return Err(FormatError::Malformed {
                    offset: index_at,
                    kind: MalformedKind::UnknownDiscriminant {
                        field: "index",
                        value: other,
                    },
                })
            }
        };

        let created_at_ms = r.u64()?;
        r.reserved(8)?;
        r.expect_end("catalog")?;

        let catalog = Self {
            name,
            dimension,
            metric,
            dtype,
            id_kind,
            index,
            created_at_ms,
        };
        catalog.validate().map_err(|e| match e {
            // Re-anchor validation failures at the field they came from.
            FormatError::Malformed { kind, .. } => FormatError::Malformed {
                offset: dim_at,
                kind,
            },
            other => other,
        })?;
        Ok(catalog)
    }

    /// Bytes one stored vector occupies.
    pub fn row_stride(&self) -> usize {
        self.dtype.row_stride(self.dimension)
    }

    fn validate(&self) -> Result<()> {
        if self.dimension == 0 {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::ZeroNotAllowed { field: "dimension" },
            });
        }
        if self.dimension > MAX_DIMENSION {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::Inconsistent { field: "dimension" },
            });
        }
        if self.name.is_empty() {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::ZeroNotAllowed {
                    field: "name length",
                },
            });
        }
        if let IdKind::Str { max_len } = self.id_kind {
            if max_len == 0 {
                return Err(FormatError::Malformed {
                    offset: 0,
                    kind: MalformedKind::ZeroNotAllowed {
                        field: "id max_len",
                    },
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalog {
        Catalog {
            name: "products".into(),
            dimension: 768,
            metric: Metric::Cosine,
            dtype: VectorDType::F32,
            id_kind: IdKind::Str { max_len: 512 },
            index: IndexSpec::Flat,
            created_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn round_trips() {
        let c = sample();
        let bytes = c.encode().unwrap();
        assert_eq!(Catalog::decode(&bytes).unwrap(), c);
    }

    #[test]
    fn round_trips_every_metric_and_id_kind() {
        for metric in Metric::ALL {
            for id_kind in [
                IdKind::U64,
                IdKind::Str { max_len: 1 },
                IdKind::Str { max_len: 512 },
            ] {
                let c = Catalog {
                    metric,
                    id_kind,
                    ..sample()
                };
                let decoded = Catalog::decode(&c.encode().unwrap()).unwrap();
                assert_eq!(decoded, c, "{metric:?} / {id_kind:?}");
            }
        }
    }

    #[test]
    fn dimension_boundaries() {
        let ok = Catalog {
            dimension: 1,
            ..sample()
        };
        assert!(ok.encode().is_ok());
        let ok = Catalog {
            dimension: MAX_DIMENSION,
            ..sample()
        };
        assert!(ok.encode().is_ok());

        let zero = Catalog {
            dimension: 0,
            ..sample()
        };
        assert!(matches!(
            zero.encode(),
            Err(FormatError::Malformed {
                kind: MalformedKind::ZeroNotAllowed { .. },
                ..
            })
        ));
        let huge = Catalog {
            dimension: MAX_DIMENSION + 1,
            ..sample()
        };
        assert!(huge.encode().is_err());
    }

    /// A catalog whose dimension was corrupted to zero must be rejected on the way in, not
    /// discovered later when the row stride turns out to be zero and every read aliases row 0.
    #[test]
    fn a_corrupted_dimension_is_caught_on_decode() {
        let c = Catalog {
            dimension: 4,
            ..sample()
        };
        let mut bytes = c.encode().unwrap();
        // Locate the dimension field: header(32) + varint len + name.
        let dim_off = 32 + 1 + c.name.len();
        bytes[dim_off..dim_off + 4].copy_from_slice(&0u32.to_le_bytes());
        // Repair the block checksum so only the semantic problem remains.
        let payload_len = bytes.len() - 32 - 4;
        let crc = crate::crc32c(&bytes[32..32 + payload_len]);
        let end = bytes.len();
        bytes[end - 4..].copy_from_slice(&crc.to_le_bytes());

        assert!(matches!(
            Catalog::decode(&bytes),
            Err(FormatError::Malformed {
                kind: MalformedKind::ZeroNotAllowed { .. },
                ..
            })
        ));
    }

    #[test]
    fn unknown_discriminants_are_named_in_the_error() {
        let c = sample();
        let bytes = c.encode().unwrap();
        let metric_off = 32 + 1 + c.name.len() + 4;

        let mut mutated = bytes.clone();
        mutated[metric_off] = 99;
        let payload_len = mutated.len() - 32 - 4;
        let crc = crate::crc32c(&mutated[32..32 + payload_len]);
        let end = mutated.len();
        mutated[end - 4..].copy_from_slice(&crc.to_le_bytes());

        match Catalog::decode(&mutated) {
            Err(FormatError::Malformed {
                kind: MalformedKind::UnknownDiscriminant { field, .. },
                ..
            }) => assert_eq!(field, "metric"),
            other => panic!("expected an unknown metric, got {other:?}"),
        }
    }

    #[test]
    fn row_stride_is_dimension_times_component_size() {
        assert_eq!(sample().row_stride(), 768 * 4);
        assert_eq!(
            Catalog {
                dimension: 1,
                ..sample()
            }
            .row_stride(),
            4
        );
    }

    #[test]
    fn truncation_at_every_length_is_an_error() {
        let bytes = sample().encode().unwrap();
        for len in 0..bytes.len() {
            assert!(
                Catalog::decode(&bytes[..len]).is_err(),
                "{len} bytes decoded"
            );
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0xABCD_1234_5678_9F00u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 96) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            let _ = Catalog::decode(&bytes);
        }
    }
}
