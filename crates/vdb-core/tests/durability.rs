//! The full durability loop: write, flush, commit, reopen — and crash at every step of it.
//!
//! Step 9's sweep covered the log alone. This one covers the whole cycle, which is where the
//! interesting orderings are: a crash between writing a segment and committing the manifest
//! that names it, or between committing and checkpointing the log.
//!
//! The harness here is a deliberately minimal stand-in for `Database::open`, which is the next
//! step. Its job is to prove the persistence pieces compose into something recoverable before a
//! public API is built on top of them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeMap;
use std::sync::Arc;

use vdb_core::document::{DocId, Include};
use vdb_core::path::DbPath;
use vdb_core::persistence::segment::{
    flush_memtable, list_segment_ids, read_catalog, remove_segment, write_catalog, SegmentData,
};
use vdb_core::persistence::{layout, replay_into, wal::WalWriter, Durability, ManifestStore};
use vdb_core::storage::Storage;
use vdb_core::vector::{VectorDType, VectorView};
use vdb_core::write::Memtable;
use vdb_format::{
    Catalog, CollectionEntry, IdKind, IndexSpec, Metric, SegmentRef, VectorDType as FmtDType, WalOp,
};
use vdb_storage_memory::MemoryStorage;
use vdb_testkit::{Fault, FaultyStorage};

const NAME: &str = "products";
const DIM: u32 = 2;
const ID_KIND: IdKind = IdKind::Str { max_len: 64 };

fn catalog() -> Catalog {
    Catalog {
        name: NAME.to_owned(),
        dimension: DIM,
        metric: Metric::Cosine,
        dtype: FmtDType::F32,
        id_kind: ID_KIND,
        index: IndexSpec::Flat,
        created_at_ms: 1_000,
    }
}

fn put(id: &str, values: [f32; 2]) -> WalOp {
    WalOp::Put {
        id: id.as_bytes().to_vec(),
        vector: VectorView::f32(&values).to_bytes(),
        metadata: vec![],
        content: None,
    }
}

fn wal_path() -> DbPath {
    layout::wal_file(NAME, 1).unwrap()
}

/// Everything a reopen reconstructs.
struct Recovered {
    /// Live documents, id to vector.
    live: BTreeMap<String, Vec<f32>>,
    /// Segment files present on disk but named by no manifest.
    orphans: Vec<u64>,
}

/// Create a database: catalog, first manifest, empty collection entry.
fn create(storage: &dyn Storage) -> vdb_core::Result<()> {
    storage.create_dir_all(&layout::collections_dir()?)?;
    write_catalog(storage, &catalog())?;
    storage.create_dir_all(&layout::wal_dir(NAME)?)?;
    let mut store = ManifestStore::create(storage, [1u8; 16], 1_000)?;
    let mut m = store.current().clone();
    m.collections.push(CollectionEntry {
        name: NAME.to_owned(),
        segments: vec![],
        index_snapshot: None,
        last_applied_wal: 0,
        live_count: 0,
        total_rows: 0,
    });
    store.commit(storage, m, 1_001)
}

/// Reopen: read the manifest, open its segments, replay the log over them, clean up orphans.
///
/// This is the sequence `Database::open` will perform, in the order §5.6 specifies.
fn reopen(storage: &dyn Storage, clean_orphans: bool) -> vdb_core::Result<Recovered> {
    let Some(store) = ManifestStore::load(storage)? else {
        return Ok(Recovered {
            live: BTreeMap::new(),
            orphans: vec![],
        });
    };
    let manifest = store.current();
    let Some(entry) = manifest.collection(NAME) else {
        return Ok(Recovered {
            live: BTreeMap::new(),
            orphans: vec![],
        });
    };
    let cat = read_catalog(storage, NAME)?;

    // Segment files present but unreferenced are aborted flushes.
    let referenced: Vec<u64> = entry.segments.iter().map(|s| s.id).collect();
    let orphans: Vec<u64> = list_segment_ids(storage, NAME)?
        .into_iter()
        .filter(|id| !referenced.contains(id))
        .collect();
    if clean_orphans {
        for id in &orphans {
            remove_segment(storage, NAME, *id)?;
        }
    }

    let mut live: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    for seg_ref in &entry.segments {
        let seg = SegmentData::open(storage, &cat, seg_ref)?;
        for row in 0..seg.rows() {
            if let Some(doc) = seg.document(row, Include::ALL)? {
                live.insert(doc.id.display(), doc.vector.unwrap_or_default());
            }
        }
    }

    // The log is replayed *over* the segments. Replaying frames already folded into a segment
    // is harmless because both operations are idempotent — which is exactly what makes it safe
    // to commit the manifest before checkpointing the log.
    let mut memtable = Memtable::new(cat.dimension, VectorDType::F32);
    replay_into(storage, &wal_path(), ID_KIND, &mut memtable)?;
    for row in memtable.live_rows() {
        let bytes = memtable.vector_bytes(row).unwrap();
        let v = VectorView::raw(VectorDType::F32, bytes, cat.dimension)?.to_f32();
        live.insert(row.id.display(), v);
    }
    for id in memtable.deleted_ids() {
        live.remove(&id.display());
    }

    Ok(Recovered { live, orphans })
}

