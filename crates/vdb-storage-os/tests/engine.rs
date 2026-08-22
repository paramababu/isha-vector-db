//! The engine, on a real filesystem.
//!
//! Everything here has an equivalent that already passes against `MemoryStorage`. Running the
//! same scenarios against a real disk is the check that the storage abstraction actually
//! abstracts — that the engine has not quietly grown a dependency on in-memory semantics.
//!
//! What cannot be replicated here is power loss: a real filesystem has no `simulate_power_loss`,
//! so the crash sweep below injects process death only. That is the failure mobile applications
//! actually meet, and the power-loss half of the sweep stays with the in-memory backend where it
//! can be modelled honestly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use vdb_core::api::{Collection, CollectionSpec, Database, DatabaseConfig, SearchRequest};
use vdb_core::clock::ManualClock;
use vdb_core::document::{DocId, DocumentInput, Include};
use vdb_core::error::{DbError, LifecycleError};
use vdb_core::filter::Filter;
use vdb_core::metadata::{Metadata, Value};
use vdb_core::storage::Storage;
use vdb_core::vector::VectorView;
use vdb_core::Metric;
use vdb_storage_os::OsStorage;
use vdb_testkit::{Fault, FaultyStorage};

#[derive(Debug)]
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "vdb-engine-{label}-{}-{unique}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn open(dir: &TempDir) -> Database {
    Database::open(
        Arc::new(OsStorage::open(dir.path()).unwrap()),
        DatabaseConfig::default(),
        Arc::new(ManualClock::default()),
    )
    .unwrap()
}

fn docs(db: &Database) -> Collection {
    db.create_collection(CollectionSpec::new("docs", 2, Metric::Cosine))
        .unwrap()
}

fn add(c: &Collection, id: &str, v: [f32; 2]) {
    c.insert(DocumentInput::new(id, VectorView::f32(&v)))
        .unwrap();
}

#[test]
fn the_full_lifecycle_works_on_a_real_filesystem() {
    let dir = TempDir::new("lifecycle");
    {
        let db = open(&dir);
        let c = docs(&db);
        for i in 0..50 {
            let angle = i as f32 * core::f32::consts::TAU / 50.0;
            add(&c, &format!("doc-{i:02}"), [angle.cos(), angle.sin()]);
        }
        c.delete("doc-10").unwrap();
        c.flush().unwrap();
        add(&c, "buffered", [1.0, 0.0]);
        db.close().unwrap();
    }

    let db = open(&dir);
    let c = db.open_collection("docs").unwrap();
    assert_eq!(c.count().unwrap(), 50, "49 flushed plus one buffered");
    assert!(!c.contains(&DocId::from("doc-10")).unwrap());

    let r = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 3))
        .unwrap();
    assert_eq!(r.hits[0].id.display(), "buffered");
    assert!(r.stats.exact);
    db.close().unwrap();

    // The files are really on disk, where an operator could look at them.
    assert!(dir.path().join("MANIFEST-A").exists() || dir.path().join("MANIFEST-B").exists());
    assert!(dir.path().join("collections/docs/CATALOG").exists());
    assert!(dir.path().join("collections/docs/segments").is_dir());
}

#[test]
fn search_and_filters_behave_identically_on_disk() {
    let dir = TempDir::new("filters");
    let db = open(&dir);
    let c = docs(&db);

    let mut meta = Metadata::new();
    meta.insert("kind", Value::Str("tool".into()));
    c.insert(
        DocumentInput::new("hammer", VectorView::f32(&[1.0, 0.0])).with_metadata(meta.clone()),
    )
    .unwrap();
    add(&c, "ball", [0.7, 0.7]);
    c.flush().unwrap();

    let tools = Filter::eq("kind", Value::Str("tool".into()));
    let r = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 5).with_filter(&tools))
        .unwrap();
    assert_eq!(r.ids(), vec![DocId::from("hammer")]);
    assert_eq!(r.stats.skipped, 1);

    let full = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 1).with_include(Include::ALL))
        .unwrap();
    assert_eq!(full.hits[0].document.as_ref().unwrap().metadata, meta);
    db.close().unwrap();
}

/// The lock is what stops two processes from writing to one database. On a real filesystem it
/// is a real `flock`, so this exercises the actual mechanism rather than an in-memory stand-in.
#[cfg(unix)]
#[test]
fn a_second_database_handle_cannot_open_the_same_directory() {
    let dir = TempDir::new("lock");
    let first = open(&dir);
    match Database::open(
        Arc::new(OsStorage::open(dir.path()).unwrap()),
        DatabaseConfig::default(),
        Arc::new(ManualClock::default()),
    ) {
        Err(DbError::Lifecycle(LifecycleError::DatabaseAlreadyOpen { .. })) => {}
        other => panic!("expected DatabaseAlreadyOpen, got {other:?}"),
    }
    first.close().unwrap();
    open(&dir).close().unwrap();
}

