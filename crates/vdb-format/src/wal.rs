//! The write-ahead log.
//!
//! Every mutation is appended here and made durable before it is visible, so a process that
//! dies at any instant can be replayed back to a consistent state. On mobile this is not an
//! edge case: the operating system kills applications without warning, routinely.
//!
//! # Frame layout
//!
//! ```text
//! [ body_len u32 ][ crc32c u32 over body ][ body ]
//!
//! body = sequence varint
//!        txn_id   varint      (0 = a standalone operation)
//!        op       u8
//!        payload  (per op)
//! ```
//!
//! # Torn tails are expected, corruption is not
//!
//! A process killed mid-append leaves a partial frame at the end of the file. That is normal,
//! and recovery truncates it. But a frame that is *fully present* and fails its checksum means
//! the bytes were written and are wrong, which is a different diagnosis entirely.
//!
//! [`scan`] distinguishes the two by which failure it hits: not enough bytes for the frame is a
//! [`WalTail::Torn`]; a complete frame that fails its checksum or its structure is a
//! [`WalTail::Corrupt`]. Conflating them would mean either treating every unclean shutdown as
//! corruption, or silently discarding real damage. Both are bad, in opposite directions.
//!
//! # Atomic batches
//!
//! Operations sharing a `txn_id` are applied only if a matching [`WalOp::Commit`] frame follows
//! them. A crash part-way through a batch therefore rolls the whole batch back, because the
//! commit record never made it to disk. The field is a `u64` rather than a flag so interactive
//! transactions can reuse it later without a format change.

use crate::crc32c::crc32c;
use crate::cursor::{Reader, Writer};
use crate::error::{FormatError, MalformedKind, Result};

/// Bytes of framing before each body: length and checksum.
pub const FRAME_HEADER_LEN: usize = 8;

/// Largest body a single frame may declare.
///
/// A bound on how much a corrupt length prefix can make a reader consider. It is generous
/// enough for a large vector with metadata and small enough that a garbage value is rejected
/// rather than acted on.
pub const MAX_FRAME_BODY: u32 = 64 * 1024 * 1024;

mod op {
    pub(crate) const PUT: u8 = 1;
    pub(crate) const DELETE: u8 = 2;
    pub(crate) const COMMIT: u8 = 3;
}

/// What a frame does.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WalOp {
    /// Insert or replace a document.
    Put {
        /// The document id, interpreted according to the collection's `IdKind`.
        id: Vec<u8>,
        /// Raw vector bytes in the collection's dtype.
        vector: Vec<u8>,
        /// Encoded metadata map; empty when there is none.
        metadata: Vec<u8>,
        /// Optional opaque payload, such as the source text.
        content: Option<Vec<u8>>,
    },
    /// Remove a document.
    Delete {
        /// The document id.
        id: Vec<u8>,
    },
    /// Close a transaction group, making everything in it applicable.
    Commit {
        /// How many operations the group contained, so replay can verify it saw them all.
        op_count: u64,
    },
}

impl WalOp {
    const fn code(&self) -> u8 {
        match self {
            Self::Put { .. } => op::PUT,
            Self::Delete { .. } => op::DELETE,
            Self::Commit { .. } => op::COMMIT,
        }
    }
}

/// One log record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalFrame {
    /// Monotonically increasing within a collection's log.
    pub sequence: u64,
    /// Transaction group, or 0 for a standalone operation.
    pub txn_id: u64,
    /// The operation.
    pub op: WalOp,
}

impl WalFrame {
    /// A standalone (immediately durable) operation.
    pub fn standalone(sequence: u64, op: WalOp) -> Self {
        Self {
            sequence,
            txn_id: 0,
            op,
        }
    }

    /// An operation belonging to a transaction group.
    pub fn in_txn(sequence: u64, txn_id: u64, op: WalOp) -> Self {
        Self {
            sequence,
            txn_id,
            op,
        }
    }

