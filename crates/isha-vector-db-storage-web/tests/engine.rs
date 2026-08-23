//! The whole engine, running on the web backend.
//!
//! Conformance proves the backend obeys the storage contract. This proves the engine actually
//! works on it: a database created, written, flushed, closed, reopened from the persisted bytes,
//! and searched — over a backend with no atomic rename and no durable sync, which is the
//! combination the dual-slot manifest was designed for and which no other backend in this
//! workspace exercises.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use isha_vector_db_core::api::{CollectionSpec, Database, DatabaseConfig, SearchRequest};
use isha_vector_db_core::clock::ManualClock;
use isha_vector_db_core::document::DocumentInput;
use isha_vector_db_core::filter::Filter;
use isha_vector_db_core::metadata::{Metadata, Value};
use isha_vector_db_core::storage::Storage;
use isha_vector_db_core::vector::VectorView;
use isha_vector_db_core::{Metric, WriteBatch};
use isha_vector_db_storage_web::{test_host, WebStorage};

fn storage(root: &str) -> Arc<WebStorage> {
    let s = WebStorage::open(root.to_owned());
    s.create_dir_all(&isha_vector_db_core::path::DbPath::root())
        .unwrap();
    Arc::new(s)
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(1_700_000_000_000))
}

#[test]
fn the_engine_runs_on_the_web_backend() {
    let root = test_host::unique_root("engine");

    {
        let db = Database::open(storage(&root), DatabaseConfig::default(), clock()).unwrap();
        let c = db
            .create_collection(CollectionSpec::new("notes", 4, Metric::Cosine))
            .unwrap();
        let mut batch = WriteBatch::with_capacity(8);
        for i in 0..8u32 {
            let mut m = Metadata::new();
            m.insert("bucket", Value::I64(i64::from(i % 2)));
            let v = [i as f32, 1.0, 0.0, -1.0];
            batch.upsert(
                DocumentInput::new(format!("doc-{i}"), VectorView::f32(&v)).with_metadata(m),
            );
        }
        c.write_batch(batch).unwrap();
        c.flush().unwrap();
        assert_eq!(c.count().unwrap(), 8);
        db.close().unwrap();
    }

    // Reopened from bytes alone: nothing survives in process memory between these blocks.
    let db = Database::open(
        storage(&root),
        DatabaseConfig::default().create_if_missing(false),
        clock(),
    )
    .unwrap();
    let c = db.open_collection("notes").unwrap();
    assert_eq!(c.count().unwrap(), 8);

    let hits = c
        .search(&SearchRequest::new(
            VectorView::f32(&[7.0, 1.0, 0.0, -1.0]),
            3,
        ))
        .unwrap();
    assert_eq!(hits.hits.len(), 3);
    assert_eq!(hits.hits[0].id, "doc-7".into());

    let filtered = c
        .search(
            &SearchRequest::new(VectorView::f32(&[7.0, 1.0, 0.0, -1.0]), 10)
                .with_filter(&Filter::eq("bucket", Value::I64(1))),
        )
        .unwrap();
    assert_eq!(filtered.hits.len(), 4, "half the documents are in bucket 1");

    db.verify(isha_vector_db_core::api::VerifyLevel::Full)
        .unwrap();
    db.close().unwrap();
}

/// The engine must not silently assume a durability guarantee this platform cannot give.
#[test]
fn the_backend_reports_the_weaker_guarantees_honestly() {
    let caps = WebStorage::open("/caps").capabilities();
    assert!(!caps.atomic_rename, "OPFS has no atomic rename");
    assert!(!caps.durable_sync, "OPFS flush is best-effort");
    assert!(
        caps.prefers_few_large_files,
        "per-file overhead dominates in OPFS"
    );
}
