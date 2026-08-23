//! Does the graph actually find the nearest neighbours?
//!
//! An approximate index is only useful if you know how approximate it is. These tests measure
//! recall against the exact scan — the ground truth by definition — on data with cluster
//! structure, because uniform random vectors in high dimensions are nearly equidistant and make
//! every index look good.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::collections::HashSet;

use isha_vector_db_core::document::RowId;
use isha_vector_db_core::error::Result;
use isha_vector_db_core::index::{
    AllLive, Budget, ExactScan, LiveSet, NoSnapshots, RowPredicate, RowVisitor, SearchCtx,
    SearchParams, VectorIndex, VectorSource,
};
use isha_vector_db_core::search::{inv_norm, Metric, TopK};
use isha_vector_db_index_hnsw::{HnswIndex, HnswParams};
use isha_vector_db_testkit::Rng;

/// A source over vectors held in memory.
#[derive(Debug)]
struct Rows {
    dimension: u32,
    rows: Vec<(RowId, Vec<u8>, f32)>,
}

impl Rows {
    fn new(dimension: u32, vectors: &[Vec<f32>]) -> Self {
        let rows = vectors
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                (RowId::new(0, i as u32), bytes, inv_norm(v))
            })
            .collect();
        Self { dimension, rows }
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

/// Clustered vectors: `clusters` centres with points scattered around each.
///
/// Real embeddings have this structure and uniform noise does not. An index that only ever sees
/// uniform data is being flattered — in high dimensions every pair of random points is roughly
/// the same distance apart, so any traversal looks like it is working.
fn clustered(n: usize, dimension: usize, clusters: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng::new(seed);
    let centres: Vec<Vec<f32>> = (0..clusters)
        .map(|_| (0..dimension).map(|_| rng.next_f32() * 2.0 - 1.0).collect())
        .collect();
    (0..n)
        .map(|i| {
            let centre = &centres[i % clusters];
            centre
                .iter()
                .map(|c| c + (rng.next_f32() - 0.5) * 0.35)
                .collect()
        })
        .collect()
}

/// A corpus and a held-out query set drawn from the same distribution.
///
/// Getting this right took two attempts and both failures were instructive. Queries generated
/// independently of the data land in random directions where the whole corpus scores about the
/// same, and the true top ten are separated by under a percent — recall then measures which of a
/// near-tie a method happened to pick. Queries made by nudging an existing vector go the other
/// way: the answer is the point itself and its immediate neighbours, and every method scores a
/// perfect 1.000 however badly it is built.
///
/// What discriminates is what real benchmarks do: generate everything from one distribution and
/// hold some of it back. The queries then sit inside the data's structure without being copies
/// of anything in it.
fn corpus_and_queries(
    n: usize,
    q: usize,
    dimension: usize,
    clusters: usize,
    seed: u64,
) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let mut all = clustered(n + q, dimension, clusters, seed);
    let queries = all.split_off(n);
    (all, queries)
}

fn run(
    index: &dyn VectorIndex,
    source: &Rows,
    query: &[f32],
    metric: Metric,
    k: usize,
) -> Vec<u32> {
    run_filtered(index, source, query, metric, k, &AllLive, None)
}

fn run_filtered(
    index: &dyn VectorIndex,
    source: &Rows,
    query: &[f32],
    metric: Metric,
    k: usize,
    live: &dyn LiveSet,
    filter: Option<&dyn RowPredicate>,
) -> Vec<u32> {
    let budget = Budget::unlimited();
    index.prepare(source, metric, &NoSnapshots).unwrap();
    let ctx = SearchCtx {
        query,
        top_k: k,
        metric,
        source,
        live,
        filter,
        min_score: None,
        params: SearchParams::default(),
        budget: &budget,
    };
    let mut top = TopK::new(k);
    index.search(&ctx, &mut top).unwrap();
    top.into_sorted().into_iter().map(|c| c.row.row()).collect()
}

/// Fraction of the true top-k that the graph also returned, averaged over queries.
fn recall_at(metric: Metric, n: usize, dimension: usize, k: usize, params: HnswParams) -> f64 {
    let (vectors, queries) = corpus_and_queries(n, 40, dimension, 12, 0xA11CE);
    let source = Rows::new(dimension as u32, &vectors);
    let hnsw = HnswIndex::with_params(params);
    let exact = ExactScan::new();

    let mut total = 0.0;
    for query in &queries {
        let truth: HashSet<u32> = run(&exact, &source, query, metric, k).into_iter().collect();
        let got = run(&hnsw, &source, query, metric, k);
        let hit = got.iter().filter(|r| truth.contains(r)).count();
        total += hit as f64 / truth.len().max(1) as f64;
    }
    total / queries.len() as f64
}

#[test]
fn recall_is_high_for_cosine() {
    let recall = recall_at(Metric::Cosine, 2000, 64, 10, HnswParams::default());
    assert!(
        recall >= 0.95,
        "cosine recall@10 was {recall:.3}, expected at least 0.95"
    );
}

#[test]
fn recall_is_high_for_l2() {
    let recall = recall_at(Metric::L2, 2000, 64, 10, HnswParams::default());
    assert!(
        recall >= 0.95,
        "L2 recall@10 was {recall:.3}, expected at least 0.95"
    );
}

#[test]
fn recall_is_high_for_dot() {
    let recall = recall_at(Metric::Dot, 2000, 64, 10, HnswParams::default());
    assert!(
        recall >= 0.90,
        "dot recall@10 was {recall:.3}, expected at least 0.90"
    );
}

/// The knob has to do something, or it is a lie in the API.
#[test]
fn a_wider_construction_beam_does_not_reduce_recall() {
    let narrow = recall_at(
        Metric::Cosine,
        2000,
        32,
        10,
        HnswParams::default().with_ef_construction(8).with_m(4),
    );
    let wide = recall_at(
        Metric::Cosine,
        2000,
        32,
        10,
        HnswParams::default().with_ef_construction(200).with_m(16),
    );
    assert!(
        wide >= narrow,
        "wider construction gave worse recall: {wide:.3} < {narrow:.3}"
    );
    // And the narrow setting must actually be worse, otherwise this test would pass with the
    // parameter ignored entirely.
    assert!(
        narrow < 0.999,
        "the deliberately-poor setting still gave perfect recall ({narrow:.3}); \
         ef_construction and m are probably not being used"
    );
}

/// Print the recall/ef trade-off. Not an assertion — the numbers are the deliverable, and they
/// belong in the documentation rather than only in a threshold.
#[test]
#[ignore = "reporting, not a check; run with --ignored"]
fn report_the_recall_curve() {
    for &(n, d) in &[(2000usize, 64usize), (10_000, 128)] {
        for ef in [16usize, 32, 64, 128, 256] {
            let r = recall_at(
                Metric::Cosine,
                n,
                d,
                10,
                HnswParams::default().with_ef_search(ef),
            );
            println!("n={n:6} d={d:4} ef_search={ef:4} recall@10={r:.3}");
        }
    }
}