    /// Encode the frame, framing and all.
    ///
    /// # Errors
    /// [`MalformedKind::Inconsistent`] if the body would exceed [`MAX_FRAME_BODY`].
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        self.append_to(&mut w)?;
        Ok(w.finish())
    }

    /// Append the frame to a writer, for batching several appends into one write.
    ///
    /// # Errors
    /// As [`WalFrame::encode`].
    pub fn append_to(&self, out: &mut Writer) -> Result<()> {
        let mut body = Writer::new();
        body.varint(self.sequence)
            .varint(self.txn_id)
            .u8(self.op.code());
        match &self.op {
            WalOp::Put {
                id,
                vector,
                metadata,
                content,
            } => {
                body.blob(id).blob(vector).blob(metadata);
                match content {
                    Some(c) => {
                        body.u8(1).blob(c);
                    }
                    None => {
                        body.u8(0);
                    }
                }
            }
            WalOp::Delete { id } => {
                body.blob(id);
            }
            WalOp::Commit { op_count } => {
                body.varint(*op_count);
            }
        }
        let body = body.finish();
        let len = u32::try_from(body.len())
            .ok()
            .filter(|&l| l <= MAX_FRAME_BODY)
            .ok_or(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::Inconsistent {
                    field: "frame body length",
                },
            })?;
        out.u32(len).u32(crc32c(&body)).raw(&body);
        Ok(())
    }

    /// Decode one frame from the front of `bytes`, returning it and its total encoded length.
    ///
    /// # Errors
    /// [`FormatError::Truncated`] for an incomplete frame (a torn tail), or another
    /// [`FormatError`] for a complete but damaged one.
    pub fn decode_at(bytes: &[u8], base: u64) -> Result<(Self, usize)> {
        let mut r = Reader::with_base(bytes, base);
        let len_at = r.offset();
        let body_len = r.u32()?;
        let stored_crc = r.u32()?;

        if body_len > MAX_FRAME_BODY {
            return Err(FormatError::LengthExceedsInput {
                offset: len_at,
                claimed: u64::from(body_len),
                available: MAX_FRAME_BODY.into(),
            });
        }
        // Not enough bytes for the declared body is the torn-tail signal, so it must surface as
        // Truncated rather than as a length inconsistency.
        let body = r.bytes(body_len as usize)?;

        let computed = crc32c(body);
        if computed != stored_crc {
            return Err(FormatError::ChecksumMismatch {
                offset: len_at,
                expected: stored_crc,
                found: computed,
            });
        }

        let mut b = Reader::with_base(body, base + FRAME_HEADER_LEN as u64);
        let sequence = b.varint()?;
        let txn_id = b.varint()?;
        let op_at = b.offset();
        let code = b.u8()?;
        let op = match code {
            op::PUT => {
                let id = b.blob()?.to_vec();
                let vector = b.blob()?.to_vec();
                let metadata = b.blob()?.to_vec();
                let has_content = b.u8()?;
                let content = match has_content {
                    0 => None,
                    1 => Some(b.blob()?.to_vec()),
                    other => {
                        return Err(FormatError::Malformed {
                            offset: b.offset(),
                            kind: MalformedKind::UnknownDiscriminant {
                                field: "content flag",
                                value: other,
                            },
                        })
                    }
                };
                WalOp::Put {
                    id,
                    vector,
                    metadata,
                    content,
                }
            }
            op::DELETE => WalOp::Delete {
                id: b.blob()?.to_vec(),
            },
            op::COMMIT => WalOp::Commit {
                op_count: b.varint()?,
            },
            other => {
                return Err(FormatError::Malformed {
                    offset: op_at,
                    kind: MalformedKind::UnknownDiscriminant {
                        field: "wal op",
                        value: other,
                    },
                })
            }
        };
        b.expect_end("wal frame body")?;

        Ok((
            Self {
                sequence,
                txn_id,
                op,
            },
            FRAME_HEADER_LEN + body_len as usize,
        ))
    }
}

