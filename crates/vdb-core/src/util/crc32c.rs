//! CRC-32C (Castagnoli), the checksum on every persisted block.
//!
//! Castagnoli rather than the zlib polynomial because it detects more burst-error patterns at
//! the block sizes we use, and because it is the polynomial modern CPUs implement in hardware
//! (SSE4.2 `crc32`, ARMv8 `crc32c`) — leaving the door open to a hardware-accelerated path in
//! `vdb-storage-os` later without changing a single byte on disk.
//!
//! This implementation is a portable, table-driven scalar one. It is deliberately in `vdb-core`,
//! where `unsafe` is forbidden, so the reference checksum can never be the thing that is wrong.

/// Reflected Castagnoli polynomial.
const POLY: u32 = 0x82F6_3B78;

/// Byte-at-a-time table, built at compile time so there is no lazy-init or `OnceLock`.
///
/// Indexing rather than `get` because slice methods are unavailable in a `const` block; the
/// loop bound is the array length, so the compiler proves this correct at build time.
#[allow(clippy::indexing_slicing)]
const TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
};

/// Checksum a single buffer.
///
/// ```
/// # use vdb_core::util::crc32c;
/// assert_eq!(crc32c(b"123456789"), 0xE306_9283);
/// ```
pub fn crc32c(data: &[u8]) -> u32 {
    Crc32c::new().update(data).finish()
}

/// Incremental CRC-32C, for checksumming data that arrives in pieces.
///
/// A frame's checksum covers its header and body, which are built separately; without an
/// incremental form every writer would have to concatenate them into a scratch buffer first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32c(u32);

impl Crc32c {
    /// Start a new checksum.
    pub const fn new() -> Self {
        Self(0xFFFF_FFFF)
    }

    /// Resume from a previously finished value, so a checksum can span several calls.
    pub const fn resume(previous: u32) -> Self {
        Self(!previous)
    }

    /// Feed bytes in.
    #[must_use]
    pub fn update(mut self, data: &[u8]) -> Self {
        let mut crc = self.0;
        for &b in data {
            let idx = ((crc ^ b as u32) & 0xFF) as usize;
            // The index is masked to 0..=255 and TABLE has 256 entries, so this always hits.
            // `get` rather than `[]` keeps the crate free of panicking indexing entirely: the
            // checksum is the last place we want a bounds check to be provably-but-not-visibly
            // safe.
            let entry = match TABLE.get(idx) {
                Some(e) => *e,
                None => 0,
            };
            crc = (crc >> 8) ^ entry;
        }
        self.0 = crc;
        self
    }

    /// The checksum of everything fed in so far.
    pub const fn finish(self) -> u32 {
        !self.0
    }
}

impl Default for Crc32c {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vectors from RFC 3720 appendix B (iSCSI CRC-32C). If these pass, the polynomial and the
    /// bit reflection are both right — which is the only way an on-disk checksum stays stable.
    #[test]
    fn rfc3720_vectors() {
        assert_eq!(crc32c(&[]), 0x0000_0000);
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);

        let ascending: Vec<u8> = (0u8..32).collect();
        assert_eq!(crc32c(&ascending), 0x46DD_794E);

        let descending: Vec<u8> = (0u8..32).rev().collect();
        assert_eq!(crc32c(&descending), 0x113F_DB5C);
    }

    #[test]
    fn check_string_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn incremental_matches_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let one_shot = crc32c(&data);
        for split in [0usize, 1, 7, 64, 255, 500, 999, 1000] {
            let (a, b) = data.split_at(split);
            let inc = Crc32c::new().update(a).update(b).finish();
            assert_eq!(inc, one_shot, "split at {split}");
        }
    }

    #[test]
    fn resume_continues_a_finished_checksum() {
        let a = b"header-bytes";
        let b = b"body-bytes";
        let combined = Crc32c::new().update(a).update(b).finish();
        let staged = Crc32c::resume(crc32c(a)).update(b).finish();
        assert_eq!(staged, combined);
    }

    /// The property that matters for corruption detection: any single-bit flip changes the CRC.
    #[test]
    fn detects_every_single_bit_flip() {
        let original = vec![0xA5u8; 64];
        let baseline = crc32c(&original);
        for byte in 0..original.len() {
            for bit in 0..8 {
                let mut mutated = original.clone();
                mutated[byte] ^= 1u8 << bit;
                assert_ne!(crc32c(&mutated), baseline, "flip at byte {byte} bit {bit}");
            }
        }
    }

    /// Truncation must not go unnoticed either — a torn write is the common real-world case.
    #[test]
    fn detects_truncation() {
        let data: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
        let full = crc32c(&data);
        for len in 0..data.len() {
            assert_ne!(crc32c(&data[..len]), full, "truncated to {len}");
        }
    }
}
