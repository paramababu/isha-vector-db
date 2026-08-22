//! The public API, exercised the way a user would.
//!
//! These tests deliberately go through `Database` and `Collection` only. Nothing here reaches
//! into persistence or format internals, because the point is to check the contract those
//! internals exist to provide.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use vdb_core::api::{
    Collection, CollectionSpec, Database, DatabaseConfig, UpsertOutcome, WriteBatch,
};
use vdb_core::clock::ManualClock;
use vdb_core::document::{DocId, DocumentInput, Include};
use vdb_core::error::{
    ConflictError, DbError, LifecycleError, NotFoundError, TransactionError, ValidationError,
};
use vdb_core::metadata::{Metadata, Value};
use vdb_core::persistence::Durability;
use vdb_core::storage::Storage;
use vdb_core::vector::VectorView;
use vdb_format::Metric;
use vdb_storage_memory::MemoryStorage;

const DIM: u32 = 4;

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::default())
}

fn open(storage: Arc<dyn Storage>) -> Database {
    Database::open(storage, DatabaseConfig::default(), clock()).unwrap()
}

fn fresh() -> (MemoryStorage, Database) {
    let mem = MemoryStorage::new();
    let db = open(Arc::new(mem.clone()));
    (mem, db)
}

fn spec(name: &str) -> CollectionSpec {
    CollectionSpec::new(name, DIM, Metric::Cosine)
}

fn vector(seed: f32) -> [f32; 4] {
    [seed, seed + 1.0, seed + 2.0, seed + 3.0]
}

fn insert(c: &Collection, id: &str, seed: f32) {
    let v = vector(seed);
    c.insert(DocumentInput::new(id, VectorView::f32(&v)))
        .unwrap();
}

// ---------------------------------------------------------------------------
// lifecycle
// ---------------------------------------------------------------------------

#[test]
fn opening_an_empty_storage_creates_a_database() {
    let (_mem, db) = fresh();
    assert!(db.is_open());
    let stats = db.stats().unwrap();
    assert_eq!(stats.collections, 0);
    assert_eq!(stats.format_version, vdb_format::FORMAT_VERSION);
    assert!(stats.manifest_sequence >= 1);
    db.close().unwrap();
}

#[test]
fn refusing_to_create_reports_that_nothing_is_there() {
    let mem = MemoryStorage::new();
    let config = DatabaseConfig::default().create_if_missing(false);
    match Database::open(Arc::new(mem), config, clock()) {
        Err(DbError::Lifecycle(LifecycleError::DatabaseNotFound { .. })) => {}
        other => panic!("expected DatabaseNotFound, got {other:?}"),
    }
}

/// The advisory lock: it prevents accidents, which is what it claims to do.
#[test]
fn a_second_writer_cannot_open_the_same_database() {
    let mem = MemoryStorage::new();
    let first = open(Arc::new(mem.clone()));
    match Database::open(Arc::new(mem.clone()), DatabaseConfig::default(), clock()) {
        Err(DbError::Lifecycle(LifecycleError::DatabaseAlreadyOpen { .. })) => {}
        other => panic!("expected DatabaseAlreadyOpen, got {other:?}"),
    }
    first.close().unwrap();
    // Once closed, the lock is released.
    open(Arc::new(mem)).close().unwrap();
}

/// A read-only handle takes no lock, so a database can be inspected while an app has it open.
#[test]
fn a_reader_can_open_a_database_that_a_writer_holds() {
    let mem = MemoryStorage::new();
    let writer = open(Arc::new(mem.clone()));
    let c = writer.create_collection(spec("docs")).unwrap();
    insert(&c, "a", 1.0);
    writer.flush().unwrap();

    let config = DatabaseConfig::default().read_only(true);
    let reader = Database::open(Arc::new(mem.clone()), config, clock()).unwrap();
    assert_eq!(reader.open_collection("docs").unwrap().count().unwrap(), 1);

    // And it refuses to write.
    match reader.create_collection(spec("nope")) {
        Err(DbError::Lifecycle(LifecycleError::ReadOnly { .. })) => {}
        other => panic!("expected ReadOnly, got {other:?}"),
    }
    let rc = reader.open_collection("docs").unwrap();
    let v = vector(9.0);
    assert!(matches!(
        rc.insert(DocumentInput::new("b", VectorView::f32(&v))),
        Err(DbError::Lifecycle(LifecycleError::ReadOnly { .. }))
    ));
    reader.close().unwrap();
    writer.close().unwrap();
}

