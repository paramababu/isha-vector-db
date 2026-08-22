//! Search through the public API: the first end-to-end proof that the engine answers questions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::Arc;

use vdb_core::api::{Collection, CollectionSpec, Database, DatabaseConfig, SearchRequest};
use vdb_core::clock::ManualClock;
use vdb_core::document::{DocId, DocumentInput, Include};
use vdb_core::index::{Budget, IndexKind};
use vdb_core::search::{inv_norm, Metric, Scorer};
use vdb_core::vector::VectorView;
use vdb_storage_memory::MemoryStorage;

fn db(mem: &MemoryStorage) -> Database {
    Database::open(
        Arc::new(mem.clone()),
        DatabaseConfig::default(),
        Arc::new(ManualClock::default()),
    )
    .unwrap()
}

fn collection(db: &Database, metric: Metric, dim: u32) -> Collection {
    db.create_collection(CollectionSpec::new("docs", dim, metric))
        .unwrap()
}

fn add(c: &Collection, id: &str, v: &[f32]) {
    c.insert(DocumentInput::new(id, VectorView::f32(v)))
        .unwrap();
}

fn ids(response: &vdb_core::api::SearchResponse) -> Vec<String> {
    response.hits.iter().map(|h| h.id.display()).collect()
}

fn query(c: &Collection, v: &[f32], k: usize) -> vdb_core::api::SearchResponse {
    c.search(&SearchRequest::new(VectorView::f32(v), k))
        .unwrap()
}

// ---------------------------------------------------------------------------

#[test]
fn finds_the_nearest_documents() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "east", &[1.0, 0.0]);
    add(&c, "north", &[0.0, 1.0]);
    add(&c, "west", &[-1.0, 0.0]);
    add(&c, "northeast", &[0.7, 0.7]);

    let r = query(&c, &[1.0, 0.0], 2);
    assert_eq!(ids(&r), vec!["east", "northeast"]);
    assert!((r.hits[0].score - 1.0).abs() < 1e-5);
    assert!(r.stats.exact);
    assert_eq!(r.stats.index_kind, IndexKind::Flat);
    db.close().unwrap();
}

#[test]
fn searching_an_empty_collection_returns_nothing_rather_than_failing() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 3);
    let r = query(&c, &[1.0, 0.0, 0.0], 10);
    assert!(r.is_empty());
    assert_eq!(r.stats.considered, 0);
    db.close().unwrap();
}

#[test]
fn asking_for_more_than_exists_returns_everything() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "a", &[1.0, 0.0]);
    add(&c, "b", &[0.0, 1.0]);
    assert_eq!(query(&c, &[1.0, 0.0], 100).len(), 2);
    db.close().unwrap();
}

/// Unflushed writes must be searchable. A document written a moment ago being invisible until
/// some internal threshold is crossed would be an astonishing thing for a database to do.
#[test]
fn documents_are_searchable_before_and_after_a_flush() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "buffered", &[1.0, 0.0]);
    assert_eq!(ids(&query(&c, &[1.0, 0.0], 1)), vec!["buffered"]);

    c.flush().unwrap();
    assert_eq!(ids(&query(&c, &[1.0, 0.0], 1)), vec!["buffered"]);

    // And a mix of flushed and buffered rows is searched as one collection.
    add(&c, "fresh", &[0.99, 0.01]);
    let r = query(&c, &[1.0, 0.0], 2);
    assert_eq!(ids(&r), vec!["buffered", "fresh"]);
    db.close().unwrap();
}

#[test]
fn deleted_documents_disappear_from_results() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "a", &[1.0, 0.0]);
    add(&c, "b", &[0.9, 0.1]);
    c.flush().unwrap();

    c.delete("a").unwrap();
    assert_eq!(ids(&query(&c, &[1.0, 0.0], 5)), vec!["b"]);

    // Still gone after the tombstone is folded into the segment.
    c.flush().unwrap();
    assert_eq!(ids(&query(&c, &[1.0, 0.0], 5)), vec!["b"]);
    db.close().unwrap();
}

