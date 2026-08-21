//! Bounded reading and writing over byte slices.
//!
//! Every decoder in this crate goes through [`Reader`], and that is the whole point: bounds
//! checking happens in one place that is small enough to audit and heavily enough tested to
//! trust, rather than being re-derived at each of a hundred call sites where one of them will
//! eventually be wrong.
//!
//! [`Reader::bytes`] is the load-bearing method. A length prefix in a corrupt file is arbitrary
//! attacker-controlled data; `bytes` refuses to hand back — or let a caller allocate — more than
//! the input actually holds.

use crate::error::{FormatError, MalformedKind, Result};
use crate::varint;

/// A cursor that reads little-endian values and never runs past its input.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// Offset of `buf[0]` within the file, so errors report file offsets rather than
    /// slice-relative ones. A corruption report that names the wrong offset wastes an hour.
    base: u64,
}

impl<'a> Reader<'a> {
    /// A reader over `buf`, whose first byte is at offset 0 of the file.
    pub fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            base: 0,
        }
    }

    /// A reader over `buf`, whose first byte is at `base` within the file.
    pub fn with_base(buf: &'a [u8], base: u64) -> Self {
        Self { buf, pos: 0, base }
    }

    /// Current offset within the file.
    pub fn offset(&self) -> u64 {
        self.base.saturating_add(self.pos as u64)
    }

    /// Bytes left.
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// Whether every byte has been consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Fail unless the input has been fully consumed.
    ///
    /// Structures that own their whole slice call this, so trailing junk is caught rather than
    /// ignored — an ignored tail is where a format divergence hides.
    ///
    /// # Errors
    /// [`MalformedKind::Inconsistent`] if bytes remain.
    pub fn expect_end(&self, field: &'static str) -> Result<()> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(FormatError::Malformed {
                offset: self.offset(),
                kind: MalformedKind::Inconsistent { field },
            })
        }
    }

    /// Take exactly `n` bytes.
    ///
    /// The one place a length from the input becomes a memory range. Nothing is allocated and
    /// nothing is returned unless `n` bytes genuinely exist.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] if fewer than `n` bytes remain.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let start = self.pos;
        let end = start.checked_add(n).ok_or(FormatError::Truncated {
            offset: self.offset(),
            needed: n as u64,
            available: self.remaining() as u64,
        })?;
        let slice = self.buf.get(start..end).ok_or(FormatError::Truncated {
            offset: self.offset(),
            needed: n as u64,
            available: self.remaining() as u64,
        })?;
        self.pos = end;
        Ok(slice)
    }

    /// Take `n` bytes where `n` came from the input itself.
    ///
    /// Identical to [`Reader::bytes`] except for the error it raises: a length prefix that
    /// exceeds the input is an internally inconsistent file, which is a different diagnosis
    /// from a file that was merely cut short mid-write.
    ///
    /// # Errors
    /// [`FormatError::LengthExceedsInput`] if the claimed length is not available.
    pub fn bytes_claimed(&mut self, claimed: u64, at: u64) -> Result<&'a [u8]> {
        let available = self.remaining() as u64;
        let n = usize::try_from(claimed).map_err(|_| FormatError::LengthExceedsInput {
            offset: at,
            claimed,
            available,
        })?;
        if n > self.remaining() {
            return Err(FormatError::LengthExceedsInput {
                offset: at,
                claimed,
                available,
            });
        }
        self.bytes(n)
    }

    /// Read a fixed-size array.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] if too few bytes remain.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self.bytes(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    /// Read one byte.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] at end of input.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }

    /// Read a little-endian `u16`.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] at end of input.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// Read a little-endian `u32`.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] at end of input.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// Read a little-endian `u64`.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] at end of input.
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Read a little-endian `f32`.
    ///
    /// Non-finite values are rejected: NaN and infinities are not storable, and letting one in
    /// here would poison every distance computed against that row.
    ///
    /// # Errors
    /// [`MalformedKind::NonFiniteFloat`] for NaN or infinity.
    pub fn f32(&mut self) -> Result<f32> {
        let at = self.offset();
        let v = f32::from_le_bytes(self.array()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(FormatError::Malformed {
                offset: at,
                kind: MalformedKind::NonFiniteFloat,
            })
        }
    }

    /// Read a little-endian `f64`, rejecting non-finite values.
    ///
    /// # Errors
    /// [`MalformedKind::NonFiniteFloat`] for NaN or infinity.
    pub fn f64(&mut self) -> Result<f64> {
        let at = self.offset();
        let v = f64::from_le_bytes(self.array()?);
        if v.is_finite() {
            Ok(v)
        } else {
            Err(FormatError::Malformed {
                offset: at,
                kind: MalformedKind::NonFiniteFloat,
            })
        }
    }

    /// Read a canonical LEB128 `u64`.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] at end of input, or [`MalformedKind::NonCanonicalVarint`] for
    /// an over-long encoding.
    pub fn varint(&mut self) -> Result<u64> {
        let at = self.offset();
        let rest = self.buf.get(self.pos..).unwrap_or(&[]);
        match varint::decode_u64(rest) {
            Ok((v, n)) => {
                self.pos = self.pos.saturating_add(n);
                Ok(v)
            }
            Err(varint::VarintError::Truncated) => Err(FormatError::Truncated {
                offset: at,
                needed: 1,
                available: self.remaining() as u64,
            }),
            Err(_) => Err(FormatError::Malformed {
                offset: at,
                kind: MalformedKind::NonCanonicalVarint,
            }),
        }
    }

    /// Read a zigzag LEB128 `i64`.
    ///
    /// # Errors
    /// As [`Reader::varint`].
    pub fn varint_i64(&mut self) -> Result<i64> {
        Ok(varint::unzigzag(self.varint()?))
    }

    /// Read a varint-length-prefixed byte string.
    ///
    /// # Errors
    /// [`FormatError::LengthExceedsInput`] if the prefix exceeds the input.
    pub fn blob(&mut self) -> Result<&'a [u8]> {
        let at = self.offset();
        let len = self.varint()?;
        self.bytes_claimed(len, at)
    }

    /// Read a varint-length-prefixed UTF-8 string.
    ///
    /// # Errors
    /// As [`Reader::blob`], plus [`MalformedKind::InvalidUtf8`].
    pub fn string(&mut self) -> Result<&'a str> {
        let at = self.offset();
        let bytes = self.blob()?;
        core::str::from_utf8(bytes).map_err(|_| FormatError::Malformed {
            offset: at,
            kind: MalformedKind::InvalidUtf8,
        })
    }

    /// Read a reserved region and require it to be zero.
    ///
    /// Reserved space is only reserved if readers enforce it; a decoder that ignores it lets
    /// junk accumulate there and the space becomes unusable for the additive change it was set
    /// aside for.
    ///
    /// # Errors
    /// [`MalformedKind::ReservedNotZero`] if any byte is set.
    pub fn reserved(&mut self, n: usize) -> Result<()> {
        let at = self.offset();
        let bytes = self.bytes(n)?;
        if bytes.iter().any(|&b| b != 0) {
            return Err(FormatError::Malformed {
                offset: at,
                kind: MalformedKind::ReservedNotZero,
            });
        }
        Ok(())
    }

    /// Look at the next `n` bytes without consuming them.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] if too few bytes remain.
    pub fn peek(&self, n: usize) -> Result<&'a [u8]> {
        self.buf
            .get(self.pos..self.pos.saturating_add(n))
            .ok_or(FormatError::Truncated {
                offset: self.offset(),
                needed: n as u64,
                available: self.remaining() as u64,
            })
    }

    /// Skip `n` bytes.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] if too few bytes remain.
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    /// A sub-reader over the next `n` bytes, with file offsets preserved.
    ///
    /// Used for length-delimited sections so an inner decoder physically cannot read past its
    /// own region into the next one.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] if too few bytes remain.
    pub fn sub(&mut self, n: usize) -> Result<Reader<'a>> {
        let base = self.offset();
        let slice = self.bytes(n)?;
        Ok(Reader::with_base(slice, base))
    }
}

