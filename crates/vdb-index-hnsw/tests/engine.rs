//! The graph index driving a real database.
//!
//! The other tests exercise the index against a hand-made source. This one goes through the
//! engine: segments, flushes, tombstones, metadata filters and reopening. That path is where an
//! index meets the parts of the system it does not control, and where an assumption about row
//! numbering or liveness that held in isolation stops holding.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::sync::Arc;

use vdb_core::api::{CollectionSpec, Database, DatabaseConfig, SearchRequest};
use vdb_core::clock::ManualClock;
use vdb_core::document::DocumentInput;
use vdb_core::filter::Filter;
use vdb_core::metadata::{Metadata, Value};
use vdb_core::vector::VectorView;
use vdb_core::{Metric, WriteBatch};
use vdb_index_hnsw::HnswIndex;
use vdb_storage_memory::MemoryStorage;
use vdb_testkit::Rng;

const DIM: usize = 32;

fn corpus(n: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng::new(seed);
    let centres: Vec<Vec<f32>> = (0..8)
        .map(|_| (0..DIM).map(|_| rng.next_f32() * 2.0 - 1.0).collect())
        .collect();
    (0..n)
        .map(|i| {
            centres[i % 8]
                .iter()
                .map(|c| c + (rng.next_f32() - 0.5) * 0.3)
                .collect()
        })
        .collect()
}

fn open(index: bool) -> (Database, Vec<Vec<f32>>) {
    let storage = Arc::new(MemoryStorage::new());
    let clock = Arc::new(ManualClock::new(1_700_000_000_000));
    let db = if index {
        Database::open_with_index(
            storage,
            DatabaseConfig::default(),
            clock,
            Arc::new(HnswIndex::new()),
        )
    } else {
        Database::open(storage, DatabaseConfig::default(), clock)
    }
    .unwrap();

    let vectors = corpus(1500, 0xC0FFEE);
    let c = db
        .create_collection(CollectionSpec::new("docs", DIM as u32, Metric::Cosine))
        .unwrap();
    let mut batch = WriteBatch::with_capacity(vectors.len());
    for (i, v) in vectors.iter().enumerate() {
        let mut m = Metadata::new();
        m.insert("bucket", Value::I64((i % 4) as i64));
        batch
            .upsert(DocumentInput::new(format!("doc-{i:05}"), VectorView::f32(v)).with_metadata(m));
    }
    c.write_batch(batch).unwrap();
    c.flush().unwrap();
    (db, vectors)
}

#[test]
fn the_engine_returns_good_results_through_the_graph() {
    let (exact_db, vectors) = open(false);
    let (hnsw_db, _) = open(true);
    let exact = exact_db.open_collection("docs").unwrap();
    let hnsw = hnsw_db.open_collection("docs").unwrap();

    let mut overlap = 0usize;
    let queries = corpus(20, 0xBEEF);
    for q in &queries {
        let want: Vec<_> = exact
            .search(&SearchRequest::new(VectorView::f32(q), 10))
            .unwrap()
            .hits
            .into_iter()
            .map(|h| h.id)
            .collect();
        let got = hnsw
            .search(&SearchRequest::new(VectorView::f32(q), 10))
            .unwrap();
        assert_eq!(got.hits.len(), 10);
        overlap += got.hits.iter().filter(|h| want.contains(&h.id)).count();
    }
    let recall = overlap as f64 / (queries.len() * 10) as f64;
    assert!(
        recall >= 0.90,
        "recall through the engine was {recall:.3}; the index works in isolation, so this is \
         the engine path"
    );
    let _ = vectors;
}

#[test]
fn deletes_are_respected_through_the_engine() {
    let (db, _) = open(true);
    let c = db.open_collection("docs").unwrap();

    let q = corpus(1, 0xBEEF).remove(0);
    let before = c
        .search(&SearchRequest::new(VectorView::f32(&q), 5))
        .unwrap();
    let doomed: Vec<_> = before.hits.iter().map(|h| h.id.clone()).collect();
    for id in &doomed {
        c.delete(id.clone()).unwrap();
    }

    let after = c
        .search(&SearchRequest::new(VectorView::f32(&q), 5))
        .unwrap();
    for hit in &after.hits {
        assert!(
            !doomed.contains(&hit.id),
            "{:?} was deleted but the graph returned it",
            hit.id
        );
    }
}

#[test]
fn metadata_filters_work_through_the_graph() {
    let (db, _) = open(true);
    let c = db.open_collection("docs").unwrap();
    let q = corpus(1, 0xBEEF).remove(0);

    let hits = c
        .search(
            &SearchRequest::new(VectorView::f32(&q), 10)
                .with_filter(&Filter::eq("bucket", Value::I64(2))),
        )
        .unwrap();
    assert_eq!(hits.hits.len(), 10, "a quarter of the corpus qualifies");
    for hit in &hits.hits {
        let doc = c.get(&hit.id).unwrap().unwrap();
        assert_eq!(doc.metadata.get("bucket"), Some(&Value::I64(2)));
    }
}

/// Writing after the graph is built must not return stale results.
#[test]
fn new_documents_appear_in_later_searches() {
    let (db, _) = open(true);
    let c = db.open_collection("docs").unwrap();
    let q = vec![9.0f32; DIM];

    // Nothing in the corpus is near this, so the newcomer must win outright.
    c.upsert(DocumentInput::new("newcomer", VectorView::f32(&q)))
        .unwrap();
    c.flush().unwrap();

    let hits = c
        .search(&SearchRequest::new(VectorView::f32(&q), 1))
        .unwrap();
    assert_eq!(hits.hits[0].id, "newcomer".into());
}
