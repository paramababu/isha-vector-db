//! The manifest: the root of the database, written to two alternating slots.
//!
//! # Why two slots instead of write-temp-and-rename
//!
//! The manifest is the root of the tree. If it is lost or half-written, the database is lost.
//! The usual POSIX answer is to write a temporary file and `rename` it, relying on rename being
//! atomic — but OPFS cannot promise that, and neither can every Android storage volume.
//!
//! So instead there are two fixed slots, `MANIFEST-A` and `MANIFEST-B`, each self-describing
//! and self-checksumming, each carrying a monotonically increasing sequence number. Committing
//! means writing the slot that is *not* currently in use and syncing it. Opening means reading
//! both and taking the valid one with the higher sequence.
//!
//! A crash at any instant leaves at least one intact slot: either the new write completed (and
//! wins on sequence) or it did not (and fails its checksum, so the old slot wins). The protocol
//! needs nothing but durable positional writes, which is the one capability every backend has.
//!
//! Atomic rename, where available, is still used — but only for whole-file replacement of
//! segments and index snapshots, never for the root.

use crate::block::{decode_block, encode_block};
use crate::cursor::{Reader, Writer};
use crate::error::{FormatError, MalformedKind, Result};
use crate::header::FileKind;

/// Which of the two manifest slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Slot {
    /// `MANIFEST-A`.
    A,
    /// `MANIFEST-B`.
    B,
}

impl Slot {
    /// The other slot — where the next commit goes.
    pub const fn other(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// The file name for this slot.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::A => "MANIFEST-A",
            Self::B => "MANIFEST-B",
        }
    }
}

/// One segment referenced by a collection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRef {
    /// Segment id; names the `NNNNNN.{vec,dir,meta,del}` files.
    pub id: u64,
    /// Rows the segment holds, live and dead alike.
    pub rows: u32,
    /// Bumped every time the tombstone bitmap is rewritten, so a stale `.del` is detectable.
    pub del_generation: u32,
}

/// What the manifest records about one collection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectionEntry {
    /// The collection's name.
    pub name: String,
    /// Its live segments, in ascending id order.
    pub segments: Vec<SegmentRef>,
    /// The current index snapshot id, if one has been written.
    ///
    /// An index is derived data: a missing or unreadable snapshot is a rebuild, never a failure
    /// to open.
    pub index_snapshot: Option<u64>,
    /// Highest WAL sequence already folded into the segments. Replay starts after this.
    pub last_applied_wal: u64,
    /// Live documents.
    pub live_count: u64,
    /// Rows including tombstones, so the dead ratio — and therefore whether to compact — is
    /// known without reading any segment.
    pub total_rows: u64,
}

/// The database root record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Monotonically increasing. The higher valid slot wins.
    pub sequence: u64,
    /// Identifies this database, so a file copied between databases is detectable.
    pub db_uuid: [u8; 16],
    /// Creation time in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Last commit time in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
    /// Every collection in the database.
    pub collections: Vec<CollectionEntry>,
}

impl Manifest {
    /// An empty manifest at sequence 1.
    ///
    /// Sequence starts at 1, not 0, so that "no manifest has ever been written" and "sequence 0"
    /// are never confused.
    pub fn new(db_uuid: [u8; 16], now_ms: u64) -> Self {
        Self {
            sequence: 1,
            db_uuid,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            collections: Vec::new(),
        }
    }

    /// Look up a collection.
    pub fn collection(&self, name: &str) -> Option<&CollectionEntry> {
        self.collections.iter().find(|c| c.name == name)
    }

