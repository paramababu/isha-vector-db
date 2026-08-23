//! Golden fixtures: byte-level proof that format v1 has not changed.
//!
//! Every structure is encoded from a fixed input and compared against bytes committed in
//! `testdata/v1/`. A refactor that alters the layout — a field reordered, a varint that became
//! a `u32`, a default that changed — fails here with the exact offset, rather than shipping and
//! making every database written by the previous release unreadable.
//!
//! To regenerate after a *deliberate* format change: `VDB_BLESS=1 cargo test -p isha-vector-db-format
//! --test golden`. That change must bump `FORMAT_VERSION`, ship a migration, and say
//! `FORMAT-CHANGE:` in the pull request. CI checks for a diff in `testdata/` and refuses it
//! otherwise.
//!
//! This file reads and writes files, which the library itself must never do. That is fine and
//! deliberate: `scripts/check-core-purity.sh` guards `src/`, because it is the *library* that
//! has to be platform-independent, not its tests.

// Integration tests are a separate crate, so the crate-level test allows in `lib.rs` do not
// reach here. Failing loudly is this file's entire job.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use isha_vector_db_format::segment::{
    Directory, DirectoryWriter, MetaBlock, MetaRecord, MetaWriter, Tombstones, VectorBlock,
    VectorBlockWriter,
};
use isha_vector_db_format::{
    Catalog, CollectionEntry, IdKind, IndexSpec, Manifest, Metric, SegmentRef, Value, VectorDType,
    WalFrame, WalOp, Writer,
};

/// Fixtures live under `testdata/v{version}`. Older directories are never regenerated: they are
/// the record of what previous releases actually wrote, and the only honest way to test that this
/// build can still read them.
fn testdata(version: u16, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../testdata/v{version}"))
        .join(name)
}

/// Compare against the committed fixture, or write it when blessing.
fn golden(name: &str, actual: &[u8]) {
    let path = testdata(isha_vector_db_format::FORMAT_VERSION, name);
    if std::env::var("VDB_BLESS").is_ok() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).expect("create fixture directory");
        }
        std::fs::write(&path, actual).expect("write fixture");
        return;
    }
    let expected = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => panic!(
            "missing fixture {}: {e}\nrun with VDB_BLESS=1 to create it, and explain the \
             format change in your pull request",
            path.display()
        ),
    };
    if expected == actual {
        return;
    }
    // Report the first difference precisely: "the bytes changed" is not actionable.
    let first_diff = expected
        .iter()
        .zip(actual.iter())
        .position(|(a, b)| a != b)
        .unwrap_or(expected.len().min(actual.len()));
    panic!(
        "format v{} changed in {name}\n  \
         expected {} bytes, produced {} bytes\n  \
         first difference at offset {first_diff}: expected {:02x?}, produced {:02x?}\n  \
         If this change is deliberate: bump FORMAT_VERSION, add a migration, re-bless with \
         VDB_BLESS=1, and write FORMAT-CHANGE: in the PR body.",
        isha_vector_db_format::FORMAT_VERSION,
        expected.len(),
        actual.len(),
        expected.get(first_diff),
        actual.get(first_diff),
    );
}

// ---------------------------------------------------------------------------
// Fixed inputs. These must never change: they are the definition of the fixtures.
// ---------------------------------------------------------------------------

fn fixture_catalog() -> Catalog {
    Catalog {
        name: "products".into(),
        dimension: 4,
        metric: Metric::Cosine,
        dtype: VectorDType::F32,
        id_kind: IdKind::Str { max_len: 512 },
        index: IndexSpec::Flat,
        created_at_ms: 1_700_000_000_000,
    }
}

