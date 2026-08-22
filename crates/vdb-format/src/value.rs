//! The metadata value model and its canonical encoding.
//!
//! This is the persisted shape of user metadata, so it lives with the format rather than with
//! the engine. `vdb-core` re-exports it as part of its public API.
//!
//! # Canonical encoding
//!
//! One logical value has exactly one byte representation. Map keys are sorted, varints are
//! minimally encoded, and there is no redundant framing. This matters for three reasons:
//! checksums over metadata are reproducible, golden fixtures are stable, and compaction can be
//! verified by comparing bytes rather than by re-parsing.
//!
//! Decoders enforce canonicality rather than merely tolerating it: unsorted or duplicated map
//! keys are an error, not something to be quietly accepted.
//!
//! # Depth
//!
//! Decoding is recursive, so hostile input could otherwise nest deeply enough to overflow the
//! stack — and a stack overflow aborts the process rather than returning an error a host app
//! could handle. Depth is therefore checked on the way *down*, before recursing, and capped at
//! [`MAX_VALUE_DEPTH`].
//!
//! ```text
//! tag  type      payload
//!   0  Null      —
//!   1  Bool      false
//!   2  Bool      true            (the value is in the tag: booleans cost one byte)
//!   3  I64       zigzag varint
//!   4  F64       8 bytes LE
//!   5  Str       varint len + UTF-8
//!   6  Bytes     varint len + raw
//!   7  Array     varint count + values
//!   8  Map       varint count + (string, value) pairs, keys strictly ascending
//! ```

use std::collections::BTreeMap;

use crate::cursor::{Reader, Writer};
use crate::error::{FormatError, MalformedKind, Result};

/// Deepest permitted nesting of arrays and maps.
pub const MAX_VALUE_DEPTH: usize = 16;

mod tag {
    pub(crate) const NULL: u8 = 0;
    pub(crate) const FALSE: u8 = 1;
    pub(crate) const TRUE: u8 = 2;
    pub(crate) const I64: u8 = 3;
    pub(crate) const F64: u8 = 4;
    pub(crate) const STR: u8 = 5;
    pub(crate) const BYTES: u8 = 6;
    pub(crate) const ARRAY: u8 = 7;
    pub(crate) const MAP: u8 = 8;
}

/// A metadata value.
///
/// `BTreeMap` rather than `HashMap` so iteration order — and therefore the encoded bytes — is a
/// deterministic function of the logical value.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    /// Absent or explicitly null.
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    I64(i64),
    /// A finite 64-bit float. NaN and infinities are not storable.
    F64(f64),
    /// A UTF-8 string.
    Str(String),
    /// Opaque bytes.
    Bytes(Vec<u8>),
    /// An ordered list.
    Array(Vec<Value>),
    /// A string-keyed map, kept sorted.
    Map(BTreeMap<String, Value>),
}