/// A handle dropped without `close()` must release its lock, or an application killed by the
/// operating system could never reopen its own database.
#[cfg(unix)]
#[test]
fn a_dropped_handle_releases_the_lock() {
    let dir = TempDir::new("lock-drop");
    {
        let db = open(&dir);
        let c = docs(&db);
        add(&c, "a", [1.0, 0.0]);
        // No close: the handle simply goes out of scope.
    }
    let db = open(&dir);
    assert_eq!(db.open_collection("docs").unwrap().count().unwrap(), 1);
    db.close().unwrap();
}

/// The crash sweep, on a real filesystem. Same driver as the in-memory one, minus the power-loss
/// variant that a real disk cannot simulate.
#[test]
fn crashing_at_every_io_operation_stays_recoverable_on_disk() {
    // Size the sweep against a clean run.
    let probe = TempDir::new("sweep-probe");
    let counting = FaultyStorage::counting(Arc::new(OsStorage::open(probe.path()).unwrap()));
    workload(Arc::new(counting.clone()));
    let total = counting.op_count();
    assert!(total > 20, "the workload should touch many files: {total}");

    // Sampled rather than exhaustive: each iteration creates a real directory and syncs real
    // files, so all of them would take minutes. The in-memory sweep runs every index on every
    // push; this one confirms the same protocol holds against a real disk.
    let step = (total / 24).max(1);
    for index in (0..total).step_by(step as usize) {
        let dir = TempDir::new(&format!("sweep-{index}"));
        let storage = Arc::new(OsStorage::open(dir.path()).unwrap());
        let faulty = FaultyStorage::failing_at(storage, index, Fault::Crash);
        workload(Arc::new(faulty));

        // Reopen through a clean handle: the process "died", but what reached the disk is there.
        let db = Database::open(
            Arc::new(OsStorage::open(dir.path()).unwrap()),
            DatabaseConfig::default(),
            Arc::new(ManualClock::default()),
        )
        .unwrap_or_else(|e| panic!("crash at operation {index} left an unopenable database: {e}"));

        if let Ok(c) = db.open_collection("docs") {
            let count = c
                .count()
                .unwrap_or_else(|e| panic!("crash at {index}: collection unreadable: {e}"));
            assert!(count <= 3, "crash at {index}: impossible count {count}");
            // Whatever survived, the database must still be usable.
            add(&c, "after", [0.0, 1.0]);
            c.flush().unwrap();
            assert!(c.contains(&DocId::from("after")).unwrap());
        }
        db.close().unwrap();
    }
}

/// Best-effort: errors are expected, since the point is to interrupt it.
fn workload(storage: Arc<dyn Storage>) {
    let Ok(db) = Database::open(
        storage,
        DatabaseConfig::default(),
        Arc::new(ManualClock::default()),
    ) else {
        return;
    };
    if let Ok(c) = db.create_collection(CollectionSpec::new("docs", 2, Metric::Cosine)) {
        let _ = c.insert(DocumentInput::new("a", VectorView::f32(&[1.0, 0.0])));
        let _ = c.insert(DocumentInput::new("b", VectorView::f32(&[0.0, 1.0])));
        let _ = c.flush();
        let _ = c.insert(DocumentInput::new("c", VectorView::f32(&[1.0, 1.0])));
        let _ = c.delete("a");
        let _ = c.flush();
    }
    let _ = db.close();
}

#[test]
fn many_collections_and_segments_survive_a_reopen() {
    let dir = TempDir::new("scale");
    {
        let db = Database::open(
            Arc::new(OsStorage::open(dir.path()).unwrap()),
            DatabaseConfig::default().flush_threshold_bytes(1024),
            Arc::new(ManualClock::default()),
        )
        .unwrap();
        for name in ["alpha", "beta", "gamma"] {
            let c = db
                .create_collection(CollectionSpec::new(name, 4, Metric::L2))
                .unwrap();
            for i in 0..120 {
                let v = [i as f32, 1.0, 2.0, 3.0];
                c.insert(DocumentInput::new(
                    format!("{name}-{i:03}"),
                    VectorView::f32(&v),
                ))
                .unwrap();
            }
        }
        db.close().unwrap();
    }
    let db = open(&dir);
    assert_eq!(db.list_collections().unwrap().len(), 3);
    for name in ["alpha", "beta", "gamma"] {
        let c = db.open_collection(name).unwrap();
        assert_eq!(c.count().unwrap(), 120, "{name}");
        assert!(
            c.stats().unwrap().segments > 1,
            "{name} should have several segments"
        );
        let r = c
            .search(&SearchRequest::new(
                VectorView::f32(&[119.0, 1.0, 2.0, 3.0]),
                1,
            ))
            .unwrap();
        assert_eq!(r.hits[0].id.display(), format!("{name}-119"));
    }
    db.stats().unwrap();
    db.close().unwrap();
}
