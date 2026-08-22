//! Everything an approximate index must get *exactly* right.
//!
//! Recall is allowed to be less than perfect. None of the following is: an index that returns a
//! deleted document, ignores a filter, or gives different answers on different runs is broken,
//! not approximate.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use vdb_core::document::RowId;
use vdb_core::error::{DbError, Result};
use vdb_core::index::{
    AllLive, Budget, IndexKind, LiveSet, NoSnapshots, RowPredicate, RowVisitor, SearchCtx,
    SearchParams, VectorIndex, VectorSource,
};
use vdb_core::search::{inv_norm, Metric, TopK};
use vdb_index_hnsw::{HnswIndex, HnswParams};
use vdb_testkit::Rng;

#[derive(Debug)]
struct Rows {
    dimension: u32,
    rows: Vec<(RowId, Vec<u8>, f32)>,
}

impl Rows {
    fn random(n: usize, dimension: usize, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let rows = (0..n)
            .map(|i| {
                let v: Vec<f32> = (0..dimension).map(|_| rng.next_f32() - 0.5).collect();
                let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                (RowId::new(0, i as u32), bytes, inv_norm(&v))
            })
            .collect();
        Self {
            dimension: dimension as u32,
            rows,
        }
    }
}

impl VectorSource for Rows {
    fn dimension(&self) -> u32 {
        self.dimension
    }
    fn len(&self) -> usize {
        self.rows.len()
    }
    fn for_each(&self, visit: &mut RowVisitor<'_>) -> Result<()> {
        for (row, bytes, norm) in &self.rows {
            visit(*row, bytes, *norm)?;
        }
        Ok(())
    }
    fn vector(&self, row: RowId) -> Option<(&[u8], f32)> {
        self.rows
            .iter()
            .find(|(r, _, _)| *r == row)
            .map(|(_, b, n)| (b.as_slice(), *n))
    }
}

struct Dead(Vec<u32>);

impl LiveSet for Dead {
    fn is_live(&self, row: RowId) -> bool {
        !self.0.contains(&row.row())
    }
}

struct OnlyEven;

impl RowPredicate for OnlyEven {
    fn matches(&self, row: RowId) -> Result<bool> {
        Ok(row.row() % 2 == 0)
    }
}

fn search(
    index: &dyn VectorIndex,
    source: &Rows,
    query: &[f32],
    k: usize,
    live: &dyn LiveSet,
    filter: Option<&dyn RowPredicate>,
    budget: &Budget,
) -> Result<Vec<u32>> {
    index.prepare(source, Metric::Cosine, &NoSnapshots)?;
    let ctx = SearchCtx {
        query,
        top_k: k,
        metric: Metric::Cosine,
        source,
        live,
        filter,
        min_score: None,
        params: SearchParams::default(),
        budget,
    };
    let mut top = TopK::new(k);
    index.search(&ctx, &mut top)?;
    Ok(top.into_sorted().into_iter().map(|c| c.row.row()).collect())
}

fn simple(index: &dyn VectorIndex, source: &Rows, query: &[f32], k: usize) -> Vec<u32> {
    search(
        index,
        source,
        query,
        k,
        &AllLive,
        None,
        &Budget::unlimited(),
    )
    .unwrap()
}

#[test]
fn it_reports_itself_as_approximate() {
    let index = HnswIndex::new();
    assert_eq!(index.kind(), IndexKind::Hnsw);
    assert!(
        !index.is_exact(),
        "an approximate result presented as exact is a correctness bug, not a performance one"
    );
}

/// Two builds of the same data must produce the same answers.
///
/// This is what the hashed level assignment and the total ordering on candidates are for. Without
/// them a recall figure could not be reproduced and a bad graph could not be recreated.
#[test]
fn the_graph_is_deterministic() {
    let source = Rows::random(800, 32, 0xD00D);
    let query: Vec<f32> = (0..32).map(|i| (i as f32 * 0.031).sin()).collect();

    let first = simple(&HnswIndex::new(), &source, &query, 20);
    for _ in 0..3 {
        let again = simple(&HnswIndex::new(), &source, &query, 20);
        assert_eq!(first, again, "the same data gave different results");
    }
}

/// A different seed must build a different graph, or the parameter is decorative.
///
/// Asserted on the structure, not the results. Comparing answers was the obvious thing to try
/// and it was wrong: two well-built graphs over the same 800 points both return the true top
/// twenty, so they agree exactly — which is the property we want, not evidence that the seed
/// does nothing. The seed decides which layer each node lands in, so the edge count is where it
/// shows up.
#[test]
fn the_seed_actually_changes_the_graph() {
    let source = Rows::random(800, 32, 0xD00D);
    let query: Vec<f32> = (0..32).map(|i| (i as f32 * 0.031).sin()).collect();

    let mut footprints = Vec::new();
    for seed in [1u64, 999, 123_456] {
        let index = HnswIndex::with_params(HnswParams::default().with_seed(seed));
        let _ = simple(&index, &source, &query, 20);
        footprints.push(index.stats().memory_bytes);
    }
    assert!(
        footprints.iter().any(|f| *f != footprints[0]),
        "three seeds produced identical graphs ({footprints:?}); level assignment is \
         probably ignoring the seed"
    );
}

#[test]
fn deleted_rows_are_never_returned() {
    let source = Rows::random(500, 16, 7);
    let index = HnswIndex::new();
    let query: Vec<f32> = (0..16).map(|i| (i as f32 * 0.1).cos()).collect();

    let all = simple(&index, &source, &query, 10);
    let dead = Dead(all.clone());
    let after = search(
        &index,
        &source,
        &query,
        10,
        &dead,
        None,
        &Budget::unlimited(),
    )
    .unwrap();
    for row in &after {
        assert!(!all.contains(row), "row {row} was deleted but came back");
    }
}