/// A growable little-endian byte sink, mirroring [`Reader`].
#[derive(Debug, Default, Clone)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// An empty writer.
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// An empty writer with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consume the writer, yielding its bytes.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// Append raw bytes.
    pub fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(bytes);
        self
    }

    /// Append one byte.
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    /// Append a little-endian `u16`.
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.raw(&v.to_le_bytes())
    }

    /// Append a little-endian `u32`.
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.raw(&v.to_le_bytes())
    }

    /// Append a little-endian `u64`.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.raw(&v.to_le_bytes())
    }

    /// Append a little-endian `f32`.
    pub fn f32(&mut self, v: f32) -> &mut Self {
        self.raw(&v.to_le_bytes())
    }

    /// Append a little-endian `f64`.
    pub fn f64(&mut self, v: f64) -> &mut Self {
        self.raw(&v.to_le_bytes())
    }

    /// Append a canonical LEB128 `u64`.
    pub fn varint(&mut self, v: u64) -> &mut Self {
        varint::encode_u64(v, &mut self.buf);
        self
    }

    /// Append a zigzag LEB128 `i64`.
    pub fn varint_i64(&mut self, v: i64) -> &mut Self {
        varint::encode_i64(v, &mut self.buf);
        self
    }

    /// Append a varint-length-prefixed byte string.
    pub fn blob(&mut self, bytes: &[u8]) -> &mut Self {
        self.varint(bytes.len() as u64).raw(bytes)
    }

    /// Append a varint-length-prefixed UTF-8 string.
    pub fn string(&mut self, s: &str) -> &mut Self {
        self.blob(s.as_bytes())
    }

    /// Append `n` zero bytes of reserved space.
    pub fn reserved(&mut self, n: usize) -> &mut Self {
        self.buf.resize(self.buf.len().saturating_add(n), 0);
        self
    }

    /// Pad with zeroes until the length is a multiple of `align`.
    ///
    /// The vector block is padded to a 64-byte boundary so a scan reads cache-line-aligned
    /// floats.
    pub fn align_to(&mut self, align: usize) -> &mut Self {
        if align > 1 {
            let rem = self.buf.len() % align;
            if rem != 0 {
                self.reserved(align - rem);
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_round_trip_in_order() {
        let mut w = Writer::new();
        w.u8(0xAB)
            .u16(0x1234)
            .u32(0xDEAD_BEEF)
            .u64(0x0123_4567_89AB_CDEF)
            .f32(1.5)
            .f64(-2.25)
            .varint(300)
            .varint_i64(-7)
            .string("hello")
            .blob(&[1, 2, 3]);
        let bytes = w.finish();

        let mut r = Reader::new(&bytes);
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert_eq!(r.u16().unwrap(), 0x1234);
        assert_eq!(r.u32().unwrap(), 0xDEAD_BEEF);
        assert_eq!(r.u64().unwrap(), 0x0123_4567_89AB_CDEF);
        assert_eq!(r.f32().unwrap(), 1.5);
        assert_eq!(r.f64().unwrap(), -2.25);
        assert_eq!(r.varint().unwrap(), 300);
        assert_eq!(r.varint_i64().unwrap(), -7);
        assert_eq!(r.string().unwrap(), "hello");
        assert_eq!(r.blob().unwrap(), &[1, 2, 3]);
        r.expect_end("test").unwrap();
    }

    #[test]
    fn everything_is_little_endian() {
        let mut w = Writer::new();
        w.u32(0x0102_0304);
        assert_eq!(w.as_slice(), &[0x04, 0x03, 0x02, 0x01]);
    }

    /// The security-critical property: a hostile length prefix cannot make us allocate or read
    /// past the buffer.
    #[test]
    fn a_huge_length_prefix_is_refused_not_allocated() {
        let mut w = Writer::new();
        w.varint(u64::MAX); // a length prefix claiming 18 exabytes
        w.raw(b"three");
        let bytes = w.finish();

        let mut r = Reader::new(&bytes);
        match r.blob() {
            Err(FormatError::LengthExceedsInput {
                claimed, available, ..
            }) => {
                assert_eq!(claimed, u64::MAX);
                assert_eq!(available, 5);
            }
            other => panic!("expected LengthExceedsInput, got {other:?}"),
        }
    }

    #[test]
    fn a_merely_optimistic_length_prefix_is_also_refused() {
        let mut w = Writer::new();
        w.varint(100);
        w.raw(b"only ten..");
        let bytes = w.finish();
        let mut r = Reader::new(&bytes);
        assert!(matches!(
            r.blob(),
            Err(FormatError::LengthExceedsInput { claimed: 100, .. })
        ));
    }

    #[test]
    fn reading_past_the_end_is_truncation_not_a_panic() {
        let mut r = Reader::new(&[1, 2, 3]);
        assert!(matches!(
            r.u64(),
            Err(FormatError::Truncated {
                needed: 8,
                available: 3,
                ..
            })
        ));
        assert!(matches!(r.bytes(4), Err(FormatError::Truncated { .. })));
        // The reader is unmoved by a failed read, so a caller can retry with a smaller request.
        assert_eq!(r.remaining(), 3);
        assert_eq!(r.u16().unwrap(), 0x0201);
    }

    #[test]
    fn empty_input_reads_fail_cleanly() {
        let mut r = Reader::new(&[]);
        assert!(r.u8().is_err());
        assert!(r.varint().is_err());
        assert!(r.blob().is_err());
        assert!(r.is_empty());
        r.expect_end("empty").unwrap();
    }

    #[test]
    fn offsets_are_file_relative() {
        let bytes = [0u8; 16];
        let mut r = Reader::with_base(&bytes, 4096);
        assert_eq!(r.offset(), 4096);
        r.u32().unwrap();
        assert_eq!(r.offset(), 4100);
        let err = r.bytes(1000).unwrap_err();
        assert!(
            matches!(err, FormatError::Truncated { offset: 4100, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn sub_readers_cannot_read_past_their_section() {
        let mut w = Writer::new();
        w.u32(0xAAAA_AAAA).u32(0xBBBB_BBBB);
        let bytes = w.finish();

        let mut r = Reader::new(&bytes);
        let mut inner = r.sub(4).unwrap();
        assert_eq!(inner.u32().unwrap(), 0xAAAA_AAAA);
        assert!(
            inner.u32().is_err(),
            "the sub-reader read into the next section"
        );
        // The outer reader is positioned after the section.
        assert_eq!(r.u32().unwrap(), 0xBBBB_BBBB);
    }

    #[test]
    fn reserved_space_must_be_zero() {
        let mut r = Reader::new(&[0, 0, 0, 0]);
        r.reserved(4).unwrap();

        let mut r = Reader::new(&[0, 0, 1, 0]);
        assert!(matches!(
            r.reserved(4),
            Err(FormatError::Malformed {
                kind: MalformedKind::ReservedNotZero,
                ..
            })
        ));
    }

    #[test]
    fn non_finite_floats_are_rejected() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut w = Writer::new();
            w.f32(bad);
            let bytes = w.finish();
            assert!(matches!(
                Reader::new(&bytes).f32(),
                Err(FormatError::Malformed {
                    kind: MalformedKind::NonFiniteFloat,
                    ..
                })
            ));
        }
        for bad in [f64::NAN, f64::INFINITY] {
            let mut w = Writer::new();
            w.f64(bad);
            let bytes = w.finish();
            assert!(Reader::new(&bytes).f64().is_err());
        }
        // Finite extremes must still pass.
        let mut w = Writer::new();
        w.f32(f32::MIN).f32(f32::MAX).f32(-0.0);
        let bytes = w.finish();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.f32().unwrap(), f32::MIN);
        assert_eq!(r.f32().unwrap(), f32::MAX);
        assert_eq!(r.f32().unwrap(), -0.0);
    }

    #[test]
    fn invalid_utf8_is_rejected() {
        let mut w = Writer::new();
        w.blob(&[0xFF, 0xFE]);
        let bytes = w.finish();
        assert!(matches!(
            Reader::new(&bytes).string(),
            Err(FormatError::Malformed {
                kind: MalformedKind::InvalidUtf8,
                ..
            })
        ));
    }

    #[test]
    fn expect_end_catches_trailing_junk() {
        let mut w = Writer::new();
        w.u32(1).raw(b"junk");
        let bytes = w.finish();
        let mut r = Reader::new(&bytes);
        r.u32().unwrap();
        assert!(r.expect_end("body").is_err());
    }

    #[test]
    fn align_to_pads_to_the_boundary() {
        let mut w = Writer::new();
        w.raw(&[1, 2, 3]).align_to(8);
        assert_eq!(w.len(), 8);
        assert_eq!(&w.as_slice()[3..], &[0, 0, 0, 0, 0]);
        w.align_to(8);
        assert_eq!(w.len(), 8, "already aligned, should not pad again");
        w.align_to(1);
        assert_eq!(w.len(), 8);
    }

    #[test]
    fn peek_does_not_consume() {
        let mut r = Reader::new(&[9, 8, 7]);
        assert_eq!(r.peek(2).unwrap(), &[9, 8]);
        assert_eq!(r.remaining(), 3);
        assert!(r.peek(4).is_err());
        r.skip(1).unwrap();
        assert_eq!(r.peek(2).unwrap(), &[8, 7]);
    }

    /// No byte sequence, however malformed, may make a decoder panic. This is the property the
    /// fuzz targets extend; keeping a cheap version in the unit tests catches regressions
    /// without waiting for the nightly fuzz run.
    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0x1234_5678_9ABC_DEF0u64;
        for _ in 0..2000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 64) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            let mut r = Reader::new(&bytes);
            let _ = r.u8();
            let _ = r.varint();
            let _ = r.blob();
            let _ = r.string();
            let _ = r.f32();
            let _ = r.f64();
            let _ = r.u64();
            let _ = r.sub(9);
        }
    }
}
