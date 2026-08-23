//! Checksummed blocks: `header || payload || crc32c(payload)`.
//!
//! The shape every small, whole-file structure uses — the manifest slots, the catalog, the
//! tombstone bitmap, index snapshots. Large files (the vector block) do not use it, because
//! verifying a whole-file checksum on every open would mean reading a 300 MB file to answer
//! "may I open this?"; those carry a footer checksum verified only by `verify(Checksums)`.
//!
//! Keeping this in one place means the ordering — write the header, write the payload, checksum
//! the payload, verify before interpreting — cannot drift between structures.

use crate::crc32c::crc32c;
use crate::error::{FormatError, Result};
use crate::header::{FileHeader, FileKind};
use crate::Writer;

/// Bytes of checksum appended after the payload.
pub const TRAILER_LEN: usize = 4;

/// Wrap `payload` in a header and a trailing checksum.
pub fn encode_block(kind: FileKind, payload: &[u8]) -> Vec<u8> {
    let mut w = Writer::with_capacity(payload.len() + 40);
    FileHeader::new(kind, payload.len() as u64).write_to(&mut w);
    w.raw(payload).u32(crc32c(payload));
    w.finish()
}

/// Locate the payload and its trailer within a block, validating the header and all bounds.
///
/// One place computes these offsets, so `open`, `decode` and `verify` cannot disagree about
/// where the payload ends — a disagreement that would show up as spurious checksum failures on
/// files that happen to have trailing bytes.
fn locate(bytes: &[u8], kind: FileKind) -> Result<(usize, usize)> {
    let header = FileHeader::decode(bytes, kind)?;
    let start = header.header_len as usize;
    let payload_len = usize::try_from(header.payload_len).map_err(|_| FormatError::Truncated {
        offset: u64::from(header.header_len),
        needed: header.payload_len,
        available: bytes.len() as u64,
    })?;
    let end = start
        .checked_add(payload_len)
        .and_then(|e| e.checked_add(TRAILER_LEN))
        .ok_or(FormatError::Truncated {
            offset: u64::from(header.header_len),
            needed: header.payload_len,
            available: bytes.len() as u64,
        })?;
    if bytes.len() < end {
        return Err(FormatError::Truncated {
            offset: u64::from(header.header_len),
            needed: end as u64,
            available: bytes.len() as u64,
        });
    }
    Ok((start, payload_len))
}

/// Return a block's payload **without** verifying its checksum.
///
/// Identical bytes on disk to [`decode_block`]; only the read policy differs. Large files — the
/// vector block, the metadata records — use this on open, because checksumming 300 MB to answer
/// "may I open this database?" would make startup unusable. Their checksums are verified by
/// [`verify_block`], which is what `verify(Checksums)` and the CLI run.
///
/// The header, its own checksum, and every length bound are still validated. Only the payload
/// checksum is skipped.
///
/// # Errors
/// [`FormatError::BadMagic`], [`FormatError::Truncated`] or [`FormatError::Malformed`].
pub fn open_block(bytes: &[u8], kind: FileKind) -> Result<&[u8]> {
    let (start, len) = locate(bytes, kind)?;
    bytes.get(start..start + len).ok_or(FormatError::Truncated {
        offset: start as u64,
        needed: (start + len) as u64,
        available: bytes.len() as u64,
    })
}

/// Verify a block and return its payload.
///
/// Order of checks is deliberate: the header validates itself first (so a garbage
/// `payload_len` cannot send us reading at a wild offset), then the payload length is checked
/// against the bytes actually present, then the payload checksum.
///
/// # Errors
/// As [`open_block`], plus [`FormatError::ChecksumMismatch`].
pub fn decode_block(bytes: &[u8], kind: FileKind) -> Result<&[u8]> {
    let (start, len) = locate(bytes, kind)?;
    check_payload(bytes, start, len)?;
    bytes.get(start..start + len).ok_or(FormatError::Truncated {
        offset: start as u64,
        needed: (start + len) as u64,
        available: bytes.len() as u64,
    })
}

/// Verify a block's payload checksum without returning the payload.
///
/// # Errors
/// As [`decode_block`].
pub fn verify_block(bytes: &[u8], kind: FileKind) -> Result<()> {
    let (start, len) = locate(bytes, kind)?;
    check_payload(bytes, start, len)
}