/// How a log ended.
#[derive(Debug, Clone, PartialEq)]
pub enum WalTail {
    /// Every byte was a complete, valid frame.
    Clean,
    /// The last frame was incomplete — a write interrupted by process death.
    ///
    /// Expected, not an error. Recovery truncates the log here and carries on.
    Torn {
        /// Where the incomplete frame began.
        offset: u64,
        /// Bytes after that point.
        trailing_bytes: usize,
    },
    /// A complete frame failed its checksum or its structure.
    ///
    /// The bytes reached the disk and are wrong, which is damage rather than an interrupted
    /// write, and is reported to the caller rather than silently discarded.
    Corrupt {
        /// Where the bad frame began.
        offset: u64,
        /// What was wrong with it.
        reason: FormatError,
    },
}

/// The result of reading a log file.
#[derive(Debug, Clone, PartialEq)]
pub struct WalScan {
    /// Frames decoded, in file order.
    pub frames: Vec<WalFrame>,
    /// How the log ended.
    pub tail: WalTail,
    /// Bytes covered by complete, valid frames — where the log should be truncated to.
    pub valid_bytes: u64,
}

impl WalScan {
    /// Frames belonging to committed work only.
    ///
    /// Standalone frames (`txn_id == 0`) always count. Frames in a transaction group count only
    /// if that group's [`WalOp::Commit`] frame is present, which is what makes a batch
    /// all-or-nothing across a crash.
    pub fn committed(&self) -> Vec<&WalFrame> {
        let mut committed_txns = Vec::new();
        for f in &self.frames {
            if let WalOp::Commit { .. } = f.op {
                committed_txns.push(f.txn_id);
            }
        }
        self.frames
            .iter()
            .filter(|f| !matches!(f.op, WalOp::Commit { .. }))
            .filter(|f| f.txn_id == 0 || committed_txns.contains(&f.txn_id))
            .collect()
    }
}