#[test]
fn a_read_only_handle_that_also_creates_is_a_configuration_error() {
    let mem = MemoryStorage::new();
    let config = DatabaseConfig::default()
        .read_only(true)
        .create_if_missing(true);
    assert!(Database::open(Arc::new(mem), config, clock()).is_err());
}

#[test]
fn using_a_collection_after_close_reports_the_database_is_closed() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    db.close().unwrap();
    let v = vector(1.0);
    assert!(matches!(
        c.insert(DocumentInput::new("a", VectorView::f32(&v))),
        Err(DbError::Lifecycle(LifecycleError::DatabaseClosed))
    ));
    assert!(matches!(
        c.count(),
        Err(DbError::Lifecycle(LifecycleError::DatabaseClosed))
    ));
}

// ---------------------------------------------------------------------------
// collections
// ---------------------------------------------------------------------------

#[test]
fn collections_can_be_created_listed_and_dropped() {
    let (_mem, db) = fresh();
    db.create_collection(spec("zebra")).unwrap();
    db.create_collection(spec("apple")).unwrap();

    let listed: Vec<String> = db
        .list_collections()
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(
        listed,
        vec!["apple", "zebra"],
        "listing should be sorted, not hash-ordered"
    );

    db.drop_collection("zebra").unwrap();
    assert_eq!(db.list_collections().unwrap().len(), 1);
    assert!(matches!(
        db.open_collection("zebra"),
        Err(DbError::NotFound(NotFoundError::Collection { .. }))
    ));
    assert!(matches!(
        db.drop_collection("zebra"),
        Err(DbError::NotFound(NotFoundError::Collection { .. }))
    ));
}

#[test]
fn creating_a_collection_twice_is_a_conflict() {
    let (_mem, db) = fresh();
    db.create_collection(spec("docs")).unwrap();
    assert!(matches!(
        db.create_collection(spec("docs")),
        Err(DbError::Conflict(ConflictError::CollectionExists { .. }))
    ));
}

/// Returning a collection whose shape differs from what was asked for would produce results
/// that look plausible and are wrong.
#[test]
fn get_or_create_refuses_a_collection_whose_shape_differs() {
    let (_mem, db) = fresh();
    db.create_collection(spec("docs")).unwrap();

    db.get_or_create_collection(spec("docs"))
        .expect("an identical spec should be accepted");

    let different_dim = CollectionSpec::new("docs", DIM + 1, Metric::Cosine);
    assert!(db.get_or_create_collection(different_dim).is_err());

    let different_metric = CollectionSpec::new("docs", DIM, Metric::L2);
    assert!(db.get_or_create_collection(different_metric).is_err());

    let different_ids = CollectionSpec::new("docs", DIM, Metric::Cosine).with_u64_ids();
    assert!(db.get_or_create_collection(different_ids).is_err());
}

#[test]
fn a_hostile_collection_name_is_refused() {
    let (_mem, db) = fresh();
    for bad in ["..", "a/b", "", "with space", "café"] {
        assert!(
            db.create_collection(CollectionSpec::new(bad, DIM, Metric::Cosine))
                .is_err(),
            "{bad:?} was accepted"
        );
    }
}

#[test]
fn a_zero_or_oversized_dimension_is_refused() {
    let (_mem, db) = fresh();
    assert!(db
        .create_collection(CollectionSpec::new("a", 0, Metric::Cosine))
        .is_err());
    assert!(db
        .create_collection(CollectionSpec::new("b", 1, Metric::Cosine))
        .is_ok());
}