/// The overwrite-across-flushes case, seen from search: one document, one hit, the newer vector.
#[test]
fn an_overwritten_document_appears_once_with_its_newer_vector() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "a", &[1.0, 0.0]);
    c.flush().unwrap();
    c.upsert(DocumentInput::new("a", VectorView::f32(&[0.0, 1.0])))
        .unwrap();
    c.flush().unwrap();

    let r = query(&c, &[0.0, 1.0], 10);
    assert_eq!(ids(&r), vec!["a"], "the document must appear exactly once");
    assert!(
        (r.hits[0].score - 1.0).abs() < 1e-5,
        "and with its newer vector"
    );
    db.close().unwrap();
}

#[test]
fn a_buffered_overwrite_shadows_the_flushed_copy() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "a", &[1.0, 0.0]);
    c.flush().unwrap();
    // Not flushed: the memtable copy must shadow the segment copy during the scan.
    c.upsert(DocumentInput::new("a", VectorView::f32(&[0.0, 1.0])))
        .unwrap();

    let r = query(&c, &[1.0, 0.0], 10);
    assert_eq!(ids(&r), vec!["a"]);
    assert!(
        r.hits[0].score.abs() < 1e-5,
        "the old vector must not be scored: {r:?}"
    );
    db.close().unwrap();
}

#[test]
fn every_metric_works_end_to_end() {
    for metric in Metric::ALL {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = collection(&db, metric, 2);
        add(&c, "near", &[1.0, 0.0]);
        add(&c, "far", &[-1.0, 0.0]);

        let r = query(&c, &[1.0, 0.0], 1);
        assert_eq!(ids(&r), vec!["near"], "{metric:?}");

        match metric {
            Metric::Dot => assert!(r.hits[0].distance.is_none(), "Dot defines no distance"),
            _ => assert!(
                r.hits[0].distance.is_some(),
                "{metric:?} should report a distance"
            ),
        }
        db.close().unwrap();
    }
}

#[test]
fn l2_reports_true_euclidean_distance() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::L2, 2);
    add(&c, "p", &[3.0, 4.0]);
    let r = query(&c, &[0.0, 0.0], 1);
    let d = r.hits[0].distance.unwrap();
    assert!((d - 5.0).abs() < 1e-4, "distance was {d}");
    assert!(
        (r.hits[0].score + 25.0).abs() < 1e-3,
        "score is -squared_l2"
    );
    db.close().unwrap();
}

#[test]
fn min_score_filters_and_is_inclusive() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "exact", &[1.0, 0.0]);
    add(&c, "orthogonal", &[0.0, 1.0]);
    add(&c, "opposite", &[-1.0, 0.0]);

    let r = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 10).with_min_score(0.0))
        .unwrap();
    let names = ids(&r);
    assert!(names.contains(&"exact".to_owned()));
    assert!(names.contains(&"orthogonal".to_owned()), "0.0 is inclusive");
    assert!(!names.contains(&"opposite".to_owned()));
    db.close().unwrap();
}

#[test]
fn the_metric_can_be_overridden_per_query() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "short", &[1.0, 0.0]);
    add(&c, "long", &[10.0, 0.0]);

    // Cosine cannot separate them; the inner product ranks by magnitude.
    let r = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 1).with_metric(Metric::Dot))
        .unwrap();
    assert_eq!(ids(&r), vec!["long"]);
    db.close().unwrap();
}

#[test]
fn include_controls_what_comes_back_with_each_hit() {
    use vdb_core::metadata::{Metadata, Value};
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);

    let mut meta = Metadata::new();
    meta.insert("kind", Value::Str("tool".into()));
    c.insert(
        DocumentInput::new("a", VectorView::f32(&[1.0, 0.0]))
            .with_metadata(meta.clone())
            .with_content(b"source text"),
    )
    .unwrap();

    let bare = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 1).with_include(Include::NONE))
        .unwrap();
    assert!(bare.hits[0].document.is_none());

    let full = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 1).with_include(Include::ALL))
        .unwrap();
    let doc = full.hits[0].document.as_ref().unwrap();
    assert_eq!(doc.metadata, meta);
    assert_eq!(doc.vector, Some(vec![1.0, 0.0]));
    assert_eq!(doc.content.as_deref(), Some(b"source text".as_slice()));

    // And the same after a flush, out of a segment rather than the memtable.
    c.flush().unwrap();
    let full = c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 1).with_include(Include::ALL))
        .unwrap();
    assert_eq!(full.hits[0].document.as_ref().unwrap().metadata, meta);
    db.close().unwrap();
}