    /// Serialize to a complete, checksummed manifest slot.
    ///
    /// # Errors
    /// [`MalformedKind::Inconsistent`] if the structure violates an invariant the decoder will
    /// enforce — segments out of order, or duplicate collection names.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut w = Writer::new();
        w.u64(self.sequence)
            .raw(&self.db_uuid)
            .u64(self.created_at_ms)
            .u64(self.updated_at_ms)
            .varint(self.collections.len() as u64);
        for c in &self.collections {
            w.string(&c.name).varint(c.segments.len() as u64);
            for s in &c.segments {
                w.varint(s.id)
                    .u32(s.rows)
                    .varint(u64::from(s.del_generation));
            }
            // Option encoded as 0 = none, n = Some(n - 1), so no separate flag byte is needed
            // and the common "no snapshot yet" case costs one byte.
            match c.index_snapshot {
                None => w.varint(0),
                Some(id) => w.varint(id.saturating_add(1)),
            };
            w.varint(c.last_applied_wal)
                .varint(c.live_count)
                .varint(c.total_rows)
                .reserved(4);
        }
        w.reserved(8);
        Ok(encode_block(FileKind::Manifest, w.as_slice()))
    }

    /// Parse a manifest slot.
    ///
    /// # Errors
    /// Any [`FormatError`].
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        let payload = decode_block(bytes, FileKind::Manifest)?;
        let mut r = Reader::new(payload);

        let sequence = r.u64()?;
        let db_uuid: [u8; 16] = r.array()?;
        let created_at_ms = r.u64()?;
        let updated_at_ms = r.u64()?;

        let count_at = r.offset();
        let count = r.varint()?;
        // A collection entry cannot be shorter than a handful of bytes; refuse a count the
        // remaining input could not possibly supply before allocating for it.
        let count = bounded(count, r.remaining() / 8, count_at)?;
        let mut collections = Vec::with_capacity(count);

        for _ in 0..count {
            let name = r.string()?.to_owned();
            let seg_at = r.offset();
            let seg_count = r.varint()?;
            let seg_count = bounded(seg_count, r.remaining() / 6, seg_at)?;
            let mut segments = Vec::with_capacity(seg_count);
            for _ in 0..seg_count {
                let id = r.varint()?;
                let rows = r.u32()?;
                let del_generation =
                    u32::try_from(r.varint()?).map_err(|_| FormatError::Malformed {
                        offset: seg_at,
                        kind: MalformedKind::Inconsistent {
                            field: "del_generation",
                        },
                    })?;
                segments.push(SegmentRef {
                    id,
                    rows,
                    del_generation,
                });
            }
            let snapshot = r.varint()?;
            let index_snapshot = if snapshot == 0 {
                None
            } else {
                Some(snapshot - 1)
            };
            let last_applied_wal = r.varint()?;
            let live_count = r.varint()?;
            let total_rows = r.varint()?;
            r.reserved(4)?;
            collections.push(CollectionEntry {
                name,
                segments,
                index_snapshot,
                last_applied_wal,
                live_count,
                total_rows,
            });
        }
        r.reserved(8)?;
        r.expect_end("manifest")?;

        let m = Self {
            sequence,
            db_uuid,
            created_at_ms,
            updated_at_ms,
            collections,
        };
        m.validate()?;
        Ok(m)
    }

    /// Read both slots and decide which one is authoritative.
    ///
    /// Never fails: a database with two unreadable slots is a real situation the caller has to
    /// report well, so the reasons for both rejections are returned rather than one error
    /// replacing the other.
    pub fn scan_slots(a: Option<&[u8]>, b: Option<&[u8]>) -> SlotScan {
        let sa = SlotStatus::examine(a);
        let sb = SlotStatus::examine(b);
        let chosen = match (&sa, &sb) {
            (SlotStatus::Valid(ma), SlotStatus::Valid(mb)) => {
                // Equal sequences should be impossible; choosing deterministically means the
                // situation is at least reproducible rather than depending on read order.
                if mb.sequence > ma.sequence {
                    Some((Slot::B, mb.clone()))
                } else {
                    Some((Slot::A, ma.clone()))
                }
            }
            (SlotStatus::Valid(ma), _) => Some((Slot::A, ma.clone())),
            (_, SlotStatus::Valid(mb)) => Some((Slot::B, mb.clone())),
            _ => None,
        };
        SlotScan {
            chosen,
            a: sa,
            b: sb,
        }
    }

    fn validate(&self) -> Result<()> {
        let mut names: Vec<&str> = self.collections.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        if names.len() != before {
            return Err(FormatError::Malformed {
                offset: 0,
                kind: MalformedKind::DuplicateKey,
            });
        }
        for c in &self.collections {
            if c.name.is_empty() {
                return Err(FormatError::Malformed {
                    offset: 0,
                    kind: MalformedKind::ZeroNotAllowed {
                        field: "collection name",
                    },
                });
            }
            // Segments ascend so recovery, compaction and the `inspect` output all agree on an
            // order without having to sort first.
            let mut last_id: Option<u64> = None;
            for s in &c.segments {
                if let Some(prev) = last_id {
                    if s.id <= prev {
                        return Err(FormatError::Malformed {
                            offset: 0,
                            kind: MalformedKind::Inconsistent {
                                field: "segment order",
                            },
                        });
                    }
                }
                last_id = Some(s.id);
            }
            let rows: u64 = c.segments.iter().map(|s| u64::from(s.rows)).sum();
            if c.total_rows != rows {
                return Err(FormatError::Malformed {
                    offset: 0,
                    kind: MalformedKind::Inconsistent {
                        field: "total_rows",
                    },
                });
            }
            if c.live_count > c.total_rows {
                return Err(FormatError::Malformed {
                    offset: 0,
                    kind: MalformedKind::Inconsistent {
                        field: "live_count",
                    },
                });
            }
        }
        Ok(())
    }
}

