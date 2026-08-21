//! The 32-byte header every vdb file begins with.
//!
//! Before a reader interprets one byte of payload it knows: what kind of file this is, what
//! format version wrote it, whether the bytes are compressed or encrypted, how long the header
//! and payload are, and whether the header itself is intact. A file that cannot answer those
//! questions is rejected before anything else happens.
//!
//! ```text
//! offset  size  field
//!      0     8  magic: b"VDB1" + a four-byte kind tag
//!      8     2  format_version   (u16 LE)
//!     10     2  flags            (u16 LE)
//!     12     4  header_len       (u32 LE)  — allows additive growth without a version bump
//!     16     8  payload_len      (u64 LE)
//!     24     4  header_crc32c    (u32 LE)  — over bytes 0..24
//!     28     4  reserved         (must be zero)
//! ```
//!
//! `header_len` is the extension mechanism. A future version may append fields between offset
//! 32 and `header_len`; an older reader skips to `header_len` and carries on, because that
//! region is explicitly declared extensible. **Everywhere else in the format, unknown means
//! error** — the extensible regions are the exceptions, and they are named.

use crate::crc32c::crc32c;
use crate::error::{FormatError, MalformedKind, Result};
use crate::{Reader, Writer, FORMAT_VERSION, MAGIC_PREFIX, MIN_READABLE_VERSION};

/// Size of the fixed part of the header.
pub const HEADER_LEN: u32 = 32;

/// Offset at which the header checksum is stored, and the number of bytes it covers.
const CRC_COVERS: usize = 24;

/// Which kind of file this is.
///
/// The tag is part of the magic, so opening a segment file as a manifest fails immediately with
/// a specific error rather than producing nonsense.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FileKind {
    /// One of the two manifest slots.
    Manifest,
    /// A collection's immutable specification.
    Catalog,
    /// A write-ahead log segment.
    Wal,
    /// A segment's fixed-stride vector block.
    Vectors,
    /// A segment's row directory.
    Directory,
    /// A segment's metadata and content records.
    Metadata,
    /// A segment's live/tombstone bitmap.
    Deleted,
    /// An index snapshot.
    Index,
}

impl FileKind {
    /// The four-byte tag that follows [`MAGIC_PREFIX`].
    pub const fn tag(self) -> [u8; 4] {
        match self {
            Self::Manifest => *b"MANI",
            Self::Catalog => *b"CATL",
            Self::Wal => *b"WAL\0",
            Self::Vectors => *b"VEC\0",
            Self::Directory => *b"DIR\0",
            Self::Metadata => *b"META",
            Self::Deleted => *b"DEL\0",
            Self::Index => *b"IDX\0",
        }
    }

    /// The full eight-byte magic for this kind.
    pub const fn magic(self) -> [u8; 8] {
        let p = MAGIC_PREFIX;
        let t = self.tag();
        [p[0], p[1], p[2], p[3], t[0], t[1], t[2], t[3]]
    }

    /// Every kind, for exhaustive tests and for the `inspect` command.
    pub const ALL: [FileKind; 8] = [
        Self::Manifest,
        Self::Catalog,
        Self::Wal,
        Self::Vectors,
        Self::Directory,
        Self::Metadata,
        Self::Deleted,
        Self::Index,
    ];

    fn from_tag(tag: [u8; 4]) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.tag() == tag)
    }
}

/// Header flag bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeaderFlags(u16);

impl HeaderFlags {
    /// No flags. The only valid value in v1.
    pub const NONE: Self = Self(0);
    /// The payload is compressed. Reserved; not produced by v1.
    pub const COMPRESSED: Self = Self(1 << 0);
    /// The payload is encrypted. Reserved; not produced by v1.
    pub const ENCRYPTED: Self = Self(1 << 1);

    /// All bits this build understands.
    const KNOWN: u16 = 0b11;

    /// The raw bits.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether the payload is compressed.
    pub const fn is_compressed(self) -> bool {
        self.0 & Self::COMPRESSED.0 != 0
    }

    /// Whether the payload is encrypted.
    pub const fn is_encrypted(self) -> bool {
        self.0 & Self::ENCRYPTED.0 != 0
    }
}

/// A decoded file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHeader {
    /// What kind of file this is.
    pub kind: FileKind,
    /// The format version that wrote it.
    pub version: u16,
    /// Payload flags.
    pub flags: HeaderFlags,
    /// Total header length, at least [`HEADER_LEN`].
    pub header_len: u32,
    /// Length of the payload that follows the header.
    pub payload_len: u64,
}