fn fixture_manifest() -> Manifest {
    Manifest {
        sequence: 7,
        db_uuid: [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54,
            0x32, 0x10,
        ],
        created_at_ms: 1_700_000_000_000,
        updated_at_ms: 1_700_000_123_456,
        collections: vec![
            CollectionEntry {
                name: "products".into(),
                segments: vec![
                    SegmentRef {
                        id: 1,
                        rows: 3,
                        del_generation: 0,
                    },
                    SegmentRef {
                        id: 4,
                        rows: 2,
                        del_generation: 2,
                    },
                ],
                index_snapshot: Some(11),
                last_applied_wal: 99,
                live_count: 4,
                total_rows: 5,
            },
            CollectionEntry {
                name: "zettel".into(),
                segments: vec![],
                index_snapshot: None,
                last_applied_wal: 0,
                live_count: 0,
                total_rows: 0,
            },
        ],
    }
}

fn fixture_value() -> Value {
    let mut inner = BTreeMap::new();
    inner.insert("plan".to_owned(), Value::Str("pro".into()));
    inner.insert("seats".to_owned(), Value::I64(-5));

    let mut root = BTreeMap::new();
    root.insert("active".to_owned(), Value::Bool(true));
    root.insert("blob".to_owned(), Value::Bytes(vec![0x00, 0xFF, 0x7F]));
    root.insert("missing".to_owned(), Value::Null);
    root.insert("price".to_owned(), Value::F64(19.99));
    root.insert(
        "tags".to_owned(),
        Value::Array(vec![
            Value::Str("a".into()),
            Value::Str("ünïcödé 🧭".into()),
        ]),
    );
    root.insert("user".to_owned(), Value::Map(inner));
    Value::Map(root)
}

/// Nine fields: above `INDEX_MIN_ENTRIES`, so this is written with an offset table while
/// `fixture_value`'s six-field root is not. Both encodings therefore have committed bytes.
fn fixture_wide_value() -> Value {
    let mut root = BTreeMap::new();
    for (i, key) in [
        "author", "brand", "colour", "depth", "edition", "format", "gtin", "height", "weight",
    ]
    .iter()
    .enumerate()
    {
        root.insert((*key).to_owned(), Value::I64(i as i64));
    }
    // A nested map that stays below the threshold, proving the choice is made per map and not
    // once for the whole document.
    let mut small = BTreeMap::new();
    small.insert("iso".to_owned(), Value::Str("GB".into()));
    root.insert("origin".to_owned(), Value::Map(small));
    Value::Map(root)
}

fn fixture_wal() -> Vec<WalFrame> {
    vec![
        WalFrame::standalone(
            1,
            WalOp::Put {
                id: b"doc-1".to_vec(),
                vector: 1.0f32
                    .to_le_bytes()
                    .iter()
                    .chain(2.0f32.to_le_bytes().iter())
                    .chain(3.0f32.to_le_bytes().iter())
                    .chain(4.0f32.to_le_bytes().iter())
                    .copied()
                    .collect(),
                metadata: fixture_value().encode().unwrap(),
                content: Some(b"the source text".to_vec()),
            },
        ),
        WalFrame::standalone(
            2,
            WalOp::Delete {
                id: b"doc-0".to_vec(),
            },
        ),
        WalFrame::in_txn(
            3,
            42,
            WalOp::Put {
                id: b"doc-2".to_vec(),
                vector: vec![0u8; 16],
                metadata: vec![],
                content: None,
            },
        ),
        WalFrame::in_txn(4, 42, WalOp::Commit { op_count: 1 }),
    ]
}

// ---------------------------------------------------------------------------
// The fixtures themselves
// ---------------------------------------------------------------------------

#[test]
fn catalog_bytes_are_stable() {
    golden("catalog.bin", &fixture_catalog().encode().unwrap());
}

#[test]
fn manifest_bytes_are_stable() {
    golden("manifest.bin", &fixture_manifest().encode().unwrap());
}

#[test]
fn value_bytes_are_stable() {
    golden("value.bin", &fixture_value().encode().unwrap());
}