fn bounded(claimed: u64, max_possible: usize, at: u64) -> Result<usize> {
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

/// What one slot turned out to be.
#[derive(Debug, Clone, PartialEq)]
pub enum SlotStatus {
    /// The file was absent. Normal for a database that has only ever committed once.
    Missing,
    /// The file was present but unreadable.
    Invalid(FormatError),
    /// The file decoded.
    Valid(Manifest),
}

impl SlotStatus {
    fn examine(bytes: Option<&[u8]>) -> Self {
        match bytes {
            None => Self::Missing,
            Some(b) => match Manifest::decode(b) {
                Ok(m) => Self::Valid(m),
                Err(e) => Self::Invalid(e),
            },
        }
    }

    /// A short description for the `NoValidManifest` error, which reports both slots.
    pub fn describe(&self) -> String {
        match self {
            Self::Missing => "missing".to_owned(),
            Self::Invalid(e) => e.to_string(),
            Self::Valid(m) => format!("valid, sequence {}", m.sequence),
        }
    }
}

/// The result of examining both slots.
#[derive(Debug, Clone, PartialEq)]
pub struct SlotScan {
    /// The authoritative slot and its manifest, if either slot was readable.
    pub chosen: Option<(Slot, Manifest)>,
    /// What slot A turned out to be.
    pub a: SlotStatus,
    /// What slot B turned out to be.
    pub b: SlotStatus,
}

impl SlotScan {
    /// Where the next commit should be written.
    ///
    /// The slot not currently authoritative, so a crash mid-commit cannot damage the state the
    /// database would otherwise fall back to. With no valid slot at all, A is where a fresh
    /// database starts.
    pub fn next_slot(&self) -> Slot {
        match &self.chosen {
            Some((slot, _)) => slot.other(),
            None => Slot::A,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> [u8; 16] {
        [7u8; 16]
    }

    fn entry(name: &str, segments: &[(u64, u32)]) -> CollectionEntry {
        let segments: Vec<SegmentRef> = segments
            .iter()
            .map(|&(id, rows)| SegmentRef {
                id,
                rows,
                del_generation: 0,
            })
            .collect();
        let total_rows = segments.iter().map(|s| u64::from(s.rows)).sum();
        CollectionEntry {
            name: name.into(),
            segments,
            index_snapshot: None,
            last_applied_wal: 0,
            live_count: total_rows,
            total_rows,
        }
    }

    fn sample() -> Manifest {
        Manifest {
            sequence: 42,
            db_uuid: uuid(),
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_500_000,
            collections: vec![
                entry("products", &[(1, 1000), (2, 512)]),
                CollectionEntry {
                    index_snapshot: Some(9),
                    last_applied_wal: 12_345,
                    live_count: 40,
                    ..entry("notes", &[(7, 64)])
                },
            ],
        }
    }

    #[test]
    fn round_trips() {
        let m = sample();
        assert_eq!(Manifest::decode(&m.encode().unwrap()).unwrap(), m);
    }

    #[test]
    fn an_empty_manifest_round_trips() {
        let m = Manifest::new(uuid(), 1);
        assert_eq!(Manifest::decode(&m.encode().unwrap()).unwrap(), m);
        assert_eq!(m.sequence, 1, "sequence 0 must never be a real manifest");
    }

    #[test]
    fn collection_lookup_works() {
        let m = sample();
        assert_eq!(m.collection("notes").unwrap().live_count, 40);
        assert!(m.collection("absent").is_none());
    }

    // ---- slot selection: the part that decides whether a database opens at all ----

    #[test]
    fn the_higher_sequence_wins_regardless_of_slot() {
        let older = Manifest {
            sequence: 5,
            ..sample()
        }
        .encode()
        .unwrap();
        let newer = Manifest {
            sequence: 6,
            ..sample()
        }
        .encode()
        .unwrap();

        let scan = Manifest::scan_slots(Some(&older), Some(&newer));
        assert_eq!(scan.chosen.as_ref().unwrap().0, Slot::B);
        assert_eq!(scan.chosen.as_ref().unwrap().1.sequence, 6);
        assert_eq!(scan.next_slot(), Slot::A);

        let scan = Manifest::scan_slots(Some(&newer), Some(&older));
        assert_eq!(scan.chosen.as_ref().unwrap().0, Slot::A);
        assert_eq!(scan.next_slot(), Slot::B);
    }

    /// The crash-during-commit case: the half-written slot fails its checksum and the intact
    /// older slot is used. This is the property the whole dual-slot design exists for.
    #[test]
    fn a_torn_slot_loses_to_an_intact_older_one() {
        let good = Manifest {
            sequence: 5,
            ..sample()
        }
        .encode()
        .unwrap();
        let mut torn = Manifest {
            sequence: 6,
            ..sample()
        }
        .encode()
        .unwrap();
        let mid = torn.len() / 2;
        torn.truncate(mid); // killed part-way through the write

        let scan = Manifest::scan_slots(Some(&good), Some(&torn));
        let (slot, m) = scan.chosen.as_ref().unwrap();
        assert_eq!(*slot, Slot::A);
        assert_eq!(m.sequence, 5);
        assert!(matches!(scan.b, SlotStatus::Invalid(_)));
        // The next commit overwrites the damaged slot, not the good one.
        assert_eq!(scan.next_slot(), Slot::B);
    }

    #[test]
    fn a_corrupted_slot_loses_even_with_a_higher_sequence() {
        let good = Manifest {
            sequence: 5,
            ..sample()
        }
        .encode()
        .unwrap();
        let mut bad = Manifest {
            sequence: 99,
            ..sample()
        }
        .encode()
        .unwrap();
        let n = bad.len();
        bad[n / 2] ^= 0xFF; // bit rot in the payload

        let scan = Manifest::scan_slots(Some(&good), Some(&bad));
        assert_eq!(scan.chosen.as_ref().unwrap().1.sequence, 5);
    }

    #[test]
    fn a_single_slot_is_enough() {
        let only = sample().encode().unwrap();
        let scan = Manifest::scan_slots(Some(&only), None);
        assert_eq!(scan.chosen.as_ref().unwrap().0, Slot::A);
        assert_eq!(scan.b, SlotStatus::Missing);
        assert_eq!(scan.next_slot(), Slot::B);

        let scan = Manifest::scan_slots(None, Some(&only));
        assert_eq!(scan.chosen.as_ref().unwrap().0, Slot::B);
        assert_eq!(scan.next_slot(), Slot::A);
    }

    #[test]
    fn no_valid_slot_reports_why_for_both() {
        let scan = Manifest::scan_slots(None, Some(b"garbage bytes here"));
        assert!(scan.chosen.is_none());
        assert_eq!(scan.a.describe(), "missing");
        assert!(scan.b.describe().contains("magic"), "{}", scan.b.describe());
        // A fresh database, and a repaired one, both start at slot A.
        assert_eq!(scan.next_slot(), Slot::A);
    }

    #[test]
    fn equal_sequences_resolve_deterministically() {
        let a = sample().encode().unwrap();
        let b = sample().encode().unwrap();
        let first = Manifest::scan_slots(Some(&a), Some(&b));
        let second = Manifest::scan_slots(Some(&a), Some(&b));
        assert_eq!(first.chosen.as_ref().unwrap().0, Slot::A);
        assert_eq!(first, second);
    }

    #[test]
    fn slots_alternate() {
        assert_eq!(Slot::A.other(), Slot::B);
        assert_eq!(Slot::B.other(), Slot::A);
        assert_eq!(Slot::A.file_name(), "MANIFEST-A");
        assert_eq!(Slot::B.file_name(), "MANIFEST-B");
    }

    // ---- invariants ----

    #[test]
    fn duplicate_collection_names_are_rejected() {
        let m = Manifest {
            collections: vec![entry("dup", &[(1, 1)]), entry("dup", &[(2, 1)])],
            ..sample()
        };
        assert!(matches!(
            m.encode(),
            Err(FormatError::Malformed {
                kind: MalformedKind::DuplicateKey,
                ..
            })
        ));
    }

    #[test]
    fn out_of_order_segments_are_rejected() {
        let m = Manifest {
            collections: vec![entry("c", &[(5, 1), (2, 1)])],
            ..sample()
        };
        assert!(matches!(
            m.encode(),
            Err(FormatError::Malformed {
                kind: MalformedKind::Inconsistent {
                    field: "segment order"
                },
                ..
            })
        ));
    }

    #[test]
    fn counts_that_disagree_with_the_segments_are_rejected() {
        let mut c = entry("c", &[(1, 10)]);
        c.total_rows = 99;
        let m = Manifest {
            collections: vec![c],
            ..sample()
        };
        assert!(m.encode().is_err());

        let mut c = entry("c", &[(1, 10)]);
        c.live_count = 11;
        let m = Manifest {
            collections: vec![c],
            ..sample()
        };
        assert!(m.encode().is_err());
    }

    #[test]
    fn a_manifest_claiming_a_billion_collections_is_refused_before_allocating() {
        let mut w = Writer::new();
        w.u64(1).raw(&uuid()).u64(0).u64(0).varint(1_000_000_000);
        let block = encode_block(FileKind::Manifest, w.as_slice());
        assert!(matches!(
            Manifest::decode(&block),
            Err(FormatError::LengthExceedsInput { .. })
        ));
    }

    #[test]
    fn truncation_at_every_length_is_an_error() {
        let bytes = sample().encode().unwrap();
        for len in 0..bytes.len() {
            assert!(
                Manifest::decode(&bytes[..len]).is_err(),
                "{len} bytes decoded"
            );
        }
    }

    #[test]
    fn every_single_bit_flip_is_detected() {
        let bytes = sample().encode().unwrap();
        for byte in 0..bytes.len() {
            for bit in 0..8 {
                let mut mutated = bytes.clone();
                mutated[byte] ^= 1u8 << bit;
                assert!(
                    Manifest::decode(&mutated).is_err(),
                    "flip at byte {byte} bit {bit} slipped through"
                );
            }
        }
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        let mut seed = 0x1111_2222_3333_4444u64;
        for _ in 0..20_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let len = (seed % 120) as usize;
            let bytes: Vec<u8> = (0..len).map(|i| (seed >> (i % 56)) as u8).collect();
            let _ = Manifest::decode(&bytes);
            let _ = Manifest::scan_slots(Some(&bytes), None);
        }
    }
}
