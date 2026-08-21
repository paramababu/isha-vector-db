//! LEB128 variable-length integers.
//!
//! Used for lengths and ids inside records, where most values are small and a fixed 8 bytes
//! would be mostly zeroes. Every decode is bounded and returns how many bytes it consumed, so a
//! corrupt record cannot walk a reader off the end of a buffer.

/// The most bytes a `u64` can occupy: 64 bits at 7 bits per byte.
pub const MAX_LEN_U64: usize = 10;

/// How a varint decode failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VarintError {
    /// The buffer ended mid-value.
    Truncated,
    /// More than [`MAX_LEN_U64`] continuation bytes, or a value that does not fit in 64 bits.
    Overflow,
    /// The value was encoded in more bytes than necessary.
    ///
    /// Rejected because the on-disk format must be canonical: two encodings of the same value
    /// would mean the same logical database could produce two different checksums.
    NonCanonical,
}

/// Bytes `value` will occupy when encoded.
pub const fn encoded_len(value: u64) -> usize {
    let mut len = 1;
    let mut v = value >> 7;
    while v != 0 {
        len += 1;
        v >>= 7;
    }
    len
}

/// Append `value` to `out`, returning how many bytes were written.
pub fn encode_u64(value: u64, out: &mut Vec<u8>) -> usize {
    let mut v = value;
    let mut written = 0;
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        written += 1;
        if v == 0 {
            return written;
        }
    }
}

/// Decode a `u64`, returning the value and the number of bytes consumed.
///
/// # Errors
/// [`VarintError`] if the buffer ends mid-value, the value overflows 64 bits, or the encoding
/// is not canonical.
pub fn decode_u64(buf: &[u8]) -> Result<(u64, usize), VarintError> {
    let mut value: u64 = 0;
    let mut shift = 0u32;
    for (i, &byte) in buf.iter().enumerate() {
        if i >= MAX_LEN_U64 {
            return Err(VarintError::Overflow);
        }
        let payload = (byte & 0x7F) as u64;
        // The tenth byte may only carry the single remaining bit.
        if shift == 63 && payload > 1 {
            return Err(VarintError::Overflow);
        }
        value |= payload << shift;
        if byte & 0x80 == 0 {
            // Canonical form: a multi-byte encoding must not end in a zero payload byte,
            // and the length must be the shortest that fits.
            if i > 0 && payload == 0 {
                return Err(VarintError::NonCanonical);
            }
            if encoded_len(value) != i + 1 {
                return Err(VarintError::NonCanonical);
            }
            return Ok((value, i + 1));
        }
        shift += 7;
    }
    Err(VarintError::Truncated)
}

/// Zigzag-encode a signed value so small negatives stay small.
pub const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

/// Reverse [`zigzag`].
pub const fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Append a signed value.
pub fn encode_i64(value: i64, out: &mut Vec<u8>) -> usize {
    encode_u64(zigzag(value), out)
}

/// Decode a signed value.
///
/// # Errors
/// As [`decode_u64`].
pub fn decode_i64(buf: &[u8]) -> Result<(i64, usize), VarintError> {
    let (v, n) = decode_u64(buf)?;
    Ok((unzigzag(v), n))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: u64) {
        let mut buf = Vec::new();
        let written = encode_u64(v, &mut buf);
        assert_eq!(written, buf.len());
        assert_eq!(written, encoded_len(v), "encoded_len disagrees for {v}");
        let (decoded, consumed) = decode_u64(&buf).unwrap();
        assert_eq!(decoded, v);
        assert_eq!(consumed, written);
    }

    #[test]
    fn round_trips_boundaries() {
        for v in [
            0,
            1,
            127,
            128,
            255,
            256,
            16_383,
            16_384,
            u32::MAX as u64,
            u64::MAX / 2,
            u64::MAX - 1,
            u64::MAX,
        ] {
            roundtrip(v);
        }
    }

    #[test]
    fn round_trips_every_shift_boundary() {
        for bit in 0..64 {
            let v = 1u64 << bit;
            roundtrip(v);
            roundtrip(v.wrapping_sub(1));
        }
    }

    #[test]
    fn max_value_uses_ten_bytes() {
        assert_eq!(encoded_len(u64::MAX), MAX_LEN_U64);
        let mut buf = Vec::new();
        encode_u64(u64::MAX, &mut buf);
        assert_eq!(buf.len(), MAX_LEN_U64);
    }

    #[test]
    fn signed_round_trips() {
        for v in [
            0i64,
            -1,
            1,
            -2,
            2,
            i64::MIN,
            i64::MAX,
            -1_000_000,
            1_000_000,
        ] {
            let mut buf = Vec::new();
            encode_i64(v, &mut buf);
            let (decoded, _) = decode_i64(&buf).unwrap();
            assert_eq!(decoded, v);
        }
    }

    #[test]
    fn zigzag_keeps_small_negatives_small() {
        assert_eq!(encoded_len(zigzag(-1)), 1);
        assert_eq!(encoded_len(zigzag(-64)), 1);
        assert_eq!(encoded_len(zigzag(63)), 1);
    }

    // ---- the failure paths, which are the whole point of bounding the decoder ----

    #[test]
    fn rejects_truncation() {
        assert_eq!(decode_u64(&[]), Err(VarintError::Truncated));
        assert_eq!(decode_u64(&[0x80]), Err(VarintError::Truncated));
        assert_eq!(decode_u64(&[0x80, 0x80, 0x80]), Err(VarintError::Truncated));
    }

    #[test]
    fn rejects_overflow() {
        // Eleven continuation bytes can never be valid.
        let buf = [0x80u8; 11];
        assert_eq!(decode_u64(&buf), Err(VarintError::Overflow));
        // Ten bytes whose final byte carries more than one bit overflows 64 bits.
        let mut buf = vec![0x80u8; 9];
        buf.push(0x02);
        assert_eq!(decode_u64(&buf), Err(VarintError::Overflow));
    }

    #[test]
    fn rejects_non_canonical_encodings() {
        // 0 encoded in two bytes.
        assert_eq!(decode_u64(&[0x80, 0x00]), Err(VarintError::NonCanonical));
        // 1 encoded in three bytes.
        assert_eq!(
            decode_u64(&[0x81, 0x80, 0x00]),
            Err(VarintError::NonCanonical)
        );
    }

    #[test]
    fn stops_at_the_value_and_reports_what_it_consumed() {
        let mut buf = Vec::new();
        encode_u64(300, &mut buf);
        buf.extend_from_slice(b"trailing data");
        let (v, n) = decode_u64(&buf).unwrap();
        assert_eq!(v, 300);
        assert_eq!(n, 2);
        assert_eq!(&buf[n..], b"trailing data");
    }

    /// Exhaustive over a wide range: no input in this space decodes to the wrong value.
    #[test]
    fn exhaustive_small_range() {
        for v in 0u64..100_000 {
            let mut buf = Vec::new();
            encode_u64(v, &mut buf);
            assert_eq!(decode_u64(&buf).unwrap().0, v);
        }
    }
}