/// Write some documents to the log, then fold them into a segment and commit.
fn write_and_flush(
    storage: &dyn Storage,
    ops: Vec<WalOp>,
    segment_id: u64,
) -> vdb_core::Result<()> {
    let cat = read_catalog(storage, NAME)?;

    let mut w = WalWriter::open(storage, &wal_path(), 1, Durability::Batch)?;
    for op in &ops {
        w.append(op.clone())?;
    }
    w.sync()?;

    // Rebuild the memtable from the log, exactly as recovery would, so the flush cannot depend
    // on in-memory state a crashed process would not have had.
    let mut memtable = Memtable::new(cat.dimension, VectorDType::F32);
    replay_into(storage, &wal_path(), ID_KIND, &mut memtable)?;

    let result = flush_memtable(storage, &cat, segment_id, &memtable)?;

    let mut store = ManifestStore::load(storage)?.expect("a manifest must exist");
    let mut manifest = store.current().clone();
    let mut segments: Vec<SegmentRef> = manifest
        .collection(NAME)
        .map(|c| c.segments.clone())
        .unwrap_or_default();

    // Apply deletions to the segments that actually hold the rows.
    for id in &result.pending_deletions {
        for seg_ref in segments.iter_mut() {
            let mut seg = SegmentData::open(storage, &cat, seg_ref)?;
            if let Some(row) = seg.row_of(id) {
                if seg.kill(row) {
                    seg_ref.del_generation = seg.persist_tombstones(storage, NAME)?;
                }
            }
        }
    }
    if result.segment.rows > 0 {
        segments.push(result.segment);
    } else {
        remove_segment(storage, NAME, segment_id)?;
    }

    let total_rows: u64 = segments.iter().map(|s| u64::from(s.rows)).sum();
    let mut live_count = 0u64;
    for seg_ref in &segments {
        live_count += u64::from(SegmentData::open(storage, &cat, seg_ref)?.live_count());
    }
    manifest.collections = vec![CollectionEntry {
        name: NAME.to_owned(),
        segments,
        index_snapshot: None,
        last_applied_wal: 0,
        live_count,
        total_rows,
    }];
    store.commit(storage, manifest, 2_000)?;

    // Checkpoint last. A crash before this leaves the log holding frames already in the
    // segment, which replay reapplies harmlessly.
    let mut f = storage.open_file(&wal_path(), vdb_core::storage::OpenMode::ReadWrite)?;
    f.truncate(0)?;
    f.sync_data()
}

// ---------------------------------------------------------------------------

#[test]
fn write_flush_and_reopen_returns_what_was_written() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    write_and_flush(&mem, vec![put("a", [1.0, 0.0]), put("b", [0.0, 1.0])], 1).unwrap();

    let r = reopen(&mem, false).unwrap();
    assert_eq!(r.live.len(), 2);
    assert_eq!(r.live["a"], vec![1.0, 0.0]);
    assert_eq!(r.live["b"], vec![0.0, 1.0]);
    assert!(r.orphans.is_empty());

    // And the log was checkpointed, so a second reopen does not double-count.
    assert_eq!(reopen(&mem, false).unwrap().live.len(), 2);
}

#[test]
fn several_flushes_accumulate_into_several_segments() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    write_and_flush(&mem, vec![put("a", [1.0, 0.0])], 1).unwrap();
    write_and_flush(&mem, vec![put("b", [0.0, 1.0])], 2).unwrap();
    write_and_flush(&mem, vec![put("c", [1.0, 1.0])], 3).unwrap();

    let store = ManifestStore::load(&mem).unwrap().unwrap();
    let entry = store.current().collection(NAME).unwrap();
    assert_eq!(entry.segments.len(), 3);
    assert_eq!(entry.total_rows, 3);
    assert_eq!(entry.live_count, 3);
    assert_eq!(reopen(&mem, false).unwrap().live.len(), 3);
}

