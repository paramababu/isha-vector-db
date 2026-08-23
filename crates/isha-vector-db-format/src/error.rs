//! Errors from decoding persisted bytes.
//!
//! Deliberately narrow, and deliberately **without paths**: this crate decodes byte slices and
//! has no idea what a file is called. `isha-vector-db-core` catches these and re-raises them as
//! `CorruptionError` with the path attached, which is the layer that actually knows it.
//!
//! Everything here means "the bytes are not what the format says they should be". There is no
//! I/O error variant, because this crate performs no I/O.

use core::fmt;

/// Result of a decode.
pub type Result<T> = core::result::Result<T, FormatError>;

/// Why some bytes could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FormatError {
    /// The file did not begin with the expected magic.
    BadMagic {
        /// The magic the reader required.
        expected: [u8; 8],
        /// The magic actually found.
        found: [u8; 8],
    },
    /// The format version is outside the range this build understands.
    UnsupportedVersion {
        /// The version the file declares.
        found: u16,
        /// The oldest version this build reads.
        min_readable: u16,
        /// The version this build writes.
        current: u16,
    },
    /// A checksum did not match the bytes it covers.
    ChecksumMismatch {
        /// Offset of the region the checksum covers.
        offset: u64,
        /// The checksum stored in the bytes.
        expected: u32,
        /// The checksum computed over what was read.
        found: u32,
    },
    /// The input ended before the structure did.
    ///
    /// The ordinary result of a torn write at the tail of an append-only file, which recovery
    /// treats as an expected truncation rather than corruption.
    Truncated {
        /// Where the read started.
        offset: u64,
        /// How many bytes the structure needed.
        needed: u64,
        /// How many were left.
        available: u64,
    },
    /// A length field claimed more bytes than the input holds.
    ///
    /// Kept separate from [`FormatError::Truncated`] because it means something different: the
    /// bytes are internally inconsistent, not merely cut short. It is also the exact condition
    /// that turns a naive parser into an out-of-memory crash, so it is worth being able to
    /// count in the wild.
    LengthExceedsInput {
        /// Offset of the length field.
        offset: u64,
        /// The length it claimed.
        claimed: u64,
        /// How many bytes were actually available.
        available: u64,
    },
    /// A field held a value the format does not define.
    Malformed {
        /// Offset of the offending field.
        offset: u64,
        /// What was wrong with it.
        kind: MalformedKind,
    },
}

/// The specific structural problem behind a [`FormatError::Malformed`].
///
/// An enum rather than a string so that callers, tests and fuzz triage can match on the exact
/// condition, and so no message is ever assembled at runtime on an error path.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MalformedKind {
    /// A reserved field was not zero. Reserved space is checked so it stays usable later.
    ReservedNotZero,
    /// `header_len` was smaller than the fixed header.
    HeaderTooShort,
    /// The file-kind tag is not one this build knows.
    UnknownFileKind([u8; 4]),
    /// A flag bit outside the defined set was set.
    UnknownFlags(u16),
    /// A type tag in a metadata value is not defined.
    UnknownValueTag(u8),
    /// A discriminant (metric, dtype, id kind, index kind, WAL op) is not defined.
    UnknownDiscriminant {
        /// Which field it was.
        field: &'static str,
        /// The value found.
        value: u8,
    },
    /// A varint was not encoded in the fewest possible bytes.
    NonCanonicalVarint,
    /// A string was not valid UTF-8.
    InvalidUtf8,
    /// Map keys were not in strictly ascending order, so the encoding is not canonical.
    KeysNotSorted,
    /// A map contained the same key twice.
    DuplicateKey,
    /// A value nested deeper than [`crate::MAX_VALUE_DEPTH`].
    ///
    /// Enforced during decode, not after: unbounded recursion over hostile input overflows the
    /// stack, and a stack overflow is an abort, not a catchable error.
    DepthExceeded {
        /// The limit.
        max: usize,
    },
    /// A count or dimension was zero where the format requires at least one.
    ZeroNotAllowed {
        /// Which field it was.
        field: &'static str,
    },
    /// A float field held NaN or an infinity, which are not storable.
    NonFiniteFloat,
    /// Two fields that must agree did not.
    Inconsistent {
        /// What disagreed.
        field: &'static str,
    },
}