// ---- validation ----

#[test]
fn a_query_of_the_wrong_dimension_is_refused() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 4);
    assert!(c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 1))
        .is_err());
    db.close().unwrap();
}

#[test]
fn a_non_finite_query_is_refused() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "a", &[1.0, 0.0]);
    assert!(c
        .search(&SearchRequest::new(VectorView::f32(&[f32::NAN, 0.0]), 1))
        .is_err());
    db.close().unwrap();
}

#[test]
fn a_top_k_of_zero_or_beyond_the_limit_is_refused() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    assert!(c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 0))
        .is_err());
    assert!(c
        .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 100_000))
        .is_err());
    db.close().unwrap();
}

// ---- ordering and determinism ----

/// The user-visible contract: score descending, ties by ascending id.
#[test]
fn ties_are_broken_by_ascending_id() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    // Identical vectors inserted in an order that does not match id order.
    for id in ["zulu", "alpha", "mike", "bravo"] {
        add(&c, id, &[1.0, 0.0]);
    }
    let r = query(&c, &[1.0, 0.0], 4);
    assert_eq!(ids(&r), vec!["alpha", "bravo", "mike", "zulu"]);
    db.close().unwrap();
}

#[test]
fn repeating_a_query_returns_exactly_the_same_answer() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 3);
    for i in 0..200 {
        let v = [(i % 7) as f32, (i % 11) as f32, (i % 3) as f32];
        add(&c, &format!("doc-{i:03}"), &v);
    }
    c.flush().unwrap();
    for i in 200..260 {
        let v = [(i % 7) as f32, (i % 11) as f32, (i % 3) as f32];
        add(&c, &format!("doc-{i:03}"), &v);
    }

    let first = query(&c, &[1.0, 2.0, 1.0], 20);
    for _ in 0..5 {
        assert_eq!(ids(&query(&c, &[1.0, 2.0, 1.0], 20)), ids(&first));
    }
    db.close().unwrap();
}

/// Exactness, end to end: the engine's ranking must match one computed independently.
#[test]
fn results_match_an_independently_computed_ranking() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 6);

    let mut seed = 0xFEED_BEEF_1234_5678u64;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        ((seed >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
    };
    let corpus: Vec<(String, Vec<f32>)> = (0..300)
        .map(|i| (format!("doc-{i:03}"), (0..6).map(|_| next()).collect()))
        .collect();
    for (id, v) in &corpus {
        add(&c, id, v);
    }
    // Half on disk, half buffered, so the scan has to cover both.
    c.flush().unwrap();
    for i in 0..40 {
        let v: Vec<f32> = (0..6).map(|_| next()).collect();
        add(&c, &format!("extra-{i:02}"), &v);
    }

    let q: Vec<f32> = (0..6).map(|_| next()).collect();
    let scorer = Scorer::new(Metric::Cosine, &q);
    let mut expected: Vec<(String, f32)> = c
        .ids()
        .unwrap()
        .into_iter()
        .map(|id| {
            let doc = c.get_with(&id, Include::ALL).unwrap().unwrap();
            let v = doc.vector.unwrap();
            (id.display(), scorer.score(&v, inv_norm(&v)))
        })
        .collect();
    expected.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    expected.truncate(15);

    let got = query(&c, &q, 15);
    assert_eq!(
        ids(&got),
        expected
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>(),
        "the engine's ranking disagreed with an independently computed one"
    );
    db.close().unwrap();
}

// ---- budgets ----

#[test]
fn a_cancelled_search_reports_cancellation() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    for i in 0..3000 {
        add(&c, &format!("doc-{i:04}"), &[i as f32, 1.0]);
    }
    let budget = Budget::unlimited();
    budget.cancel();
    let request = SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 5);
    assert!(matches!(
        c.search_with_budget(&request, &budget),
        Err(vdb_core::DbError::Cancelled)
    ));
    db.close().unwrap();
}

#[test]
fn a_scan_ceiling_stops_a_search_that_would_be_too_expensive() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    for i in 0..5000 {
        add(&c, &format!("doc-{i:04}"), &[i as f32, 1.0]);
    }
    let budget = Budget::with_max_scanned(1000);
    let request = SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 5);
    assert!(c.search_with_budget(&request, &budget).is_err());
    db.close().unwrap();
}