/// A delete of a document living in an older segment must reach that segment's bitmap.
#[test]
fn a_delete_reaches_a_document_in_an_earlier_segment() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    write_and_flush(&mem, vec![put("a", [1.0, 0.0]), put("b", [0.0, 1.0])], 1).unwrap();
    write_and_flush(&mem, vec![WalOp::Delete { id: b"a".to_vec() }], 2).unwrap();

    let r = reopen(&mem, false).unwrap();
    assert_eq!(r.live.keys().collect::<Vec<_>>(), vec!["b"]);

    let store = ManifestStore::load(&mem).unwrap().unwrap();
    let entry = store.current().collection(NAME).unwrap();
    assert_eq!(entry.live_count, 1);
    assert_eq!(
        entry.total_rows, 2,
        "the dead row is still on disk until compaction"
    );
    assert_eq!(
        entry.segments[0].del_generation, 1,
        "the bitmap was rewritten"
    );
}

#[test]
fn overwriting_a_document_in_an_earlier_segment_returns_the_newer_value() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    write_and_flush(&mem, vec![put("a", [1.0, 0.0])], 1).unwrap();
    write_and_flush(&mem, vec![put("a", [9.0, 9.0])], 2).unwrap();

    let r = reopen(&mem, false).unwrap();
    assert_eq!(r.live["a"], vec![9.0, 9.0], "the later segment must win");
}

/// The sweep, now over the whole cycle rather than the log alone.
#[test]
fn crashing_anywhere_in_the_write_flush_commit_cycle_stays_recoverable() {
    // Two legal outcomes: the flush committed, or it did not.
    let before: BTreeMap<String, Vec<f32>> =
        [("a".to_owned(), vec![1.0, 0.0])].into_iter().collect();
    let after: BTreeMap<String, Vec<f32>> = [
        ("a".to_owned(), vec![1.0, 0.0]),
        ("b".to_owned(), vec![0.0, 1.0]),
    ]
    .into_iter()
    .collect();

    // Size the sweep against a clean run.
    let probe = MemoryStorage::new();
    let counting = FaultyStorage::counting(Arc::new(probe.clone()));
    create(&counting).unwrap();
    write_and_flush(&counting, vec![put("a", [1.0, 0.0])], 1).unwrap();
    let base = counting.op_count();
    write_and_flush(&counting, vec![put("b", [0.0, 1.0])], 2).unwrap();
    let total = counting.op_count();
    assert!(
        total - base > 8,
        "the flush cycle should touch several files: {}",
        total - base
    );

    for index in base..total {
        let mem = MemoryStorage::new();
        create(&mem).unwrap();
        write_and_flush(&mem, vec![put("a", [1.0, 0.0])], 1).unwrap();

        let faulty = FaultyStorage::failing_at(Arc::new(mem.clone()), index - base, Fault::Crash);
        let _ = write_and_flush(&faulty, vec![put("b", [0.0, 1.0])], 2);

        let r = reopen(&mem, true).unwrap_or_else(|e| {
            panic!(
                "crash at cycle operation {} was unrecoverable: {e}",
                index - base
            )
        });
        assert!(
            r.live == before || r.live == after,
            "crash at cycle operation {}: recovered {:?}, expected one of {:?} / {:?}",
            index - base,
            r.live,
            before,
            after
        );

        // Whatever happened, the database must still be usable afterwards.
        write_and_flush(&mem, vec![put("c", [2.0, 2.0])], 3)
            .unwrap_or_else(|e| panic!("database unusable after a crash at {}: {e}", index - base));
        assert!(reopen(&mem, true).unwrap().live.contains_key("c"));
    }
}

/// A crash between writing a segment and committing the manifest leaves files nothing points
/// at. They must be removed, not left to accumulate or — worse — be picked up by a later flush
/// that reuses the id.
#[test]
fn an_aborted_flush_leaves_orphan_files_that_reopening_removes() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    write_and_flush(&mem, vec![put("a", [1.0, 0.0])], 1).unwrap();

    // Write a segment's files without ever committing a manifest naming it.
    let cat = read_catalog(&mem, NAME).unwrap();
    let mut memtable = Memtable::new(DIM, VectorDType::F32);
    memtable
        .put_view(
            DocId::from("ghost"),
            VectorView::f32(&[7.0, 7.0]),
            None,
            None,
        )
        .unwrap();
    flush_memtable(&mem, &cat, 99, &memtable).unwrap();
    assert!(list_segment_ids(&mem, NAME).unwrap().contains(&99));

    let r = reopen(&mem, true).unwrap();
    assert_eq!(r.orphans, vec![99]);
    assert!(
        !r.live.contains_key("ghost"),
        "an uncommitted segment must be invisible"
    );
    assert!(
        !list_segment_ids(&mem, NAME).unwrap().contains(&99),
        "the orphan should have been cleaned up"
    );
}