impl Value {
    /// A short, stable name for the variant, for error messages and filter type diagnostics.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::I64(_) => "i64",
            Self::F64(_) => "f64",
            Self::Str(_) => "string",
            Self::Bytes(_) => "bytes",
            Self::Array(_) => "array",
            Self::Map(_) => "map",
        }
    }

    /// How deeply this value nests. A scalar is depth 1.
    pub fn depth(&self) -> usize {
        match self {
            Self::Array(items) => 1 + items.iter().map(Self::depth).max().unwrap_or(0),
            Self::Map(entries) => 1 + entries.values().map(Self::depth).max().unwrap_or(0),
            _ => 1,
        }
    }

    /// Encode to bytes.
    ///
    /// # Errors
    /// [`MalformedKind::NonFiniteFloat`] for a NaN or infinite `F64`, and
    /// [`MalformedKind::DepthExceeded`] beyond [`MAX_VALUE_DEPTH`]. Encoding is fallible so
    /// that encode and decode are exact inverses: anything this accepts, the decoder accepts.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        self.write_to(&mut w)?;
        Ok(w.finish())
    }

    /// Append this value to a writer.
    ///
    /// # Errors
    /// As [`Value::encode`].
    pub fn write_to(&self, w: &mut Writer) -> Result<()> {
        self.write_at_depth(w, 1)
    }

    fn write_at_depth(&self, w: &mut Writer, depth: usize) -> Result<()> {
        if depth > MAX_VALUE_DEPTH {
            return Err(FormatError::Malformed {
                offset: w.len() as u64,
                kind: MalformedKind::DepthExceeded {
                    max: MAX_VALUE_DEPTH,
                },
            });
        }
        match self {
            Self::Null => {
                w.u8(tag::NULL);
            }
            Self::Bool(false) => {
                w.u8(tag::FALSE);
            }
            Self::Bool(true) => {
                w.u8(tag::TRUE);
            }
            Self::I64(v) => {
                w.u8(tag::I64).varint_i64(*v);
            }
            Self::F64(v) => {
                if !v.is_finite() {
                    return Err(FormatError::Malformed {
                        offset: w.len() as u64,
                        kind: MalformedKind::NonFiniteFloat,
                    });
                }
                w.u8(tag::F64).f64(*v);
            }
            Self::Str(s) => {
                w.u8(tag::STR).string(s);
            }
            Self::Bytes(b) => {
                w.u8(tag::BYTES).blob(b);
            }
            Self::Array(items) => {
                w.u8(tag::ARRAY).varint(items.len() as u64);
                for item in items {
                    item.write_at_depth(w, depth + 1)?;
                }
            }
            Self::Map(entries) => {
                w.u8(tag::MAP).varint(entries.len() as u64);
                // BTreeMap iterates in key order, which is what makes the encoding canonical.
                for (key, value) in entries {
                    w.string(key);
                    value.write_at_depth(w, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    /// Decode a value from bytes, requiring the whole input to be consumed.
    ///
    /// # Errors
    /// Any [`FormatError`]; in particular [`MalformedKind::UnknownValueTag`],
    /// [`MalformedKind::KeysNotSorted`], [`MalformedKind::DuplicateKey`] and
    /// [`MalformedKind::DepthExceeded`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let v = Self::read_from(&mut r)?;
        r.expect_end("value")?;
        Ok(v)
    }

    /// Read one value from a reader, leaving anything after it.
    ///
    /// # Errors
    /// As [`Value::decode`].
    pub fn read_from(r: &mut Reader<'_>) -> Result<Self> {
        Self::read_at_depth(r, 1)
    }

    fn read_at_depth(r: &mut Reader<'_>, depth: usize) -> Result<Self> {
        // Checked before recursing, not after: the point is to never make the recursive call.
        if depth > MAX_VALUE_DEPTH {
            return Err(FormatError::Malformed {
                offset: r.offset(),
                kind: MalformedKind::DepthExceeded {
                    max: MAX_VALUE_DEPTH,
                },
            });
        }
        let at = r.offset();
        let t = r.u8()?;
        Ok(match t {
            tag::NULL => Self::Null,
            tag::FALSE => Self::Bool(false),
            tag::TRUE => Self::Bool(true),
            tag::I64 => Self::I64(r.varint_i64()?),
            tag::F64 => Self::F64(r.f64()?),
            tag::STR => Self::Str(r.string()?.to_owned()),
            tag::BYTES => Self::Bytes(r.blob()?.to_vec()),
            tag::ARRAY => {
                let count_at = r.offset();
                let count = r.varint()?;
                // One byte per element is the theoretical floor (a Null), so a count larger
                // than the bytes remaining cannot be honest. Checking here is what stops a
                // four-byte file from asking for a billion-element allocation.
                let count = bounded_count(count, r.remaining(), count_at)?;
                let mut items = Vec::with_capacity(count);
                for _ in 0..count {
                    items.push(Self::read_at_depth(r, depth + 1)?);
                }
                Self::Array(items)
            }
            tag::MAP => {
                let count_at = r.offset();
                let count = r.varint()?;
                // A pair is at minimum a one-byte key length, zero key bytes and a one-byte
                // value tag: two bytes.
                let count = bounded_count(count, r.remaining() / 2, count_at)?;
                let mut entries = BTreeMap::new();
                let mut previous: Option<String> = None;
                for _ in 0..count {
                    let key_at = r.offset();
                    let key = r.string()?.to_owned();
                    if let Some(prev) = &previous {
                        match key.as_str().cmp(prev.as_str()) {
                            core::cmp::Ordering::Greater => {}
                            core::cmp::Ordering::Equal => {
                                return Err(FormatError::Malformed {
                                    offset: key_at,
                                    kind: MalformedKind::DuplicateKey,
                                })
                            }
                            core::cmp::Ordering::Less => {
                                return Err(FormatError::Malformed {
                                    offset: key_at,
                                    kind: MalformedKind::KeysNotSorted,
                                })
                            }
                        }
                    }
                    let value = Self::read_at_depth(r, depth + 1)?;
                    previous = Some(key.clone());
                    entries.insert(key, value);
                }
                Self::Map(entries)
            }
            other => {
                return Err(FormatError::Malformed {
                    offset: at,
                    kind: MalformedKind::UnknownValueTag(other),
                })
            }
        })
    }
}

/// Walk past one encoded value without materialising it.
///
/// The point of the whole lazy-lookup path: skipping a 200-byte string costs a bounds-checked
/// pointer advance, where decoding it costs an allocation and a copy. A filter that reads one
/// field out of six should pay for one.
///
/// # Errors
/// Any [`FormatError`], with the same strictness as a full decode — a value that cannot be
/// skipped is one that cannot be trusted.
pub fn skip_value(r: &mut Reader<'_>) -> Result<()> {
    skip_at_depth(r, 1)
}

fn skip_at_depth(r: &mut Reader<'_>, depth: usize) -> Result<()> {
    if depth > MAX_VALUE_DEPTH {
        return Err(FormatError::Malformed {
            offset: r.offset(),
            kind: MalformedKind::DepthExceeded {
                max: MAX_VALUE_DEPTH,
            },
        });
    }
    let at = r.offset();
    let t = r.u8()?;
    match t {
        tag::NULL | tag::FALSE | tag::TRUE => {}
        tag::I64 => {
            r.varint()?;
        }
        tag::F64 => {
            r.skip(8)?;
        }
        tag::STR | tag::BYTES => {
            r.blob()?;
        }
        tag::ARRAY => {
            let count_at = r.offset();
            let count = bounded_count(r.varint()?, r.remaining(), count_at)?;
            for _ in 0..count {
                skip_at_depth(r, depth + 1)?;
            }
        }
        tag::MAP => {
            let count_at = r.offset();
            let count = bounded_count(r.varint()?, r.remaining() / 2, count_at)?;
            for _ in 0..count {
                r.blob()?;
                skip_at_depth(r, depth + 1)?;
            }
        }
        other => {
            return Err(FormatError::Malformed {
                offset: at,
                kind: MalformedKind::UnknownValueTag(other),
            })
        }
    }
    Ok(())
}

/// Decode only the value at a dotted path, skipping everything else.
///
/// `bytes` must be an encoded map. Returns `None` when the path does not resolve — a missing
/// key, or an attempt to descend into a scalar — which is the same total behaviour a full decode
/// followed by a lookup would give.
///
/// Map keys are stored in ascending order, so the scan stops as soon as it passes the key it is
/// looking for rather than reading to the end.
///
/// # Errors
/// Any [`FormatError`] from malformed input.
pub fn find_path(bytes: &[u8], path: &str) -> Result<Option<Value>> {
    if bytes.is_empty() || path.is_empty() {
        return Ok(None);
    }
    let mut r = Reader::new(bytes);
    let mut segments = path.split('.').peekable();

    loop {
        let Some(segment) = segments.next() else {
            return Ok(None);
        };
        let at = r.offset();
        if r.u8()? != tag::MAP {
            // Not a map, so there is nothing to descend into.
            let _ = at;
            return Ok(None);
        }
        let count_at = r.offset();
        let count = bounded_count(r.varint()?, r.remaining() / 2, count_at)?;

        let mut found = false;
        for _ in 0..count {
            let key = r.string()?;
            match key.cmp(segment) {
                core::cmp::Ordering::Less => skip_value(&mut r)?,
                core::cmp::Ordering::Equal => {
                    found = true;
                    break;
                }
                // Keys ascend, so once we are past the target it cannot appear later.
                core::cmp::Ordering::Greater => return Ok(None),
            }
        }
        if !found {
            return Ok(None);
        }
        if segments.peek().is_none() {
            return Value::read_from(&mut r).map(Some);
        }
        // More path to walk: the reader is now positioned at the nested value, which the next
        // iteration requires to be a map.
    }
}

/// Reject a container count that could not possibly be satisfied by the bytes left.
///
/// Without this, `Vec::with_capacity(count)` on an attacker-chosen count is an out-of-memory
/// crash from a handful of input bytes.
fn bounded_count(claimed: u64, max_possible: usize, at: u64) -> Result<usize> {
    let available = max_possible as u64;
    if claimed > available {
        return Err(FormatError::LengthExceedsInput {
            offset: at,
            claimed,
            available,
        });
    }
    usize::try_from(claimed).map_err(|_| FormatError::LengthExceedsInput {
        offset: at,
        claimed,
        available,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, Value)]) -> Value {
        Value::Map(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), v.clone()))
                .collect(),
        )
    }

    fn roundtrip(v: &Value) {
        let bytes = v.encode().unwrap();
        let decoded = Value::decode(&bytes).unwrap();
        assert_eq!(&decoded, v, "round-trip changed the value");
        // Canonical: re-encoding the decoded value reproduces the same bytes exactly.
        assert_eq!(
            decoded.encode().unwrap(),
            bytes,
            "encoding is not canonical"
        );
    }

    #[test]
    fn scalars_round_trip() {
        for v in [
            Value::Null,
            Value::Bool(true),
            Value::Bool(false),
            Value::I64(0),
            Value::I64(-1),
            Value::I64(i64::MIN),
            Value::I64(i64::MAX),
            Value::F64(0.0),
            Value::F64(-0.0),
            Value::F64(f64::MIN),
            Value::F64(f64::MAX),
            Value::F64(core::f64::consts::PI),
            Value::Str(String::new()),
            Value::Str("hello".into()),
            Value::Str("emoji 🧭 and ünïcödé".into()),
            Value::Bytes(vec![]),
            Value::Bytes(vec![0, 255, 128]),
        ] {
            roundtrip(&v);
        }
    }

    #[test]
    fn containers_round_trip() {
        roundtrip(&Value::Array(vec![]));
        roundtrip(&Value::Array(vec![
            Value::I64(1),
            Value::Str("two".into()),
            Value::Null,
        ]));
        roundtrip(&map(&[]));
        roundtrip(&map(&[
            ("category", Value::Str("tools".into())),
            ("in_stock", Value::Bool(true)),
            ("price", Value::F64(19.99)),
            (
                "tags",
                Value::Array(vec![Value::Str("a".into()), Value::Str("b".into())]),
            ),
        ]));
        roundtrip(&map(&[(
            "user",
            map(&[("plan", Value::Str("pro".into())), ("seats", Value::I64(5))]),
        )]));
    }

    #[test]
    fn booleans_cost_one_byte() {
        assert_eq!(Value::Bool(true).encode().unwrap().len(), 1);
        assert_eq!(Value::Null.encode().unwrap().len(), 1);
    }

    /// The canonicality property, stated directly: map insertion order must not reach the bytes.
    #[test]
    fn map_key_order_does_not_affect_the_encoding() {
        let mut a = BTreeMap::new();
        a.insert("zebra".to_owned(), Value::I64(1));
        a.insert("apple".to_owned(), Value::I64(2));

        let mut b = BTreeMap::new();
        b.insert("apple".to_owned(), Value::I64(2));
        b.insert("zebra".to_owned(), Value::I64(1));

        assert_eq!(
            Value::Map(a).encode().unwrap(),
            Value::Map(b).encode().unwrap()
        );
    }

    #[test]
    fn unsorted_map_keys_are_rejected() {
        // Hand-build a map whose keys descend.
        let mut w = Writer::new();
        w.u8(tag::MAP)
            .varint(2)
            .string("zebra")
            .u8(tag::NULL)
            .string("apple")
            .u8(tag::NULL);
        let bytes = w.finish();
        assert!(matches!(
            Value::decode(&bytes),
            Err(FormatError::Malformed {
                kind: MalformedKind::KeysNotSorted,
                ..
            })
        ));
    }

    #[test]
    fn duplicate_map_keys_are_rejected() {
        let mut w = Writer::new();
        w.u8(tag::MAP)
            .varint(2)
            .string("same")
            .u8(tag::NULL)
            .string("same")
            .u8(tag::NULL);
        let bytes = w.finish();
        assert!(matches!(
            Value::decode(&bytes),
            Err(FormatError::Malformed {
                kind: MalformedKind::DuplicateKey,
                ..
            })
        ));
    }

    #[test]
    fn an_unknown_tag_is_rejected_with_its_value() {
        for bad in [9u8, 42, 255] {
            match Value::decode(&[bad]) {
                Err(FormatError::Malformed {
                    kind: MalformedKind::UnknownValueTag(t),
                    ..
                }) => {
                    assert_eq!(t, bad);
                }
                other => panic!("tag {bad} gave {other:?}"),
            }
        }
    }

    #[test]
    fn non_finite_floats_cannot_be_encoded() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(
                Value::F64(bad).encode(),
                Err(FormatError::Malformed {
                    kind: MalformedKind::NonFiniteFloat,
                    ..
                })
            ));
        }
    }

    #[test]
    fn depth_is_reported_correctly() {
        assert_eq!(Value::Null.depth(), 1);
        assert_eq!(Value::Array(vec![]).depth(), 1);
        assert_eq!(Value::Array(vec![Value::Null]).depth(), 2);
        assert_eq!(map(&[("a", map(&[("b", Value::Null)]))]).depth(), 3);
    }

    #[test]
    fn nesting_at_the_limit_works_and_one_past_it_does_not() {
        let mut v = Value::Null;
        for _ in 1..MAX_VALUE_DEPTH {
            v = Value::Array(vec![v]);
        }
        assert_eq!(v.depth(), MAX_VALUE_DEPTH);
        roundtrip(&v);

        let too_deep = Value::Array(vec![v]);
        assert!(matches!(
            too_deep.encode(),
            Err(FormatError::Malformed {
                kind: MalformedKind::DepthExceeded { .. },
                ..
            })
        ));
    }

    /// The attack this defends against: a few hundred bytes of nesting that would otherwise
    /// recurse deeply enough to abort the process on a stack overflow.
    #[test]
    fn hostile_nesting_is_an_error_not_a_stack_overflow() {
        let mut bytes = Vec::new();
        for _ in 0..100_000 {
            bytes.push(tag::ARRAY);
            bytes.push(1); // varint count = 1
        }
        bytes.push(tag::NULL);
        match Value::decode(&bytes) {
            Err(FormatError::Malformed {
                kind: MalformedKind::DepthExceeded { max },
                ..
            }) => {
                assert_eq!(max, MAX_VALUE_DEPTH);
            }
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }

    /// The other resource attack: a tiny input claiming an enormous element count.
    #[test]
    fn an_absurd_container_count_is_refused_before_allocating() {
        let mut w = Writer::new();
        w.u8(tag::ARRAY).varint(u64::MAX);
        assert!(matches!(
            Value::decode(&w.clone().finish()),
            Err(FormatError::LengthExceedsInput { .. })
        ));

        let mut w = Writer::new();
        w.u8(tag::MAP).varint(1_000_000_000);
        assert!(matches!(
            Value::decode(&w.finish()),
            Err(FormatError::LengthExceedsInput { .. })
        ));
    }

    #[test]
    fn trailing_bytes_after_a_value_are_rejected() {
        let mut bytes = Value::I64(7).encode().unwrap();
        bytes.push(0);
        assert!(Value::decode(&bytes).is_err());
    }

    #[test]
    fn truncation_at_every_offset_is_an_error_and_never_a_panic() {
        let value = map(&[
            ("bytes", Value::Bytes(vec![1, 2, 3, 4, 5])),
            (
                "nested",
                Value::Array(vec![Value::I64(-42), Value::Bool(true)]),
            ),
            ("text", Value::Str("a reasonably long string value".into())),
        ]);
        let full = value.encode().unwrap();
        for len in 0..full.len() {
            let result = Value::decode(&full[..len]);
            assert!(result.is_err(), "a {len}-byte prefix decoded successfully");
        }
        assert_eq!(Value::decode(&full).unwrap(), value);
    }

    #[test]
    fn every_single_byte_corruption_is_an_error_or_a_different_value_never_a_panic() {
        let value = map(&[
            ("a", Value::I64(1)),
            ("b", Value::Array(vec![Value::Str("x".into())])),
        ]);
        let full = value.encode().unwrap();
        for i in 0..full.len() {
            for bit in 0..8 {
                let mut mutated = full.clone();
                mutated[i] ^= 1u8 << bit;
                // Either it fails cleanly or it decodes to something; neither may panic, and a
                // successful decode must still re-encode canonically.
                if let Ok(v) = Value::decode(&mutated) {
                    let _ = v.encode();
                }
            }
        }
    }

    #[test]
    fn find_path_reads_only_what_it_needs() {
        let v = map(&[
            ("alpha", Value::I64(1)),
            ("bravo", Value::Str("x".repeat(500))),
            ("charlie", Value::Bool(true)),
            ("delta", Value::Array(vec![Value::I64(1), Value::I64(2)])),
        ]);
        let bytes = v.encode().unwrap();

        assert_eq!(find_path(&bytes, "alpha").unwrap(), Some(Value::I64(1)));
        assert_eq!(
            find_path(&bytes, "charlie").unwrap(),
            Some(Value::Bool(true))
        );
        assert_eq!(
            find_path(&bytes, "delta").unwrap(),
            Some(Value::Array(vec![Value::I64(1), Value::I64(2)]))
        );
        assert_eq!(find_path(&bytes, "missing").unwrap(), None);
        // A key that sorts after everything present exercises the early stop.
        assert_eq!(find_path(&bytes, "zulu").unwrap(), None);
    }

    #[test]
    fn find_path_descends_into_nested_maps() {
        let inner = map(&[("plan", Value::Str("pro".into())), ("seats", Value::I64(5))]);
        let v = map(&[("other", Value::I64(9)), ("user", inner)]);
        let bytes = v.encode().unwrap();

        assert_eq!(
            find_path(&bytes, "user.plan").unwrap(),
            Some(Value::Str("pro".into()))
        );
        assert_eq!(
            find_path(&bytes, "user.seats").unwrap(),
            Some(Value::I64(5))
        );
        assert_eq!(find_path(&bytes, "user.missing").unwrap(), None);
        assert_eq!(
            find_path(&bytes, "other.nope").unwrap(),
            None,
            "cannot descend a scalar"
        );
        assert_eq!(find_path(&bytes, "").unwrap(), None);
    }

    /// The lazy lookup must agree with decoding everything and then looking up — otherwise the
    /// optimisation changes what a filter matches, which is far worse than being slow.
    #[test]
    fn find_path_agrees_with_a_full_decode_for_every_field() {
        let inner = map(&[("a", Value::Null), ("b", Value::F64(1.5))]);
        let v = map(&[
            ("bytes", Value::Bytes(vec![1, 2, 3])),
            ("empty_list", Value::Array(vec![])),
            ("list", Value::Array(vec![Value::Str("x".into())])),
            ("nested", inner),
            ("num", Value::I64(-7)),
            ("text", Value::Str("hello".into())),
        ]);
        let bytes = v.encode().unwrap();
        let Value::Map(decoded) = Value::decode(&bytes).unwrap() else {
            panic!("not a map")
        };

        for key in decoded.keys() {
            assert_eq!(
                find_path(&bytes, key).unwrap().as_ref(),
                decoded.get(key),
                "field {key}"
            );
        }
        for absent in ["", "zzz", "nested.zzz", "num.zzz", "list.0"] {
            assert_eq!(find_path(&bytes, absent).unwrap(), None, "{absent}");
        }
    }

    #[test]
    fn skipping_a_value_lands_exactly_where_decoding_it_would() {
        for value in [
            Value::Null,
            Value::Bool(true),
            Value::I64(i64::MIN),
            Value::F64(1.5),
            Value::Str("some text".into()),
            Value::Bytes(vec![0; 300]),
            Value::Array(vec![Value::I64(1), Value::Str("two".into())]),
            map(&[("a", Value::I64(1)), ("b", Value::Array(vec![Value::Null]))]),
        ] {
            let mut bytes = value.encode().unwrap();
            bytes.extend_from_slice(b"SENTINEL");

            let mut skipping = Reader::new(&bytes);
            skip_value(&mut skipping).unwrap();
            let mut decoding = Reader::new(&bytes);
            Value::read_from(&mut decoding).unwrap();

            assert_eq!(skipping.offset(), decoding.offset(), "{value:?}");
            assert_eq!(skipping.bytes(8).unwrap(), b"SENTINEL", "{value:?}");
        }
    }

    #[test]
    fn the_lazy_path_is_as_strict_as_a_full_decode() {
        // Hostile nesting must be refused while skipping, not just while decoding.
        let mut bytes = Vec::new();
        for _ in 0..100_000 {
            bytes.push(tag::ARRAY);
            bytes.push(1);
        }
        bytes.push(tag::NULL);
        let mut r = Reader::new(&bytes);
        assert!(matches!(
            skip_value(&mut r),
            Err(FormatError::Malformed {
                kind: MalformedKind::DepthExceeded { .. },
                ..
            })
        ));

        // And an absurd container count is refused before allocating.
        let mut w = Writer::new();
        w.u8(tag::ARRAY).varint(u64::MAX);
        let bytes = w.finish();
        assert!(skip_value(&mut Reader::new(&bytes)).is_err());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0xDEAD_BEEF_CAFE_1234u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 48) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            if let Ok(v) = Value::decode(&bytes) {
                // Anything that decodes must re-encode to exactly the same bytes.
                assert_eq!(
                    v.encode().unwrap(),
                    bytes,
                    "decode/encode disagreed on {bytes:?}"
                );
            }
        }
    }
}