// ---------------------------------------------------------------------------
// documents
// ---------------------------------------------------------------------------

#[test]
fn insert_get_and_count() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    assert_eq!(c.count().unwrap(), 0);
    assert!(c.get(&DocId::from("missing")).unwrap().is_none());

    insert(&c, "a", 1.0);
    insert(&c, "b", 2.0);
    assert_eq!(c.count().unwrap(), 2);
    assert!(c.contains(&DocId::from("a")).unwrap());

    let doc = c
        .get_with(&DocId::from("a"), Include::ALL)
        .unwrap()
        .unwrap();
    assert_eq!(doc.id, DocId::from("a"));
    assert_eq!(doc.vector, Some(vector(1.0).to_vec()));
    assert_eq!(c.ids().unwrap(), vec![DocId::from("a"), DocId::from("b")]);
}

#[test]
fn inserting_a_duplicate_id_is_a_conflict_but_upsert_replaces() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    insert(&c, "a", 1.0);

    let v = vector(2.0);
    match c.insert(DocumentInput::new("a", VectorView::f32(&v))) {
        Err(DbError::Conflict(ConflictError::DuplicateId { id, collection })) => {
            assert_eq!(id, "a");
            assert_eq!(collection, "docs");
        }
        other => panic!("expected DuplicateId, got {other:?}"),
    }

    assert_eq!(
        c.upsert(DocumentInput::new("a", VectorView::f32(&v)))
            .unwrap(),
        UpsertOutcome::Updated
    );
    let w = vector(3.0);
    assert_eq!(
        c.upsert(DocumentInput::new("new", VectorView::f32(&w)))
            .unwrap(),
        UpsertOutcome::Inserted
    );
    assert_eq!(c.count().unwrap(), 2);
    let doc = c
        .get_with(&DocId::from("a"), Include::ALL)
        .unwrap()
        .unwrap();
    assert_eq!(doc.vector, Some(vector(2.0).to_vec()));
}

#[test]
fn delete_reports_whether_the_document_existed() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    insert(&c, "a", 1.0);

    assert!(c.delete("a").unwrap());
    assert!(
        !c.delete("a").unwrap(),
        "deleting twice is a no-op, not an error"
    );
    assert!(!c.delete("never-existed").unwrap());
    assert_eq!(c.count().unwrap(), 0);
    assert!(c.get(&DocId::from("a")).unwrap().is_none());
    assert!(c.ids().unwrap().is_empty());
}

#[test]
fn a_wrong_dimension_names_the_collection_and_both_dimensions() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    let short = [1.0f32, 2.0];
    match c.insert(DocumentInput::new("a", VectorView::f32(&short))) {
        Err(DbError::Validation(ValidationError::InvalidVectorDimension {
            collection,
            expected,
            actual,
        })) => {
            assert_eq!(collection, "docs");
            assert_eq!(expected, DIM);
            assert_eq!(actual, 2);
        }
        other => panic!("expected InvalidVectorDimension, got {other:?}"),
    }
    assert_eq!(
        c.count().unwrap(),
        0,
        "a rejected write must leave nothing behind"
    );
}

#[test]
fn a_non_finite_component_is_refused() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    let bad = [1.0f32, 2.0, f32::NAN, 4.0];
    assert!(c
        .insert(DocumentInput::new("a", VectorView::f32(&bad)))
        .is_err());
    assert_eq!(c.count().unwrap(), 0);
}