impl FileHeader {
    /// A v1 header for `kind` with a payload of `payload_len` bytes.
    pub fn new(kind: FileKind, payload_len: u64) -> Self {
        Self {
            kind,
            version: FORMAT_VERSION,
            flags: HeaderFlags::NONE,
            header_len: HEADER_LEN,
            payload_len,
        }
    }

    /// Serialize into exactly [`HEADER_LEN`] bytes.
    pub fn encode(&self) -> [u8; HEADER_LEN as usize] {
        let mut w = Writer::with_capacity(HEADER_LEN as usize);
        w.raw(&self.kind.magic())
            .u16(self.version)
            .u16(self.flags.bits())
            .u32(self.header_len)
            .u64(self.payload_len);
        let crc = crc32c(w.as_slice());
        w.u32(crc).reserved(4);

        let mut out = [0u8; HEADER_LEN as usize];
        // The writer wrote exactly HEADER_LEN bytes; copy defensively rather than indexing.
        let src = w.as_slice();
        let n = src.len().min(out.len());
        if let (Some(dst), Some(src)) = (out.get_mut(..n), src.get(..n)) {
            dst.copy_from_slice(src);
        }
        out
    }

    /// Append this header to a writer.
    pub fn write_to(&self, w: &mut Writer) {
        w.raw(&self.encode());
    }

    /// Decode a header and check that it is the kind the caller expected.
    ///
    /// Checks happen in the order that produces the most useful diagnosis: magic first (is this
    /// even our file?), then the header checksum (are these bytes intact?), then the version
    /// (can we read it?), then structural fields.
    ///
    /// # Errors
    /// [`FormatError::BadMagic`], [`FormatError::ChecksumMismatch`],
    /// [`FormatError::UnsupportedVersion`], [`FormatError::Truncated`], or
    /// [`FormatError::Malformed`].
    pub fn decode(bytes: &[u8], expected: FileKind) -> Result<Self> {
        let header = Self::decode_any(bytes)?;
        if header.kind != expected {
            return Err(FormatError::BadMagic {
                expected: expected.magic(),
                found: header.kind.magic(),
            });
        }
        Ok(header)
    }

    /// Decode a header without knowing which kind to expect.
    ///
    /// Used by the `inspect` command, and by recovery when it is trying to work out what a
    /// stray file in the database directory actually is.
    ///
    /// # Errors
    /// As [`FileHeader::decode`].
    pub fn decode_any(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let magic: [u8; 8] = r.array().map_err(|_| FormatError::Truncated {
            offset: 0,
            needed: u64::from(HEADER_LEN),
            available: bytes.len() as u64,
        })?;

        let (prefix, tag) = magic.split_at(4);
        if prefix != MAGIC_PREFIX {
            return Err(FormatError::BadMagic {
                expected: unknown_magic(),
                found: magic,
            });
        }
        let mut tag_arr = [0u8; 4];
        tag_arr.copy_from_slice(tag);
        let Some(kind) = FileKind::from_tag(tag_arr) else {
            return Err(FormatError::Malformed {
                offset: 4,
                kind: MalformedKind::UnknownFileKind(tag_arr),
            });
        };

        let version = r.u16()?;
        let flag_bits = r.u16()?;
        let header_len = r.u32()?;
        let payload_len = r.u64()?;
        let stored_crc = r.u32()?;

        // Verify the header's own integrity before trusting any field in it. A header whose
        // length fields are garbage would otherwise send the caller reading at a wild offset.
        let covered = bytes.get(..CRC_COVERS).ok_or(FormatError::Truncated {
            offset: 0,
            needed: CRC_COVERS as u64,
            available: bytes.len() as u64,
        })?;
        let computed = crc32c(covered);
        if computed != stored_crc {
            return Err(FormatError::ChecksumMismatch {
                offset: 0,
                expected: stored_crc,
                found: computed,
            });
        }

        if version < MIN_READABLE_VERSION || version > FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                found: version,
                min_readable: MIN_READABLE_VERSION,
                current: FORMAT_VERSION,
            });
        }
        if flag_bits & !HeaderFlags::KNOWN != 0 {
            return Err(FormatError::Malformed {
                offset: 10,
                kind: MalformedKind::UnknownFlags(flag_bits),
            });
        }
        if header_len < HEADER_LEN {
            return Err(FormatError::Malformed {
                offset: 12,
                kind: MalformedKind::HeaderTooShort,
            });
        }
        r.reserved(4)?;

        Ok(Self {
            kind,
            version,
            flags: HeaderFlags(flag_bits),
            header_len,
            payload_len,
        })
    }

    /// Check that a file is long enough to hold the payload the header promises.
    ///
    /// `checked_add` rather than `saturating_add`: saturation would quietly turn an absurd
    /// `payload_len` into a value that happens to fit, which is exactly the arithmetic mistake
    /// that lets a corrupt header through. A header whose extent overflows `u64` cannot
    /// describe any real file, so it is malformed.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] if the file is shorter than `header_len + payload_len`, or
    /// [`MalformedKind::Inconsistent`] if that sum overflows.
    pub fn check_file_len(&self, file_len: u64) -> Result<()> {
        let Some(needed) = u64::from(self.header_len).checked_add(self.payload_len) else {
            return Err(FormatError::Malformed {
                offset: 16,
                kind: MalformedKind::Inconsistent {
                    field: "payload_len",
                },
            });
        };
        if file_len < needed {
            return Err(FormatError::Truncated {
                offset: u64::from(self.header_len),
                needed,
                available: file_len,
            });
        }
        Ok(())
    }
}