/// A segment the manifest promises but which is not on disk is data loss, and must be reported
/// rather than quietly skipped. Silently opening a database with a missing segment would show
/// the user a collection that has lost documents with no indication anything went wrong.
#[test]
fn a_missing_segment_is_reported_not_skipped() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    write_and_flush(&mem, vec![put("a", [1.0, 0.0]), put("b", [0.0, 1.0])], 1).unwrap();

    mem.remove_file(&layout::segment_file(NAME, 1, layout::SegmentFile::Vectors).unwrap())
        .unwrap();

    match reopen(&mem, false) {
        Err(e) => {
            assert!(e.is_corruption(), "expected corruption, got {e}");
            assert!(
                e.to_string().contains("products"),
                "should name the collection: {e}"
            );
        }
        Ok(r) => panic!("a missing segment was skipped, recovering {:?}", r.live),
    }
}

/// The row count in the manifest, the directory and the bitmap must agree, or every row index
/// means something different depending on which file answered.
#[test]
fn a_segment_whose_files_disagree_about_row_count_is_rejected() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    write_and_flush(&mem, vec![put("a", [1.0, 0.0]), put("b", [0.0, 1.0])], 1).unwrap();

    let mut store = ManifestStore::load(&mem).unwrap().unwrap();
    let mut manifest = store.current().clone();
    manifest.collections[0].segments[0].rows = 5;
    manifest.collections[0].total_rows = 5;
    store.commit(&mem, manifest, 3_000).unwrap();

    match reopen(&mem, false) {
        Err(e) => assert!(e.is_corruption(), "expected corruption, got {e}"),
        Ok(r) => panic!(
            "an inconsistent segment was accepted, recovering {:?}",
            r.live
        ),
    }
}

#[test]
fn a_flushed_segment_reads_back_its_metadata_and_content() {
    use vdb_core::metadata::{Metadata, Value};

    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    let cat = read_catalog(&mem, NAME).unwrap();

    let mut meta = Metadata::new();
    meta.insert("category", Value::Str("tools".into()));
    meta.insert("price", Value::F64(19.99));

    let mut memtable = Memtable::new(DIM, VectorDType::F32);
    memtable
        .put_view(
            DocId::from("full"),
            VectorView::f32(&[3.0, 4.0]),
            Some(meta.clone()),
            Some(b"the source text".to_vec()),
        )
        .unwrap();
    memtable
        .put_view(
            DocId::from("bare"),
            VectorView::f32(&[1.0, 0.0]),
            None,
            None,
        )
        .unwrap();
    let result = flush_memtable(&mem, &cat, 1, &memtable).unwrap();

    let seg = SegmentData::open(&mem, &cat, &result.segment).unwrap();
    assert_eq!(seg.rows(), 2);
    assert_eq!(seg.live_count(), 2);

    let row = seg.row_of(&DocId::from("full")).unwrap();
    let doc = seg.document(row, Include::ALL).unwrap().unwrap();
    assert_eq!(doc.metadata, meta);
    assert_eq!(doc.content.as_deref(), Some(b"the source text".as_slice()));
    assert_eq!(doc.vector, Some(vec![3.0, 4.0]));
    assert!((seg.inv_norm(row).unwrap() - 0.2).abs() < 1e-6);

    // A bare document reads back with empty metadata, not a failure.
    let bare_row = seg.row_of(&DocId::from("bare")).unwrap();
    let bare = seg.document(bare_row, Include::ALL).unwrap().unwrap();
    assert!(bare.metadata.is_empty());
    assert_eq!(bare.content, None);

    // Only what was asked for comes back.
    let lean = seg.document(row, Include::NONE).unwrap().unwrap();
    assert!(lean.vector.is_none());
    assert!(lean.metadata.is_empty());
}

#[test]
fn a_dead_row_cannot_be_read_back_by_index() {
    let mem = MemoryStorage::new();
    create(&mem).unwrap();
    let cat = read_catalog(&mem, NAME).unwrap();

    let mut memtable = Memtable::new(DIM, VectorDType::F32);
    memtable
        .put_view(
            DocId::from("doomed"),
            VectorView::f32(&[1.0, 1.0]),
            None,
            None,
        )
        .unwrap();
    let result = flush_memtable(&mem, &cat, 1, &memtable).unwrap();

    let mut seg = SegmentData::open(&mem, &cat, &result.segment).unwrap();
    let row = seg.row_of(&DocId::from("doomed")).unwrap();
    assert!(seg.document(row, Include::ALL).unwrap().is_some());
    assert!(seg.kill(row));
    assert!(
        seg.document(row, Include::ALL).unwrap().is_none(),
        "a deleted document must not be resurrectable by row index"
    );
    assert_eq!(seg.live_count(), 0);
}