fn check_payload(bytes: &[u8], start: usize, len: usize) -> Result<()> {
    let payload = bytes
        .get(start..start + len)
        .ok_or(FormatError::Truncated {
            offset: start as u64,
            needed: (start + len) as u64,
            available: bytes.len() as u64,
        })?;
    let trailer_at = start + len;
    let stored = bytes
        .get(trailer_at..trailer_at + TRAILER_LEN)
        .and_then(|b| <[u8; 4]>::try_from(b).ok())
        .map(u32::from_le_bytes)
        .ok_or(FormatError::Truncated {
            offset: trailer_at as u64,
            needed: TRAILER_LEN as u64,
            available: bytes.len() as u64,
        })?;
    let computed = crc32c(payload);
    if computed != stored {
        return Err(FormatError::ChecksumMismatch {
            offset: start as u64,
            expected: stored,
            found: computed,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_any_payload() {
        for payload in [
            vec![],
            vec![0u8],
            (0..=255u8).collect::<Vec<_>>(),
            vec![7u8; 10_000],
        ] {
            let block = encode_block(FileKind::Catalog, &payload);
            assert_eq!(
                decode_block(&block, FileKind::Catalog).unwrap(),
                payload.as_slice()
            );
        }
    }

    #[test]
    fn the_wrong_kind_is_refused() {
        let block = encode_block(FileKind::Catalog, b"x");
        assert!(matches!(
            decode_block(&block, FileKind::Manifest),
            Err(FormatError::BadMagic { .. })
        ));
    }

    #[test]
    fn every_single_bit_flip_anywhere_is_detected() {
        let block = encode_block(FileKind::Deleted, b"the quick brown fox");
        for byte in 0..block.len() {
            for bit in 0..8 {
                let mut mutated = block.clone();
                mutated[byte] ^= 1u8 << bit;
                assert!(
                    decode_block(&mutated, FileKind::Deleted).is_err(),
                    "flip at byte {byte} bit {bit} slipped through"
                );
            }
        }
    }

    #[test]
    fn truncation_at_every_length_is_an_error() {
        let block = encode_block(FileKind::Index, b"payload bytes here");
        for len in 0..block.len() {
            assert!(
                decode_block(&block[..len], FileKind::Index).is_err(),
                "a {len}-byte block decoded"
            );
        }
    }

    #[test]
    fn trailing_bytes_after_the_trailer_are_ignored() {
        // A file may be longer than its block — the header bounds it. Recovery relies on this
        // when a torn write left junk after a complete structure.
        let mut block = encode_block(FileKind::Catalog, b"abc");
        block.extend_from_slice(b"junk after the block");
        assert_eq!(decode_block(&block, FileKind::Catalog).unwrap(), b"abc");
    }

    #[test]
    fn a_payload_len_larger_than_the_file_is_refused() {
        let mut block = encode_block(FileKind::Catalog, b"abc");
        // Rewrite payload_len to something enormous and fix the header checksum.
        let mut header = FileHeader::decode_any(&block).unwrap();
        header.payload_len = u64::MAX / 2;
        block[..32].copy_from_slice(&header.encode());
        assert!(matches!(
            decode_block(&block, FileKind::Catalog),
            Err(FormatError::Truncated { .. })
        ));
    }

    /// The large-file policy: `open_block` must accept a payload whose checksum is wrong,
    /// while `verify_block` must reject it. Otherwise opening a database would mean reading
    /// every byte of every vector file.
    #[test]
    fn open_skips_the_payload_checksum_and_verify_does_not() {
        let mut block = encode_block(FileKind::Vectors, &[1, 2, 3, 4, 5, 6, 7, 8]);
        block[35] ^= 0xFF; // corrupt the payload, leave the header intact

        assert!(open_block(&block, FileKind::Vectors).is_ok());
        assert!(matches!(
            verify_block(&block, FileKind::Vectors),
            Err(FormatError::ChecksumMismatch { .. })
        ));
        assert!(decode_block(&block, FileKind::Vectors).is_err());
    }

    /// Regression: computing the trailer's position from the end of the buffer instead of from
    /// `header_len + payload_len` made any file with trailing bytes fail its checksum.
    #[test]
    fn a_block_with_trailing_bytes_still_verifies() {
        let mut block = encode_block(FileKind::Catalog, b"payload");
        block.extend_from_slice(b"trailing junk from a torn write");
        assert!(verify_block(&block, FileKind::Catalog).is_ok());
        assert_eq!(decode_block(&block, FileKind::Catalog).unwrap(), b"payload");
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0x5EED_5EED_5EED_5EEDu64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 80) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            for kind in FileKind::ALL {
                let _ = decode_block(&bytes, kind);
                let _ = open_block(&bytes, kind);
                let _ = verify_block(&bytes, kind);
            }
        }
    }
}