#[test]
fn wide_value_bytes_are_stable() {
    golden("value-wide.bin", &fixture_wide_value().encode().unwrap());
}

#[test]
fn wal_bytes_are_stable() {
    let mut w = Writer::new();
    for frame in fixture_wal() {
        frame.append_to(&mut w).unwrap();
    }
    golden("wal.bin", w.as_slice());
}

#[test]
fn vector_block_bytes_are_stable() {
    let mut w = VectorBlockWriter::new(4, 16).unwrap();
    w.push_f32(&[1.0, 2.0, 3.0, 4.0]).unwrap();
    w.push_f32(&[-0.5, 0.0, 0.25, 1e-8]).unwrap();
    w.push_f32(&[f32::MIN, f32::MAX, 0.0, -0.0]).unwrap();
    golden("segment.vec", &w.finish());
}

/// The three records of the fixture segment, in row order.
fn fixture_records() -> [MetaRecord; 3] {
    [
        MetaRecord {
            metadata: Some(fixture_value()),
            content: None,
        },
        MetaRecord::default(),
        MetaRecord {
            metadata: None,
            content: Some(b"content only".to_vec()),
        },
    ]
}

/// The directory and the metadata block are written together, from the same records, so the
/// fixtures form one coherent segment rather than three unrelated files. Hand-written offsets
/// would drift the moment any encoding changed — and cross-file consistency is exactly what a
/// segment fixture should be proving.
fn fixture_segment() -> (Vec<u8>, Vec<u8>) {
    let ids: [&[u8]; 3] = [b"doc-1", b"doc-2", b"a-longer-document-identifier"];
    let norms = [0.182_574_18f32, 1.0, 0.5];

    let mut meta = MetaWriter::new();
    let mut dir = DirectoryWriter::new();
    for ((record, id), norm) in fixture_records().iter().zip(ids).zip(norms) {
        let (offset, len) = meta.push(record).unwrap();
        dir.push(id, offset, len, norm).unwrap();
    }
    (dir.finish(), meta.finish())
}

#[test]
fn directory_bytes_are_stable() {
    golden("segment.dir", &fixture_segment().0);
}

#[test]
fn meta_block_bytes_are_stable() {
    golden("segment.meta", &fixture_segment().1);
}

#[test]
fn tombstone_bytes_are_stable() {
    let mut t = Tombstones::all_live(130, 4);
    t.kill(0);
    t.kill(64);
    t.kill(129);
    golden("segment.del", &t.encode());
}

// ---------------------------------------------------------------------------
// Reading the fixtures back: the direction that actually matters to a user, because it is what
// happens when a new build opens a database written by an old one.
// ---------------------------------------------------------------------------

fn read_at(version: u16, name: &str) -> Vec<u8> {
    std::fs::read(testdata(version, name))
        .unwrap_or_else(|e| panic!("missing v{version} fixture {name}: {e}; run with VDB_BLESS=1"))
}

/// Every format version this build claims to read gets its own case, so dropping support for one
/// is a visible deletion rather than a silent gap.
#[test]
fn this_build_can_read_every_v1_fixture() {
    assert_fixtures_readable(1);
}

#[test]
fn this_build_can_read_every_v2_fixture() {
    assert_fixtures_readable(2);
}