// ---- persistence ----

#[test]
fn search_works_after_a_reopen() {
    let mem = MemoryStorage::new();
    {
        let db = db(&mem);
        let c = collection(&db, Metric::Cosine, 2);
        add(&c, "east", &[1.0, 0.0]);
        add(&c, "north", &[0.0, 1.0]);
        db.close().unwrap();
    }
    let db = db(&mem);
    let c = db.open_collection("docs").unwrap();
    assert_eq!(ids(&query(&c, &[1.0, 0.0], 1)), vec!["east"]);
    db.close().unwrap();
}

#[test]
fn search_spans_many_segments() {
    let mem = MemoryStorage::new();
    let db = Database::open(
        Arc::new(mem.clone()),
        DatabaseConfig::default().flush_threshold_bytes(512),
        Arc::new(ManualClock::default()),
    )
    .unwrap();
    let c = collection(&db, Metric::Cosine, 4);
    // Directions spread around a circle, so neighbouring documents are genuinely
    // distinguishable rather than differing below f32 precision.
    let vectors: Vec<[f32; 4]> = (0..300)
        .map(|i| {
            let angle = i as f32 * core::f32::consts::TAU / 300.0;
            [angle.cos(), angle.sin(), 0.25, -0.5]
        })
        .collect();
    for (i, v) in vectors.iter().enumerate() {
        add(&c, &format!("doc-{i:03}"), v);
    }
    assert!(
        c.stats().unwrap().segments > 1,
        "the test needs several segments"
    );
    assert_eq!(c.count().unwrap(), 300);

    // A document scattered across some segment in the middle must be findable by its own
    // vector — which is the thing that would break if a segment were skipped.
    for target in [0usize, 77, 150, 299] {
        let r = query(&c, &vectors[target], 1);
        assert_eq!(ids(&r), vec![format!("doc-{target:03}")], "target {target}");
    }
    assert_eq!(
        query(&c, &vectors[0], 300).len(),
        300,
        "every segment must be scanned"
    );
    db.close().unwrap();
}

#[test]
fn a_search_on_a_collection_of_only_deleted_documents_returns_nothing() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    for id in ["a", "b", "c"] {
        add(&c, id, &[1.0, 0.0]);
    }
    c.flush().unwrap();
    for id in ["a", "b", "c"] {
        c.delete(id).unwrap();
    }
    assert!(query(&c, &[1.0, 0.0], 10).is_empty());
    c.flush().unwrap();
    assert!(query(&c, &[1.0, 0.0], 10).is_empty());
    assert_eq!(c.count().unwrap(), 0);
    db.close().unwrap();
}

#[test]
fn the_zero_vector_is_searchable_and_scores_zero_under_cosine() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = collection(&db, Metric::Cosine, 2);
    add(&c, "zero", &[0.0, 0.0]);
    add(&c, "east", &[1.0, 0.0]);

    let r = query(&c, &[1.0, 0.0], 2);
    assert_eq!(ids(&r), vec!["east", "zero"]);
    assert_eq!(r.hits[1].score, 0.0, "the zero vector has no direction");
    assert!(r.hits.iter().all(|h| h.score.is_finite()));
    db.close().unwrap();
}

#[test]
fn u64_ids_search_and_tie_break_numerically() {
    let mem = MemoryStorage::new();
    let db = db(&mem);
    let c = db
        .create_collection(CollectionSpec::new("numeric", 2, Metric::Cosine).with_u64_ids())
        .unwrap();
    for id in [30u64, 10, 20] {
        c.insert(DocumentInput::new(id, VectorView::f32(&[1.0, 0.0])))
            .unwrap();
    }
    let r = query(&c, &[1.0, 0.0], 3);
    assert_eq!(
        r.hits.iter().map(|h| h.id.clone()).collect::<Vec<_>>(),
        vec![DocId::U64(10), DocId::U64(20), DocId::U64(30)]
    );
    db.close().unwrap();
}

// ---------------------------------------------------------------------------
// metadata filters
// ---------------------------------------------------------------------------

mod filters {
    use super::*;
    use vdb_core::filter::Filter;
    use vdb_core::metadata::{Metadata, Value};