impl FormatError {
    /// Whether this is the benign end-of-input a torn append leaves behind.
    ///
    /// Recovery uses this to distinguish "the process died mid-write, truncate the tail and
    /// carry on" from "these bytes are damaged, tell the user". Getting that distinction wrong
    /// in either direction is bad: too strict and every unclean shutdown looks like corruption;
    /// too lax and real damage is silently discarded.
    pub fn is_truncation(&self) -> bool {
        matches!(self, FormatError::Truncated { .. })
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { expected, found } => {
                write!(f, "wrong file kind: magic {} != {}", Magic(found), Magic(expected))
            }
            Self::UnsupportedVersion { found, min_readable, current } => write!(
                f,
                "format version {found} is outside the readable range {min_readable}..={current}"
            ),
            Self::ChecksumMismatch { offset, expected, found } => write!(
                f,
                "checksum mismatch at offset {offset}: stored {expected:#010x}, computed {found:#010x}"
            ),
            Self::Truncated { offset, needed, available } => write!(
                f,
                "input ends early at offset {offset}: needed {needed} bytes, {available} available"
            ),
            Self::LengthExceedsInput { offset, claimed, available } => write!(
                f,
                "length field at offset {offset} claims {claimed} bytes but only {available} exist"
            ),
            Self::Malformed { offset, kind } => {
                write!(f, "malformed structure at offset {offset}: {kind}")
            }
        }
    }
}

impl fmt::Display for MalformedKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReservedNotZero => f.write_str("a reserved field was not zero"),
            Self::HeaderTooShort => f.write_str("header_len is smaller than the fixed header"),
            Self::UnknownFileKind(tag) => write!(f, "unknown file kind {}", Tag(tag)),
            Self::UnknownFlags(bits) => write!(f, "unknown flag bits {bits:#06x}"),
            Self::UnknownValueTag(tag) => write!(f, "unknown metadata value tag {tag}"),
            Self::UnknownDiscriminant { field, value } => {
                write!(f, "unknown {field} discriminant {value}")
            }
            Self::NonCanonicalVarint => f.write_str("varint is not minimally encoded"),
            Self::InvalidUtf8 => f.write_str("string is not valid UTF-8"),
            Self::KeysNotSorted => f.write_str("map keys are not in ascending order"),
            Self::DuplicateKey => f.write_str("map contains a duplicate key"),
            Self::DepthExceeded { max } => write!(f, "value nested deeper than {max}"),
            Self::ZeroNotAllowed { field } => write!(f, "{field} must not be zero"),
            Self::NonFiniteFloat => f.write_str("float field is NaN or infinite"),
            Self::Inconsistent { field } => {
                write!(f, "{field} disagrees with the rest of the structure")
            }
        }
    }
}

/// Renders eight magic bytes readably, so a mismatch is diagnosable at a glance.
struct Magic<'a>(&'a [u8; 8]);

impl fmt::Display for Magic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for &b in self.0 {
            if b.is_ascii_graphic() {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        f.write_str("\"")
    }
}

struct Tag<'a>(&'a [u8; 4]);

impl fmt::Display for Tag<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\"")?;
        for &b in self.0 {
            if b.is_ascii_graphic() {
                write!(f, "{}", b as char)?;
            } else {
                write!(f, "\\x{b:02x}")?;
            }
        }
        f.write_str("\"")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_truncation_is_a_torn_tail() {
        assert!(FormatError::Truncated {
            offset: 0,
            needed: 4,
            available: 1
        }
        .is_truncation());
        assert!(!FormatError::LengthExceedsInput {
            offset: 0,
            claimed: 9,
            available: 1
        }
        .is_truncation());
        assert!(!FormatError::ChecksumMismatch {
            offset: 0,
            expected: 1,
            found: 2
        }
        .is_truncation());
    }

    #[test]
    fn messages_name_offsets_and_values() {
        let e = FormatError::ChecksumMismatch {
            offset: 4096,
            expected: 0xDEAD_BEEF,
            found: 1,
        };
        let s = e.to_string();
        assert!(s.contains("4096"), "{s}");
        assert!(s.contains("0xdeadbeef"), "{s}");

        let e = FormatError::Malformed {
            offset: 12,
            kind: MalformedKind::UnknownDiscriminant {
                field: "metric",
                value: 9,
            },
        };
        let s = e.to_string();
        assert!(s.contains("offset 12"), "{s}");
        assert!(s.contains("metric"), "{s}");
        assert!(s.contains('9'), "{s}");
    }

    #[test]
    fn magic_renders_unprintable_bytes_escaped() {
        let e = FormatError::BadMagic {
            expected: *b"VDB1MANI",
            found: [0, 1, 2, 3, 4, 5, 6, 7],
        };
        let s = e.to_string();
        assert!(s.contains("VDB1MANI"), "{s}");
        assert!(s.contains("\\x00\\x01"), "{s}");
    }
}