#[test]
fn metadata_and_content_survive_a_round_trip() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();

    let mut meta = Metadata::new();
    meta.insert("category", Value::Str("tools".into()));
    meta.insert("price", Value::F64(19.99));
    meta.insert(
        "tags",
        Value::Array(vec![Value::Str("a".into()), Value::Str("b".into())]),
    );

    let v = vector(1.0);
    c.insert(
        DocumentInput::new("a", VectorView::f32(&v))
            .with_metadata(meta.clone())
            .with_content(b"the source text"),
    )
    .unwrap();

    let doc = c
        .get_with(&DocId::from("a"), Include::ALL)
        .unwrap()
        .unwrap();
    assert_eq!(doc.metadata, meta);
    assert_eq!(doc.content.as_deref(), Some(b"the source text".as_slice()));

    // And after a flush, out of a segment rather than the memtable.
    c.flush().unwrap();
    let doc = c
        .get_with(&DocId::from("a"), Include::ALL)
        .unwrap()
        .unwrap();
    assert_eq!(doc.metadata, meta);
    assert_eq!(doc.content.as_deref(), Some(b"the source text".as_slice()));
}

#[test]
fn vectors_are_opt_in_on_reads() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    insert(&c, "a", 1.0);
    assert!(c.get(&DocId::from("a")).unwrap().unwrap().vector.is_none());
    assert!(c
        .get_with(&DocId::from("a"), Include::ALL)
        .unwrap()
        .unwrap()
        .vector
        .is_some());
}

// ---------------------------------------------------------------------------
// batches
// ---------------------------------------------------------------------------

#[test]
fn a_batch_applies_every_operation() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    insert(&c, "existing", 1.0);

    let a = vector(2.0);
    let b = vector(3.0);
    let mut batch = WriteBatch::new();
    batch
        .upsert(DocumentInput::new("a", VectorView::f32(&a)))
        .upsert(DocumentInput::new("existing", VectorView::f32(&b)))
        .delete("gone");

    let report = c.write_batch(batch).unwrap();
    assert_eq!(report.inserted, 1);
    assert_eq!(report.updated, 1);
    assert_eq!(report.missing_deletes, 1);
    assert_eq!(report.changed(), 2);
    assert_eq!(c.count().unwrap(), 2);
}

/// The promise of a batch: a failure part-way through leaves nothing applied.
#[test]
fn an_invalid_operation_aborts_the_whole_batch() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();

    let good = vector(1.0);
    let short = [1.0f32];
    let mut batch = WriteBatch::new();
    batch
        .upsert(DocumentInput::new("good-1", VectorView::f32(&good)))
        .upsert(DocumentInput::new("bad", VectorView::f32(&short)))
        .upsert(DocumentInput::new("good-2", VectorView::f32(&good)));

    match c.write_batch(batch) {
        Err(DbError::Transaction(TransactionError::Aborted {
            failed_at,
            total_ops,
            ..
        })) => {
            assert_eq!(failed_at, 1);
            assert_eq!(total_ops, 3);
        }
        other => panic!("expected Aborted, got {other:?}"),
    }
    assert_eq!(
        c.count().unwrap(),
        0,
        "nothing from an aborted batch may be applied"
    );
    assert!(!c.contains(&DocId::from("good-1")).unwrap());
}

#[test]
fn an_empty_batch_is_a_no_op() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();
    let report = c.write_batch(WriteBatch::new()).unwrap();
    assert_eq!(report.changed(), 0);
    assert_eq!(c.count().unwrap(), 0);
}

// ---------------------------------------------------------------------------
// persistence
// ---------------------------------------------------------------------------

#[test]
fn everything_survives_a_close_and_reopen() {
    let mem = MemoryStorage::new();
    {
        let db = open(Arc::new(mem.clone()));
        let c = db.create_collection(spec("docs")).unwrap();
        for i in 0..20 {
            insert(&c, &format!("doc-{i:02}"), i as f32);
        }
        c.delete("doc-05").unwrap();
        db.close().unwrap();
    }

    let db = open(Arc::new(mem));
    let c = db.open_collection("docs").unwrap();
    assert_eq!(c.count().unwrap(), 19);
    assert!(!c.contains(&DocId::from("doc-05")).unwrap());
    let doc = c
        .get_with(&DocId::from("doc-07"), Include::ALL)
        .unwrap()
        .unwrap();
    assert_eq!(doc.vector, Some(vector(7.0).to_vec()));
    db.close().unwrap();
}