    fn meta(pairs: &[(&str, Value)]) -> Metadata {
        let mut m = Metadata::new();
        for (k, v) in pairs {
            m.insert(*k, v.clone());
        }
        m
    }

    /// Four documents at increasing angles from the query, so ranking and filtering can be told
    /// apart: the filter must change *which* documents come back, not their relative order.
    fn corpus(db: &Database) -> Collection {
        let c = collection(db, Metric::Cosine, 2);
        let docs: [(&str, [f32; 2], Metadata); 4] = [
            (
                "hammer",
                [1.0, 0.0],
                meta(&[
                    ("category", Value::Str("tools".into())),
                    ("price", Value::F64(25.0)),
                    ("tags", Value::Array(vec![Value::Str("hand".into())])),
                ]),
            ),
            (
                "saw",
                [0.95, 0.31],
                meta(&[
                    ("category", Value::Str("tools".into())),
                    ("price", Value::F64(75.0)),
                    (
                        "tags",
                        Value::Array(vec![Value::Str("hand".into()), Value::Str("sharp".into())]),
                    ),
                ]),
            ),
            (
                "ball",
                [0.7, 0.7],
                meta(&[
                    ("category", Value::Str("toys".into())),
                    ("price", Value::F64(5.0)),
                ]),
            ),
            (
                "kite",
                [0.31, 0.95],
                meta(&[("category", Value::Str("toys".into()))]),
            ),
        ];
        for (id, v, m) in docs {
            c.insert(DocumentInput::new(id, VectorView::f32(&v)).with_metadata(m))
                .unwrap();
        }
        c
    }

    fn filtered(c: &Collection, filter: &Filter, k: usize) -> Vec<String> {
        let request = SearchRequest::new(VectorView::f32(&[1.0, 0.0]), k).with_filter(filter);
        ids(&c.search(&request).unwrap())
    }

