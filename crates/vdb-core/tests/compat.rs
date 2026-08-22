//! A whole database written by an older release must still open.
//!
//! The golden fixtures in `vdb-format` prove that individual *structures* survive a format
//! change. They do not prove that a database does: a database is a manifest pointing at segments
//! pointing at four files each, opened through recovery, the write lock and the catalog. This
//! test opens one that was genuinely written by the v1 encoder, byte for byte as committed.
//!
//! To regenerate — which should be needed only when adding a *new* version's fixture, never to
//! repair an old one — set the format constants back to that version and run:
//!
//! ```text
//! VDB_BLESS=1 cargo test -p vdb-core --test compat
//! ```
//!
//! An old fixture that starts failing is a compatibility break, not a stale fixture. Deleting or
//! re-blessing it hides exactly the bug this file exists to catch.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use vdb_core::api::{CollectionSpec, Database, DatabaseConfig, SearchRequest};
use vdb_core::clock::ManualClock;
use vdb_core::document::{DocumentInput, Include};
use vdb_core::filter::Filter;
use vdb_core::metadata::{Metadata, Value};
use vdb_core::vector::VectorView;
use vdb_core::{Metric, WriteBatch};
use vdb_storage_os::OsStorage;

fn fixture(version: u16) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("../../testdata/db-v{version}"))
}

/// Four documents, one of them with metadata wide enough to be indexed under v2 and walked
/// under v1 — so the fixture covers both encodings of the same logical content.
fn seed(db: &Database) {
    let c = db
        .create_collection(CollectionSpec::new("notes", 4, Metric::Cosine))
        .unwrap();
    let mut batch = WriteBatch::with_capacity(4);
    for i in 0..4u32 {
        let mut m = Metadata::new();
        m.insert(
            "kind",
            Value::Str(if i % 2 == 0 { "even" } else { "odd" }.into()),
        );
        m.insert("n", Value::I64(i64::from(i)));
        if i == 3 {
            // Ten fields: above the v2 index threshold, plainly encoded under v1.
            for f in 0..8 {
                m.insert(format!("wide_{f:02}"), Value::I64(i64::from(f)));
            }
        }
        let v = [i as f32, 1.0, 0.0, -1.0];
        batch.upsert(DocumentInput::new(format!("doc-{i}"), VectorView::f32(&v)).with_metadata(m));
    }
    c.write_batch(batch).unwrap();
    c.flush().unwrap();
}

#[test]
fn a_database_written_by_v1_still_opens() {
    let path = fixture(1);

    if std::env::var("VDB_BLESS").is_ok() {
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        let storage = Arc::new(OsStorage::open(path.to_str().unwrap()).unwrap());
        let db = Database::open(
            storage,
            DatabaseConfig::default(),
            Arc::new(ManualClock::new(1_700_000_000_000)),
        )
        .unwrap();
        seed(&db);
        db.close().unwrap();
        return;
    }

    assert!(
        path.join("MANIFEST-A").exists() || path.join("MANIFEST-B").exists(),
        "missing v1 database fixture at {}",
        path.display()
    );

    // Read-only, so opening it can neither take the write lock nor rewrite the committed bytes.
    let storage = Arc::new(OsStorage::open(path.to_str().unwrap()).unwrap());
    let db = Database::open(
        storage,
        DatabaseConfig::default().read_only(true),
        Arc::new(ManualClock::new(1_700_000_000_000)),
    )
    .unwrap();

    let c = db.open_collection("notes").unwrap();
    assert_eq!(c.count().unwrap(), 4);

    // Metadata written by the old encoder must decode, including through a filter — the path
    // that changed. A v1 record has no offset table, so this exercises the fallback.
    let got = c
        .get_with(&"doc-3".into(), Include::ALL)
        .unwrap()
        .expect("doc-3 must be present");
    assert_eq!(got.metadata.get("wide_07"), Some(&Value::I64(7)));

    let hits = c
        .search(
            &SearchRequest::new(VectorView::f32(&[3.0, 1.0, 0.0, -1.0]), 4)
                .with_filter(&Filter::eq("kind", Value::Str("odd".into()))),
        )
        .unwrap();
    assert_eq!(hits.hits.len(), 2, "two odd documents");

    // A filter naming a field of the wide record: v1 bytes reached through the v2 lookup.
    let wide = c
        .search(
            &SearchRequest::new(VectorView::f32(&[3.0, 1.0, 0.0, -1.0]), 4)
                .with_filter(&Filter::eq("wide_07", Value::I64(7))),
        )
        .unwrap();
    assert_eq!(wide.hits.len(), 1);
    assert_eq!(wide.hits[0].id, "doc-3".into());

    db.verify(vdb_core::api::VerifyLevel::Full).unwrap();
    db.close().unwrap();
}