/// Read every frame in a log file's payload.
///
/// Stops at the first frame that does not decode, and reports whether that was a torn tail or
/// real corruption.
pub fn scan(bytes: &[u8]) -> WalScan {
    let mut frames = Vec::new();
    let mut offset = 0usize;

    loop {
        let remaining = bytes.len().saturating_sub(offset);
        if remaining == 0 {
            return WalScan {
                frames,
                tail: WalTail::Clean,
                valid_bytes: offset as u64,
            };
        }
        let Some(rest) = bytes.get(offset..) else {
            return WalScan {
                frames,
                tail: WalTail::Clean,
                valid_bytes: offset as u64,
            };
        };
        match WalFrame::decode_at(rest, offset as u64) {
            Ok((frame, len)) => {
                frames.push(frame);
                offset = offset.saturating_add(len);
            }
            Err(e) if e.is_truncation() => {
                return WalScan {
                    frames,
                    tail: WalTail::Torn {
                        offset: offset as u64,
                        trailing_bytes: remaining,
                    },
                    valid_bytes: offset as u64,
                }
            }
            Err(reason) => {
                return WalScan {
                    frames,
                    tail: WalTail::Corrupt {
                        offset: offset as u64,
                        reason,
                    },
                    valid_bytes: offset as u64,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(seq: u64, id: &str) -> WalFrame {
        WalFrame::standalone(
            seq,
            WalOp::Put {
                id: id.as_bytes().to_vec(),
                vector: vec![0u8; 16],
                metadata: vec![],
                content: None,
            },
        )
    }

    fn log(frames: &[WalFrame]) -> Vec<u8> {
        let mut w = Writer::new();
        for f in frames {
            f.append_to(&mut w).unwrap();
        }
        w.finish()
    }

    #[test]
    fn every_op_round_trips() {
        let frames = vec![
            put(1, "a"),
            WalFrame::standalone(
                2,
                WalOp::Put {
                    id: b"with-everything".to_vec(),
                    vector: vec![1, 2, 3, 4],
                    metadata: vec![8, 0],
                    content: Some(b"the source text".to_vec()),
                },
            ),
            WalFrame::standalone(
                3,
                WalOp::Delete {
                    id: b"gone".to_vec(),
                },
            ),
            WalFrame::in_txn(4, 77, WalOp::Commit { op_count: 3 }),
        ];
        let bytes = log(&frames);
        let scan = scan(&bytes);
        assert_eq!(scan.tail, WalTail::Clean);
        assert_eq!(scan.frames, frames);
        assert_eq!(scan.valid_bytes, bytes.len() as u64);
    }

    #[test]
    fn an_empty_log_is_clean() {
        let scan = scan(&[]);
        assert_eq!(scan.tail, WalTail::Clean);
        assert!(scan.frames.is_empty());
        assert_eq!(scan.valid_bytes, 0);
    }

    /// The central recovery property: a process killed mid-append leaves a tail that is
    /// truncated, not treated as damage.
    #[test]
    fn a_torn_tail_at_every_cut_point_is_recognised_as_torn() {
        let complete = log(&[put(1, "one"), put(2, "two")]);
        let first_frame_len = WalFrame::encode(&put(1, "one")).unwrap().len();

        for cut in (first_frame_len + 1)..complete.len() {
            let partial = &complete[..cut];
            let scan = scan(partial);
            assert_eq!(
                scan.frames.len(),
                1,
                "cut at {cut} should keep the first frame"
            );
            assert_eq!(scan.valid_bytes, first_frame_len as u64);
            match scan.tail {
                WalTail::Torn { offset, .. } => assert_eq!(offset, first_frame_len as u64),
                other => panic!("cut at {cut} gave {other:?}, expected Torn"),
            }
        }
    }

    #[test]
    fn a_log_cut_inside_its_very_first_frame_yields_nothing_and_is_torn() {
        let complete = log(&[put(1, "one")]);
        for cut in 0..complete.len() {
            let scan = scan(&complete[..cut]);
            assert!(scan.frames.is_empty(), "cut at {cut}");
            assert_eq!(scan.valid_bytes, 0);
            if cut > 0 {
                assert!(
                    matches!(scan.tail, WalTail::Torn { offset: 0, .. }),
                    "cut at {cut}"
                );
            }
        }
    }

    /// The other half of the distinction: a frame that is entirely present but wrong is damage.
    #[test]
    fn a_complete_frame_that_fails_its_checksum_is_corruption_not_a_torn_tail() {
        let mut bytes = log(&[put(1, "one"), put(2, "two")]);
        let first_len = WalFrame::encode(&put(1, "one")).unwrap().len();
        // Flip a bit inside the second frame's body, leaving its length intact.
        let target = first_len + FRAME_HEADER_LEN + 2;
        bytes[target] ^= 0x01;

        let scan = scan(&bytes);
        assert_eq!(scan.frames.len(), 1);
        match scan.tail {
            WalTail::Corrupt { offset, reason } => {
                assert_eq!(offset, first_len as u64);
                assert!(
                    matches!(reason, FormatError::ChecksumMismatch { .. }),
                    "{reason:?}"
                );
            }
            other => panic!("expected Corrupt, got {other:?}"),
        }
    }

    #[test]
    fn every_bit_flip_in_a_frame_is_caught_as_corruption_or_truncation_never_silently() {
        let original = log(&[put(1, "document-one")]);
        for byte in 0..original.len() {
            for bit in 0..8 {
                let mut mutated = original.clone();
                mutated[byte] ^= 1u8 << bit;
                let scan = scan(&mutated);
                let unchanged = scan.frames.first() == Some(&put(1, "document-one"))
                    && scan.tail == WalTail::Clean;
                assert!(
                    !unchanged,
                    "flip at byte {byte} bit {bit} produced an identical clean scan"
                );
            }
        }
    }

    #[test]
    fn a_garbage_length_prefix_is_refused_rather_than_acted_on() {
        let mut bytes = log(&[put(1, "x")]);
        bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let scan = scan(&bytes);
        assert!(scan.frames.is_empty());
        assert!(matches!(scan.tail, WalTail::Corrupt { .. }));
    }

    #[test]
    fn an_unknown_op_code_is_corruption() {
        let mut w = Writer::new();
        let mut body = Writer::new();
        body.varint(1).varint(0).u8(99);
        let body = body.finish();
        w.u32(body.len() as u32).u32(crc32c(&body)).raw(&body);
        let scan = scan(&w.finish());
        match scan.tail {
            WalTail::Corrupt {
                reason: FormatError::Malformed { kind, .. },
                ..
            } => {
                assert_eq!(
                    kind,
                    MalformedKind::UnknownDiscriminant {
                        field: "wal op",
                        value: 99
                    }
                );
            }
            other => panic!("expected an unknown op, got {other:?}"),
        }
    }

    #[test]
    fn trailing_junk_in_a_frame_body_is_rejected() {
        let mut body = Writer::new();
        body.varint(1)
            .varint(0)
            .u8(op::COMMIT)
            .varint(2)
            .raw(b"extra");
        let body = body.finish();
        let mut w = Writer::new();
        w.u32(body.len() as u32).u32(crc32c(&body)).raw(&body);
        assert!(matches!(scan(&w.finish()).tail, WalTail::Corrupt { .. }));
    }

    // ---- batch atomicity ----

    #[test]
    fn an_uncommitted_transaction_is_not_applied() {
        let frames = vec![
            put(1, "standalone"),
            WalFrame::in_txn(
                2,
                5,
                WalOp::Put {
                    id: b"batch-a".to_vec(),
                    vector: vec![0; 4],
                    metadata: vec![],
                    content: None,
                },
            ),
            WalFrame::in_txn(
                3,
                5,
                WalOp::Put {
                    id: b"batch-b".to_vec(),
                    vector: vec![0; 4],
                    metadata: vec![],
                    content: None,
                },
            ),
            // no Commit: the process died before the batch closed
        ];
        let scan = scan(&log(&frames));
        let committed = scan.committed();
        assert_eq!(committed.len(), 1, "only the standalone op should apply");
        assert_eq!(committed[0].sequence, 1);
    }

    #[test]
    fn a_committed_transaction_is_applied_in_full() {
        let frames = vec![
            WalFrame::in_txn(
                1,
                5,
                WalOp::Put {
                    id: b"a".to_vec(),
                    vector: vec![0; 4],
                    metadata: vec![],
                    content: None,
                },
            ),
            WalFrame::in_txn(2, 5, WalOp::Delete { id: b"b".to_vec() }),
            WalFrame::in_txn(3, 5, WalOp::Commit { op_count: 2 }),
        ];
        let scan = scan(&log(&frames));
        assert_eq!(scan.committed().len(), 2);
    }

    /// A batch torn mid-write must roll back entirely — this is the whole promise of
    /// `write_batch`.
    #[test]
    fn a_batch_torn_before_its_commit_rolls_back_completely() {
        let mut frames = vec![put(1, "before-the-batch")];
        for i in 0..5u64 {
            frames.push(WalFrame::in_txn(
                2 + i,
                9,
                WalOp::Put {
                    id: format!("batch-{i}").into_bytes(),
                    vector: vec![0; 8],
                    metadata: vec![],
                    content: None,
                },
            ));
        }
        let full = log(&frames);
        let with_commit = {
            let mut f = frames.clone();
            f.push(WalFrame::in_txn(7, 9, WalOp::Commit { op_count: 5 }));
            log(&f)
        };

        // Every truncation point before the commit lands must roll the batch back.
        for cut in full.len()..with_commit.len() {
            let scan = scan(&with_commit[..cut]);
            assert_eq!(
                scan.committed().len(),
                1,
                "cut at {cut} leaked part of an uncommitted batch"
            );
        }
        // With the commit fully present, all six apply.
        assert_eq!(scan(&with_commit).committed().len(), 6);
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0x9999_8888_7777_6666u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 100) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            let s = scan(&bytes);
            let _ = s.committed();
        }
    }
}