#[test]
fn a_filter_is_always_respected() {
    let source = Rows::random(500, 16, 11);
    let index = HnswIndex::new();
    let query: Vec<f32> = (0..16).map(|i| (i as f32 * 0.2).sin()).collect();
    let got = search(
        &index,
        &source,
        &query,
        10,
        &AllLive,
        Some(&OnlyEven),
        &Budget::unlimited(),
    )
    .unwrap();
    assert!(
        !got.is_empty(),
        "a filter matching half the rows found none"
    );
    for row in &got {
        assert_eq!(row % 2, 0, "row {row} does not match the filter");
    }
}

/// A filtered search must still fill `top_k` when enough rows qualify.
///
/// The failure this guards against is subtle: the graph finds `ef` candidates, the filter
/// rejects most of them, and the caller silently gets three results where ten exist. Widening
/// the beam under a filter is what prevents it.
#[test]
fn a_filtered_search_still_fills_the_result_set() {
    let source = Rows::random(2000, 32, 13);
    let index = HnswIndex::new();
    let query: Vec<f32> = (0..32).map(|i| (i as f32 * 0.05).sin()).collect();
    let got = search(
        &index,
        &source,
        &query,
        10,
        &AllLive,
        Some(&OnlyEven),
        &Budget::unlimited(),
    )
    .unwrap();
    assert_eq!(
        got.len(),
        10,
        "half the corpus qualifies; ten results exist"
    );
}

#[test]
fn an_empty_source_is_not_an_error() {
    let source = Rows::random(0, 8, 1);
    let index = HnswIndex::new();
    assert!(simple(&index, &source, &[0.0; 8], 5).is_empty());
}

#[test]
fn a_single_row_is_found() {
    let source = Rows::random(1, 8, 1);
    let index = HnswIndex::new();
    assert_eq!(simple(&index, &source, &[0.1; 8], 5), vec![0]);
}

#[test]
fn asking_for_more_than_exists_returns_everything() {
    let source = Rows::random(7, 8, 3);
    let index = HnswIndex::new();
    assert_eq!(simple(&index, &source, &[0.1; 8], 100).len(), 7);
}

#[test]
fn asking_for_nothing_returns_nothing() {
    let source = Rows::random(50, 8, 3);
    let index = HnswIndex::new();
    assert!(simple(&index, &source, &[0.1; 8], 0).is_empty());
}

/// The graph is rebuilt when the data changes, rather than answering from a stale one.
#[test]
fn a_changed_source_rebuilds_the_graph() {
    let index = HnswIndex::new();
    let small = Rows::random(100, 16, 5);
    let _ = simple(&index, &small, &[0.1; 16], 5);
    assert_eq!(index.rows(), 100);

    let larger = Rows::random(300, 16, 5);
    let got = simple(&index, &larger, &[0.1; 16], 5);
    assert_eq!(index.rows(), 300, "the graph was not rebuilt");
    assert_eq!(got.len(), 5);
}

/// A graph built for one metric must not be used for another: it ranks by the wrong function.
#[test]
fn changing_the_metric_rebuilds_the_graph() {
    let index = HnswIndex::new();
    let source = Rows::random(200, 16, 9);
    let budget = Budget::unlimited();
    let query: Vec<f32> = (0..16).map(|i| (i as f32 * 0.3).cos()).collect();

    for metric in [Metric::Cosine, Metric::L2, Metric::Dot] {
        index.prepare(&source, metric, &NoSnapshots).unwrap();
        let ctx = SearchCtx {
            query: &query,
            top_k: 5,
            metric,
            source: &source,
            live: &AllLive,
            filter: None,
            min_score: None,
            params: SearchParams::default(),
            budget: &budget,
        };
        let mut top = TopK::new(5);
        index.search(&ctx, &mut top).unwrap();
        assert_eq!(top.into_sorted().len(), 5, "{metric:?}");
    }
}

/// An exhausted budget must stop the search rather than being ignored.
#[test]
fn a_budget_is_honoured() {
    let source = Rows::random(3000, 32, 17);
    let index = HnswIndex::new();
    index
        .prepare(&source, Metric::Cosine, &NoSnapshots)
        .unwrap();

    let budget = Budget::with_max_scanned(4);
    let query: Vec<f32> = (0..32).map(|i| (i as f32 * 0.02).sin()).collect();
    let ctx = SearchCtx {
        query: &query,
        top_k: 10,
        metric: Metric::Cosine,
        source: &source,
        live: &AllLive,
        filter: None,
        min_score: None,
        params: SearchParams::default(),
        budget: &budget,
    };
    let mut top = TopK::new(10);
    let result = index.search(&ctx, &mut top);
    assert!(
        matches!(result, Err(DbError::Cancelled)),
        "a tiny budget did not stop the search: {result:?}"
    );
}

/// Statistics must describe the graph that exists, not the one that was asked for.
#[test]
fn stats_describe_the_built_graph() {
    let index = HnswIndex::new();
    let before = index.stats();
    assert_eq!(before.rows, 0);
    assert_eq!(before.kind, IndexKind::Hnsw);

    let source = Rows::random(400, 16, 21);
    let _ = simple(&index, &source, &[0.1; 16], 5);
    let after = index.stats();
    assert_eq!(after.rows, 400);
    assert!(after.memory_bytes > 0, "a built graph occupies memory");
}