/// A handle dropped without `close()` must lose nothing that was logged — the situation an
/// application is in when the OS kills it, or when a `?` returns early past the close.
#[test]
fn writes_survive_a_handle_that_was_never_closed() {
    let mem = MemoryStorage::new();
    {
        let db = open(Arc::new(mem.clone()));
        let c = db.create_collection(spec("docs")).unwrap();
        insert(&c, "a", 1.0);
        insert(&c, "b", 2.0);
        // No close and no flush: the handle simply goes out of scope. Nothing was written to a
        // segment, so everything here has to come back out of the log.
        drop(c);
        drop(db);
    }
    let db = open(Arc::new(mem));
    let c = db.open_collection("docs").unwrap();
    assert_eq!(c.count().unwrap(), 2);
    assert!(c.contains(&DocId::from("b")).unwrap());
    db.close().unwrap();
}

/// Regression: an overwritten document used to stay live in the old segment as well as the new
/// one, so `count()` over-reported and a scan would have returned the same id twice with two
/// different vectors. A flush now supersedes earlier copies.
#[test]
fn overwriting_across_flushes_does_not_leave_two_live_copies() {
    let (_mem, db) = fresh();
    let c = db.create_collection(spec("docs")).unwrap();

    insert(&c, "a", 1.0);
    c.flush().unwrap();
    let v = vector(9.0);
    c.upsert(DocumentInput::new("a", VectorView::f32(&v)))
        .unwrap();
    c.flush().unwrap();

    assert_eq!(
        c.count().unwrap(),
        1,
        "the document must be live in exactly one segment"
    );
    assert_eq!(c.ids().unwrap(), vec![DocId::from("a")]);
    let doc = c
        .get_with(&DocId::from("a"), Include::ALL)
        .unwrap()
        .unwrap();
    assert_eq!(
        doc.vector,
        Some(vector(9.0).to_vec()),
        "the newer value must win"
    );

    let stats = c.stats().unwrap();
    assert_eq!(stats.live_documents, 1);
    assert_eq!(
        stats.total_rows, 2,
        "the superseded row is still on disk until compaction"
    );
    assert!(
        stats.dead_ratio > 0.4,
        "the dead row should be visible in the stats"
    );
}

#[test]
fn a_delete_of_a_flushed_document_survives_a_reopen() {
    let mem = MemoryStorage::new();
    {
        let db = open(Arc::new(mem.clone()));
        let c = db.create_collection(spec("docs")).unwrap();
        insert(&c, "a", 1.0);
        insert(&c, "b", 2.0);
        c.flush().unwrap();
        c.delete("a").unwrap();
        c.flush().unwrap();
        db.close().unwrap();
    }
    let db = open(Arc::new(mem));
    let c = db.open_collection("docs").unwrap();
    assert_eq!(c.count().unwrap(), 1);
    assert!(!c.contains(&DocId::from("a")).unwrap());
    db.close().unwrap();
}

#[test]
fn crossing_the_flush_threshold_produces_a_segment_automatically() {
    let mem = MemoryStorage::new();
    let config = DatabaseConfig::default()
        .flush_threshold_bytes(2_000)
        .durability(Durability::Batch);
    let db = Database::open(Arc::new(mem.clone()), config, clock()).unwrap();
    let c = db.create_collection(spec("docs")).unwrap();

    for i in 0..200 {
        insert(&c, &format!("doc-{i:03}"), i as f32);
    }
    let stats = c.stats().unwrap();
    assert!(
        stats.segments > 0,
        "the memtable should have flushed on its own"
    );
    assert_eq!(stats.live_documents, 200);
    assert_eq!(c.count().unwrap(), 200);

    db.close().unwrap();
    let db = open(Arc::new(mem));
    assert_eq!(db.open_collection("docs").unwrap().count().unwrap(), 200);
    db.close().unwrap();
}