    #[test]
    fn a_filter_narrows_the_result_without_reordering_it() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);

        assert_eq!(
            ids(&query(&c, &[1.0, 0.0], 4)),
            vec!["hammer", "saw", "ball", "kite"]
        );
        let tools = Filter::eq("category", Value::Str("tools".into()));
        assert_eq!(filtered(&c, &tools, 4), vec!["hammer", "saw"]);
        db.close().unwrap();
    }

    #[test]
    fn filters_compose() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);

        let cheap_tools = Filter::eq("category", Value::Str("tools".into()))
            .and(Filter::lt("price", Value::F64(50.0)));
        assert_eq!(filtered(&c, &cheap_tools, 4), vec!["hammer"]);

        let either = Filter::any(vec![
            Filter::eq("category", Value::Str("toys".into())),
            Filter::gt("price", Value::F64(50.0)),
        ]);
        assert_eq!(filtered(&c, &either, 4), vec!["saw", "ball", "kite"]);

        let not_tools = Filter::negate(Filter::eq("category", Value::Str("tools".into())));
        assert_eq!(filtered(&c, &not_tools, 4), vec!["ball", "kite"]);
        db.close().unwrap();
    }

    #[test]
    fn a_filter_on_an_absent_field_matches_only_what_the_rules_say() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);

        // "kite" has no price.
        assert_eq!(
            filtered(&c, &Filter::exists("price"), 4),
            vec!["hammer", "saw", "ball"]
        );
        assert_eq!(filtered(&c, &Filter::is_null("price"), 4), vec!["kite"]);
        assert_eq!(
            filtered(&c, &Filter::gt("price", Value::F64(0.0)), 4),
            vec!["hammer", "saw", "ball"]
        );
        db.close().unwrap();
    }

    #[test]
    fn array_membership_and_prefix_filters() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);

        assert_eq!(
            filtered(&c, &Filter::contains("tags", Value::Str("sharp".into())), 4),
            vec!["saw"]
        );
        assert_eq!(
            filtered(&c, &Filter::starts_with("category", "too"), 4),
            vec!["hammer", "saw"]
        );
        db.close().unwrap();
    }

    #[test]
    fn a_filter_matching_nothing_returns_no_hits() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);
        let none = Filter::eq("category", Value::Str("nonexistent".into()));
        let r = c
            .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 4).with_filter(&none))
            .unwrap();
        assert!(r.is_empty());
        assert_eq!(r.stats.considered, 0, "nothing should have been scored");
        assert_eq!(r.stats.skipped, 4, "everything should have been skipped");
        db.close().unwrap();
    }

    #[test]
    fn a_filter_matching_everything_changes_nothing() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);
        let all = Filter::all(vec![]);
        assert_eq!(filtered(&c, &all, 4), ids(&query(&c, &[1.0, 0.0], 4)));
        db.close().unwrap();
    }

    /// `top_k` must count *matching* documents, not scanned ones. Returning three results
    /// because the nearest ten happened to be filtered out is a classic and infuriating bug.
    #[test]
    fn top_k_counts_matches_not_candidates() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = collection(&db, Metric::Cosine, 2);
        for i in 0..100 {
            let angle = i as f32 * core::f32::consts::TAU / 100.0;
            let keep = i % 10 == 0;
            c.insert(
                DocumentInput::new(
                    format!("doc-{i:03}"),
                    VectorView::f32(&[angle.cos(), angle.sin()]),
                )
                .with_metadata(meta(&[("keep", Value::Bool(keep))])),
            )
            .unwrap();
        }
        let keep = Filter::eq("keep", Value::Bool(true));
        let r = c
            .search(&SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 5).with_filter(&keep))
            .unwrap();
        assert_eq!(
            r.len(),
            5,
            "should return five matches, not five of the nearest hundred"
        );
        assert_eq!(
            r.stats.considered, 10,
            "only the ten matching rows should be scored"
        );
        assert_eq!(r.stats.skipped, 90);
        db.close().unwrap();
    }

    #[test]
    fn filters_work_across_the_memtable_and_segments_alike() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);
        c.flush().unwrap();

        // A buffered document that also matches.
        c.insert(
            DocumentInput::new("chisel", VectorView::f32(&[0.99, 0.14])).with_metadata(meta(&[
                ("category", Value::Str("tools".into())),
                ("price", Value::F64(30.0)),
            ])),
        )
        .unwrap();

        let tools = Filter::eq("category", Value::Str("tools".into()));
        assert_eq!(filtered(&c, &tools, 5), vec!["hammer", "chisel", "saw"]);
        db.close().unwrap();
    }

    #[test]
    fn a_filter_survives_a_reopen() {
        let mem = MemoryStorage::new();
        {
            let db = db(&mem);
            corpus(&db);
            db.close().unwrap();
        }
        let db = db(&mem);
        let c = db.open_collection("docs").unwrap();
        let tools = Filter::eq("category", Value::Str("tools".into()));
        assert_eq!(filtered(&c, &tools, 4), vec!["hammer", "saw"]);
        db.close().unwrap();
    }

    #[test]
    fn filters_combine_with_thresholds_and_metric_overrides() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);

        let tools = Filter::eq("category", Value::Str("tools".into()));
        let r = c
            .search(
                &SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 10)
                    .with_filter(&tools)
                    .with_min_score(0.99),
            )
            .unwrap();
        assert_eq!(
            ids(&r),
            vec!["hammer"],
            "the threshold applies on top of the filter"
        );
        db.close().unwrap();
    }

    #[test]
    fn an_over_complex_filter_is_refused() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = corpus(&db);
        let mut f = Filter::exists("category");
        for _ in 0..64 {
            f = Filter::negate(f);
        }
        let request = SearchRequest::new(VectorView::f32(&[1.0, 0.0]), 4).with_filter(&f);
        assert!(c.search(&request).is_err());
        db.close().unwrap();
    }

    /// A document with no metadata at all must not break a filtered scan.
    #[test]
    fn documents_without_metadata_are_handled() {
        let mem = MemoryStorage::new();
        let db = db(&mem);
        let c = collection(&db, Metric::Cosine, 2);
        add(&c, "bare", &[1.0, 0.0]);
        c.insert(
            DocumentInput::new("tagged", VectorView::f32(&[0.9, 0.1]))
                .with_metadata(meta(&[("kind", Value::Str("x".into()))])),
        )
        .unwrap();

        assert_eq!(filtered(&c, &Filter::exists("kind"), 5), vec!["tagged"]);
        assert_eq!(filtered(&c, &Filter::is_null("kind"), 5), vec!["bare"]);
        db.close().unwrap();
    }
}