/// Placeholder "expected" magic for a file that is not one of ours at all.
const fn unknown_magic() -> [u8; 8] {
    [
        MAGIC_PREFIX[0],
        MAGIC_PREFIX[1],
        MAGIC_PREFIX[2],
        MAGIC_PREFIX[3],
        b'?',
        b'?',
        b'?',
        b'?',
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_exactly_thirty_two_bytes() {
        let h = FileHeader::new(FileKind::Manifest, 100);
        assert_eq!(h.encode().len(), HEADER_LEN as usize);
    }

    #[test]
    fn every_kind_round_trips() {
        for kind in FileKind::ALL {
            let h = FileHeader::new(kind, 4096);
            let decoded = FileHeader::decode(&h.encode(), kind).unwrap();
            assert_eq!(decoded, h, "{kind:?}");
            assert_eq!(decoded.version, FORMAT_VERSION);
            assert_eq!(decoded.payload_len, 4096);
        }
    }

    #[test]
    fn every_kind_has_a_distinct_tag() {
        let mut tags: Vec<[u8; 4]> = FileKind::ALL.iter().map(|k| k.tag()).collect();
        tags.sort_unstable();
        let before = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), before, "two file kinds share a tag");
    }

    #[test]
    fn decode_any_identifies_the_kind() {
        for kind in FileKind::ALL {
            let h = FileHeader::new(kind, 0);
            assert_eq!(FileHeader::decode_any(&h.encode()).unwrap().kind, kind);
        }
    }

    /// Opening one kind of file as another must fail immediately, not produce nonsense.
    #[test]
    fn the_wrong_kind_is_rejected() {
        let bytes = FileHeader::new(FileKind::Vectors, 0).encode();
        match FileHeader::decode(&bytes, FileKind::Manifest) {
            Err(FormatError::BadMagic { expected, found }) => {
                assert_eq!(expected, FileKind::Manifest.magic());
                assert_eq!(found, FileKind::Vectors.magic());
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn a_foreign_file_is_rejected_by_prefix() {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(b"SQLite f");
        assert!(matches!(
            FileHeader::decode_any(&bytes),
            Err(FormatError::BadMagic { .. })
        ));
    }

    #[test]
    fn an_unknown_kind_tag_is_named_in_the_error() {
        let mut bytes = FileHeader::new(FileKind::Manifest, 0).encode();
        bytes[4..8].copy_from_slice(b"WHAT");
        match FileHeader::decode_any(&bytes) {
            Err(FormatError::Malformed {
                kind: MalformedKind::UnknownFileKind(tag),
                ..
            }) => {
                assert_eq!(&tag, b"WHAT");
            }
            other => panic!("expected UnknownFileKind, got {other:?}"),
        }
    }

    /// The header checksum must be verified before any field in it is trusted.
    #[test]
    fn every_single_bit_flip_in_the_covered_region_is_caught() {
        let original = FileHeader::new(FileKind::Wal, 12_345).encode();
        for byte in 0..CRC_COVERS {
            for bit in 0..8 {
                let mut mutated = original;
                mutated[byte] ^= 1u8 << bit;
                let result = FileHeader::decode_any(&mutated);
                assert!(
                    result.is_err(),
                    "flip at byte {byte} bit {bit} was not detected: {result:?}"
                );
            }
        }
    }

    #[test]
    fn a_version_from_the_future_is_refused_rather_than_guessed_at() {
        let mut h = FileHeader::new(FileKind::Manifest, 0);
        h.version = FORMAT_VERSION + 1;
        // Re-checksum so the version is the only thing wrong.
        let bytes = h.encode();
        match FileHeader::decode_any(&bytes) {
            Err(FormatError::UnsupportedVersion {
                found,
                min_readable,
                current,
            }) => {
                assert_eq!(found, FORMAT_VERSION + 1);
                assert_eq!(min_readable, MIN_READABLE_VERSION);
                assert_eq!(current, FORMAT_VERSION);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }
    }

    #[test]
    fn a_version_below_the_readable_floor_is_refused() {
        let mut h = FileHeader::new(FileKind::Manifest, 0);
        h.version = MIN_READABLE_VERSION.saturating_sub(1);
        if h.version < MIN_READABLE_VERSION {
            assert!(matches!(
                FileHeader::decode_any(&h.encode()),
                Err(FormatError::UnsupportedVersion { .. })
            ));
        }
    }

    #[test]
    fn unknown_flag_bits_are_refused() {
        let mut h = FileHeader::new(FileKind::Manifest, 0);
        h.flags = HeaderFlags(0b1000_0000);
        assert!(matches!(
            FileHeader::decode_any(&h.encode()),
            Err(FormatError::Malformed {
                kind: MalformedKind::UnknownFlags(_),
                ..
            })
        ));
    }

    #[test]
    fn a_header_len_below_the_minimum_is_refused() {
        let mut h = FileHeader::new(FileKind::Manifest, 0);
        h.header_len = 8;
        assert!(matches!(
            FileHeader::decode_any(&h.encode()),
            Err(FormatError::Malformed {
                kind: MalformedKind::HeaderTooShort,
                ..
            })
        ));
    }

    /// Reserved bytes are only reserved if readers enforce them.
    #[test]
    fn a_dirty_reserved_field_is_refused() {
        let mut bytes = FileHeader::new(FileKind::Manifest, 0).encode();
        bytes[30] = 0x01; // outside the CRC-covered region, so only the explicit check catches it
        assert!(matches!(
            FileHeader::decode_any(&bytes),
            Err(FormatError::Malformed {
                kind: MalformedKind::ReservedNotZero,
                ..
            })
        ));
    }

    #[test]
    fn truncation_at_every_length_is_an_error_and_never_a_panic() {
        let full = FileHeader::new(FileKind::Metadata, 7).encode();
        for len in 0..full.len() {
            let result = FileHeader::decode_any(&full[..len]);
            assert!(result.is_err(), "a {len}-byte header decoded successfully");
        }
        assert!(FileHeader::decode_any(&full).is_ok());
    }

    #[test]
    fn check_file_len_catches_a_short_file() {
        let h = FileHeader::new(FileKind::Vectors, 1000);
        assert!(h.check_file_len(1032).is_ok());
        assert!(
            h.check_file_len(2000).is_ok(),
            "a longer file is fine; the header bounds it"
        );
        match h.check_file_len(500) {
            Err(FormatError::Truncated {
                needed, available, ..
            }) => {
                assert_eq!(needed, 1032);
                assert_eq!(available, 500);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    /// Regression: `saturating_add` here made `header_len + u64::MAX` fit inside `u64::MAX`,
    /// so a header claiming an impossible payload was accepted.
    #[test]
    fn a_gigantic_payload_len_is_malformed_rather_than_saturating() {
        let h = FileHeader::new(FileKind::Vectors, u64::MAX);
        match h.check_file_len(u64::MAX) {
            Err(FormatError::Malformed {
                kind:
                    MalformedKind::Inconsistent {
                        field: "payload_len",
                    },
                ..
            }) => {}
            other => panic!("expected Inconsistent, got {other:?}"),
        }
        // One below the overflow point is merely truncated, not malformed.
        let h = FileHeader::new(FileKind::Vectors, u64::MAX - u64::from(HEADER_LEN));
        assert!(matches!(
            h.check_file_len(64),
            Err(FormatError::Truncated { .. })
        ));
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0xC0FF_EE00_1234_5678u64;
        for _ in 0..5000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 40) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            let _ = FileHeader::decode_any(&bytes);
            let _ = FileHeader::decode(&bytes, FileKind::Manifest);
        }
    }
}