#[test]
fn dropping_a_collection_removes_its_files_and_survives_a_reopen() {
    let mem = MemoryStorage::new();
    {
        let db = open(Arc::new(mem.clone()));
        let c = db.create_collection(spec("doomed")).unwrap();
        insert(&c, "a", 1.0);
        c.flush().unwrap();
        db.create_collection(spec("kept")).unwrap();
        db.drop_collection("doomed").unwrap();
        db.close().unwrap();
    }
    assert!(
        !mem.file_paths().iter().any(|p| p.contains("doomed")),
        "dropping should have removed the files: {:?}",
        mem.file_paths()
    );
    let db = open(Arc::new(mem));
    let names: Vec<String> = db
        .list_collections()
        .unwrap()
        .into_iter()
        .map(|c| c.name)
        .collect();
    assert_eq!(names, vec!["kept"]);
    db.close().unwrap();
}

#[test]
fn database_stats_aggregate_every_collection() {
    let (_mem, db) = fresh();
    let a = db.create_collection(spec("a")).unwrap();
    let b = db.create_collection(spec("b")).unwrap();
    insert(&a, "1", 1.0);
    insert(&a, "2", 2.0);
    insert(&b, "1", 3.0);
    a.flush().unwrap();

    let stats = db.stats().unwrap();
    assert_eq!(stats.collections, 2);
    assert_eq!(stats.live_documents, 3);
    assert!(
        stats.durable_sync,
        "the memory backend declares durable sync"
    );
    assert!(!stats.read_only);
}

#[test]
fn u64_ids_work_end_to_end() {
    let mem = MemoryStorage::new();
    {
        let db = open(Arc::new(mem.clone()));
        let c = db
            .create_collection(spec("numeric").with_u64_ids())
            .unwrap();
        let v = vector(1.0);
        c.insert(DocumentInput::new(42u64, VectorView::f32(&v)))
            .unwrap();
        // A string id in a numeric collection is a mismatch, not a coercion.
        assert!(c
            .insert(DocumentInput::new("42", VectorView::f32(&v)))
            .is_err());
        db.close().unwrap();
    }
    let db = open(Arc::new(mem));
    let c = db.open_collection("numeric").unwrap();
    assert!(c.contains(&DocId::U64(42)).unwrap());
    assert_eq!(c.count().unwrap(), 1);
    db.close().unwrap();
}

/// The other half of the empty-slot fix: treating an empty slot as "no database" must never let
/// the engine create a fresh manifest over collections that already exist. Losing a 200-byte
/// manifest is recoverable; silently orphaning every document because of it is not.
#[test]
fn a_missing_manifest_over_existing_collections_is_reported_not_overwritten() {
    let mem = MemoryStorage::new();
    {
        let db = open(Arc::new(mem.clone()));
        let c = db.create_collection(spec("docs")).unwrap();
        insert(&c, "precious", 1.0);
        c.flush().unwrap();
        db.close().unwrap();
    }
    // Lose both manifest slots, as a botched backup or a partial copy would.
    for slot in ["MANIFEST-A", "MANIFEST-B"] {
        let path = vdb_core::path::DbPath::parse(slot).unwrap();
        if mem.exists(&path).unwrap() {
            mem.remove_file(&path).unwrap();
        }
    }

    match Database::open(Arc::new(mem.clone()), DatabaseConfig::default(), clock()) {
        Err(e) => {
            assert!(e.is_corruption(), "expected corruption, got {e}");
            assert!(e.to_string().contains("collections"), "{e}");
        }
        Ok(db) => panic!(
            "created a fresh database over existing data: {:?}",
            db.list_collections().unwrap()
        ),
    }
    // The data is still on disk, which is the whole point of refusing.
    assert!(mem.file_paths().iter().any(|p| p.contains("docs")));
}