/// The same assertions against whichever version's bytes: decoding must produce the identical
/// logical content regardless of how it was encoded. That is what backward compatibility *means*
/// here, and it is why these compare values rather than bytes.
fn assert_fixtures_readable(version: u16) {
    let read = |name: &str| read_at(version, name);
    assert_eq!(
        Catalog::decode(&read("catalog.bin")).unwrap(),
        fixture_catalog()
    );
    assert_eq!(
        Manifest::decode(&read("manifest.bin")).unwrap(),
        fixture_manifest()
    );
    assert_eq!(Value::decode(&read("value.bin")).unwrap(), fixture_value());
    // Added in v2 along with the indexed map encoding; v1 has no such file.
    if version >= 2 {
        assert_eq!(
            Value::decode(&read("value-wide.bin")).unwrap(),
            fixture_wide_value()
        );
        // The point of the table: every field reachable by path, including the last.
        for (i, key) in ["author", "gtin", "weight"].iter().enumerate() {
            let _ = i;
            assert!(
                isha_vector_db_format::find_path(&read("value-wide.bin"), key)
                    .unwrap()
                    .is_some(),
                "{key} must be reachable through the offset table"
            );
        }
        assert!(
            isha_vector_db_format::find_path(&read("value-wide.bin"), "origin.iso")
                .unwrap()
                .is_some()
        );
        assert!(
            isha_vector_db_format::find_path(&read("value-wide.bin"), "absent")
                .unwrap()
                .is_none()
        );
    }

    let wal = isha_vector_db_format::wal::scan(&read("wal.bin"));
    assert_eq!(wal.tail, isha_vector_db_format::WalTail::Clean);
    assert_eq!(wal.frames, fixture_wal());
    assert_eq!(wal.committed().len(), 3);

    let vec_bytes = read("segment.vec");
    let block = VectorBlock::open(&vec_bytes, 16).unwrap();
    assert_eq!(block.rows(), 3);
    assert_eq!(block.row_f32(0).unwrap(), vec![1.0, 2.0, 3.0, 4.0]);
    assert_eq!(
        block.row_f32(2).unwrap(),
        vec![f32::MIN, f32::MAX, 0.0, -0.0]
    );
    VectorBlock::verify(&vec_bytes).unwrap();

    let dir_bytes = read("segment.dir");
    let dir = Directory::open(&dir_bytes).unwrap();
    assert_eq!(dir.rows(), 3);
    assert_eq!(dir.id(0).unwrap(), b"doc-1");
    assert_eq!(dir.id(2).unwrap(), b"a-longer-document-identifier");

    // Every row's record must be reachable through its directory entry — the cross-file
    // consistency that makes these four files a segment rather than four blobs.
    let meta_bytes = read("segment.meta");
    let meta = MetaBlock::open(&meta_bytes).unwrap();
    for (row, expected) in fixture_records().iter().enumerate() {
        let entry = dir.entry(row as u32).unwrap();
        assert_eq!(&meta.record(&entry).unwrap(), expected, "row {row}");
    }
    assert_eq!(
        dir.entry(1).unwrap().meta_len,
        0,
        "an empty record occupies no bytes"
    );
    MetaBlock::verify(&meta_bytes).unwrap();

    let del = Tombstones::decode(&read("segment.del")).unwrap();
    assert_eq!(del.rows, 130);
    assert_eq!(del.generation, 4);
    assert_eq!(del.live_count(), 127);
    assert!(!del.is_live(0));
    assert!(del.is_live(1));
    assert!(!del.is_live(129));
}

/// A fixture must declare the version of the directory it sits in. If a structure silently
/// started writing a different version, the round-trip tests would still pass while old builds
/// broke — the bytes would be self-consistent and simply unreadable elsewhere.
#[test]
fn every_fixture_declares_the_version_of_its_directory() {
    for version in 1..=isha_vector_db_format::FORMAT_VERSION {
        assert_headers_declare(version);
    }
}

fn assert_headers_declare(version: u16) {
    for name in [
        "catalog.bin",
        "manifest.bin",
        "segment.vec",
        "segment.dir",
        "segment.meta",
        "segment.del",
    ] {
        let bytes = read_at(version, name);
        let header = isha_vector_db_format::FileHeader::decode_any(&bytes)
            .unwrap_or_else(|e| panic!("v{version} {name}: {e}"));
        assert_eq!(header.version, version, "{name}");
        assert_eq!(
            header.flags.bits(),
            0,
            "v{version} {name} must not be compressed or encrypted"
        );
    }
}
