//! The accelerated exact scan.
//!
//! The *reference* exact scan lives in `vdb_core::index::ExactScan`: it is the correctness
//! baseline, it forbids `unsafe`, and it has nothing clever in it to be wrong. This crate is
//! where the fast version goes — the runtime-dispatched AVX2, NEON and `simd128` kernels, and
//! the audited pointer casts that let them read a segment's bytes as floats without copying.
//!
//! Today [`FlatIndex`] delegates to the reference implementation, so the two cannot disagree.
//! What is already here and already earning its place is the test suite: exactness against an
//! independently computed ranking, per-metric ranking behaviour, tie-breaking, dead rows,
//! filters, thresholds and cancellation. That suite is what will validate the SIMD kernels when
//! they land, and it is the reason this crate exists before the kernels do rather than after.
//!
//! # Why brute force at all
//!
//! Below roughly a hundred thousand vectors on a phone it is genuinely the right choice: a scan
//! is a sequential read at memory bandwidth, with no graph to build, no parameters to tune, no
//! recall to lose and no rebuild after deletes. An approximate index only starts to win once
//! the scan stops fitting in the latency budget — and even then, this remains the ground truth
//! its recall is measured against.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![warn(missing_docs)]

use vdb_core::error::Result;
use vdb_core::index::{ExactScan, IndexKind, IndexStats, SearchCtx, VectorIndex};
use vdb_core::search::TopK;

/// An exact, scan-based index.
#[derive(Debug, Default, Clone, Copy)]
pub struct FlatIndex {
    reference: ExactScan,
}

impl FlatIndex {
    /// Create one. It holds no state.
    pub const fn new() -> Self {
        Self {
            reference: ExactScan::new(),
        }
    }
}

impl VectorIndex for FlatIndex {
    fn kind(&self) -> IndexKind {
        IndexKind::Flat
    }

    fn is_exact(&self) -> bool {
        true
    }

    fn search(&self, ctx: &SearchCtx<'_>, out: &mut TopK) -> Result<()> {
        // Delegated until the accelerated kernels land. Keeping one implementation means the
        // fast path cannot silently diverge from the baseline before it even exists.
        self.reference.search(ctx, out)
    }

    fn stats(&self) -> IndexStats {
        IndexStats::new(IndexKind::Flat, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vdb_core::document::RowId;
    use vdb_core::index::{AllLive, Budget, LiveSet, RowPredicate, SearchParams, VectorSource};
    use vdb_core::search::{inv_norm, Metric, Scorer};

    /// A source backed by a plain vector of rows, for testing the index in isolation.
    #[derive(Debug)]
    struct Rows {
        dimension: u32,
        rows: Vec<(RowId, Vec<u8>, f32)>,
    }

    impl Rows {
        fn new(dimension: u32, vectors: &[&[f32]]) -> Self {
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
        fn for_each(&self, visit: &mut vdb_core::index::RowVisitor<'_>) -> Result<()> {
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

    fn search(
        source: &Rows,
        query: &[f32],
        metric: Metric,
        k: usize,
        live: &dyn LiveSet,
        filter: Option<&dyn RowPredicate>,
        min_score: Option<f32>,
    ) -> Vec<(u32, f32)> {
        let budget = Budget::unlimited();
        let ctx = SearchCtx {
            query,
            top_k: k,
            metric,
            source,
            live,
            filter,
            min_score,
            params: SearchParams::default(),
            budget: &budget,
        };
        let mut top = TopK::new(k).with_min_score(min_score);
        FlatIndex::new().search(&ctx, &mut top).unwrap();
        top.into_sorted()
            .into_iter()
            .map(|c| (c.row.row(), c.score))
            .collect()
    }

    fn corpus() -> Rows {
        Rows::new(
            2,
            &[
                &[1.0, 0.0],  // 0
                &[0.0, 1.0],  // 1
                &[-1.0, 0.0], // 2
                &[0.7, 0.7],  // 3
                &[0.9, 0.1],  // 4
            ],
        )
    }

    #[test]
    fn finds_the_nearest_neighbours_by_cosine() {
        let hits = search(
            &corpus(),
            &[1.0, 0.0],
            Metric::Cosine,
            3,
            &AllLive,
            None,
            None,
        );
        assert_eq!(
            hits.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
            vec![0, 4, 3]
        );
        assert!(
            (hits[0].1 - 1.0).abs() < 1e-5,
            "an exact match should score 1.0"
        );
    }

    /// A vector is its own nearest neighbour under cosine and L2. If this ever fails,
    /// everything downstream is meaningless.
    #[test]
    fn every_vector_is_its_own_nearest_neighbour_under_cosine_and_l2() {
        let vectors: Vec<Vec<f32>> = (0..20)
            .map(|i| vec![i as f32, (i * 3 % 7) as f32, 1.0])
            .collect();
        let refs: Vec<&[f32]> = vectors.iter().map(Vec::as_slice).collect();
        let source = Rows::new(3, &refs);

        for metric in [Metric::Cosine, Metric::L2] {
            for (i, v) in vectors.iter().enumerate() {
                let hits = search(&source, v, metric, 1, &AllLive, None, None);
                assert_eq!(
                    hits[0].0, i as u32,
                    "{metric:?}: row {i} did not find itself"
                );
            }
        }
    }

    /// The inner product deliberately does **not** have that property, and users are regularly
    /// surprised by it: `Dot` rewards magnitude as well as direction, so a longer vector
    /// pointing roughly the same way outscores an exact match. That is what the inner product
    /// means, not a defect — but it is why `Cosine` is the sensible default for embeddings,
    /// whose magnitudes usually carry no meaning.
    ///
    /// Pinned as a test so nobody later "fixes" the dot product into a cosine.
    #[test]
    fn the_inner_product_does_not_make_a_vector_its_own_nearest_neighbour() {
        let source = Rows::new(2, &[&[1.0, 1.0], &[10.0, 10.0]]);
        let hits = search(&source, &[1.0, 1.0], Metric::Dot, 1, &AllLive, None, None);
        assert_eq!(hits[0].0, 1, "the longer vector should win under Dot");

        // Under cosine both point the same way, so both score essentially 1 — the magnitude
        // that decided the inner product is irrelevant. They are not *bit*-identical, so which
        // of the two ranks first is decided by rounding rather than by the tie-break; what
        // matters, and what is asserted, is that cosine considers them equally good.
        let hits = search(
            &source,
            &[1.0, 1.0],
            Metric::Cosine,
            2,
            &AllLive,
            None,
            None,
        );
        assert!(
            hits.iter().all(|(_, s)| (s - 1.0).abs() < 1e-5),
            "got {hits:?}"
        );
    }

    #[test]
    fn every_metric_ranks_sensibly() {
        let source = corpus();
        // L2 measures position, so the closest point to (1,0) is itself, then (0.9,0.1).
        let l2 = search(&source, &[1.0, 0.0], Metric::L2, 2, &AllLive, None, None);
        assert_eq!(l2.iter().map(|(r, _)| *r).collect::<Vec<_>>(), vec![0, 4]);
        assert!(
            l2[0].1 <= 0.0,
            "L2 scores are negated distances, so never positive"
        );

        // The inner product rewards magnitude as well as direction.
        let dot = search(&source, &[1.0, 0.0], Metric::Dot, 2, &AllLive, None, None);
        assert_eq!(dot[0].0, 0);
    }

    #[test]
    fn an_empty_source_returns_nothing_rather_than_failing() {
        let empty = Rows::new(2, &[]);
        assert!(search(&empty, &[1.0, 0.0], Metric::Cosine, 5, &AllLive, None, None).is_empty());
    }

    #[test]
    fn asking_for_more_than_exists_returns_everything() {
        let hits = search(
            &corpus(),
            &[1.0, 0.0],
            Metric::Cosine,
            100,
            &AllLive,
            None,
            None,
        );
        assert_eq!(hits.len(), 5);
    }

    #[test]
    fn a_top_k_of_zero_returns_nothing() {
        assert!(search(
            &corpus(),
            &[1.0, 0.0],
            Metric::Cosine,
            0,
            &AllLive,
            None,
            None
        )
        .is_empty());
    }

    #[test]
    fn dead_rows_are_never_returned() {
        let dead = Dead(vec![0, 4]);
        let hits = search(&corpus(), &[1.0, 0.0], Metric::Cosine, 5, &dead, None, None);
        let rows: Vec<u32> = hits.iter().map(|(r, _)| *r).collect();
        assert!(!rows.contains(&0) && !rows.contains(&4), "got {rows:?}");
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn a_filter_excludes_rows_before_they_are_scored() {
        let only_even = |row: RowId| row.row() % 2 == 0;
        let hits = search(
            &corpus(),
            &[1.0, 0.0],
            Metric::Cosine,
            5,
            &AllLive,
            Some(&only_even),
            None,
        );
        assert!(hits.iter().all(|(r, _)| r % 2 == 0), "got {hits:?}");
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn a_filter_that_excludes_everything_returns_nothing() {
        let nothing = |_row: RowId| false;
        assert!(search(
            &corpus(),
            &[1.0, 0.0],
            Metric::Cosine,
            5,
            &AllLive,
            Some(&nothing),
            None
        )
        .is_empty());
    }

    #[test]
    fn min_score_is_applied_and_is_inclusive() {
        let source = corpus();
        let all = search(
            &source,
            &[1.0, 0.0],
            Metric::Cosine,
            10,
            &AllLive,
            None,
            None,
        );
        let cutoff = all[1].1; // the second-best score
        let filtered = search(
            &source,
            &[1.0, 0.0],
            Metric::Cosine,
            10,
            &AllLive,
            None,
            Some(cutoff),
        );
        assert_eq!(filtered.len(), 2, "the threshold itself must qualify");
        assert!(filtered.iter().all(|(_, s)| *s >= cutoff));
    }

    /// Exactness, stated as a property: a scan must agree with a naive sort of every score.
    #[test]
    fn results_match_an_independently_computed_ranking() {
        let mut seed = 0x2024_1234_5678_9abcu64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            ((seed >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
        };
        let vectors: Vec<Vec<f32>> = (0..200).map(|_| (0..8).map(|_| next()).collect()).collect();
        let refs: Vec<&[f32]> = vectors.iter().map(Vec::as_slice).collect();
        let source = Rows::new(8, &refs);
        let query: Vec<f32> = (0..8).map(|_| next()).collect();

        for metric in Metric::ALL {
            let scorer = Scorer::new(metric, &query);
            let mut expected: Vec<(u32, f32)> = vectors
                .iter()
                .enumerate()
                .map(|(i, v)| (i as u32, scorer.score(v, inv_norm(v))))
                .collect();
            expected.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            expected.truncate(10);

            let hits = search(&source, &query, metric, 10, &AllLive, None, None);
            assert_eq!(
                hits.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
                expected.iter().map(|(r, _)| *r).collect::<Vec<_>>(),
                "{metric:?} ranking disagreed with the reference"
            );
        }
    }

    #[test]
    fn identical_vectors_tie_and_resolve_by_row() {
        let source = Rows::new(2, &[&[1.0, 0.0], &[1.0, 0.0], &[1.0, 0.0], &[0.0, 1.0]]);
        let hits = search(
            &source,
            &[1.0, 0.0],
            Metric::Cosine,
            2,
            &AllLive,
            None,
            None,
        );
        assert_eq!(hits.iter().map(|(r, _)| *r).collect::<Vec<_>>(), vec![0, 1]);

        // And repeating the query gives exactly the same answer.
        let again = search(
            &source,
            &[1.0, 0.0],
            Metric::Cosine,
            2,
            &AllLive,
            None,
            None,
        );
        assert_eq!(hits, again);
    }

    #[test]
    fn a_cancelled_search_stops_and_says_so() {
        let vectors: Vec<Vec<f32>> = (0..5000).map(|i| vec![i as f32, 1.0]).collect();
        let refs: Vec<&[f32]> = vectors.iter().map(Vec::as_slice).collect();
        let source = Rows::new(2, &refs);

        let budget = Budget::unlimited();
        budget.cancel();
        let query = [1.0f32, 0.0];
        let ctx = SearchCtx {
            query: &query,
            top_k: 5,
            metric: Metric::Cosine,
            source: &source,
            live: &AllLive,
            filter: None,
            min_score: None,
            params: SearchParams::default(),
            budget: &budget,
        };
        let mut top = TopK::new(5);
        let err = FlatIndex::new().search(&ctx, &mut top).unwrap_err();
        assert!(matches!(err, vdb_core::DbError::Cancelled), "got {err:?}");
    }

    #[test]
    fn a_scan_ceiling_stops_the_search() {
        let vectors: Vec<Vec<f32>> = (0..5000).map(|i| vec![i as f32, 1.0]).collect();
        let refs: Vec<&[f32]> = vectors.iter().map(Vec::as_slice).collect();
        let source = Rows::new(2, &refs);

        let budget = Budget::with_max_scanned(2000);
        let query = [1.0f32, 0.0];
        let ctx = SearchCtx {
            query: &query,
            top_k: 5,
            metric: Metric::Cosine,
            source: &source,
            live: &AllLive,
            filter: None,
            min_score: None,
            params: SearchParams::default(),
            budget: &budget,
        };
        let mut top = TopK::new(5);
        assert!(FlatIndex::new().search(&ctx, &mut top).is_err());
        assert!(budget.scanned() >= 2000);
    }

    #[test]
    fn it_declares_itself_exact() {
        let index = FlatIndex::new();
        assert!(index.is_exact());
        assert_eq!(index.kind(), IndexKind::Flat);
        assert_eq!(index.kind().name(), "flat");
        // It holds no rows of its own, which is the point: nothing to keep in sync.
        assert_eq!(index.stats().memory_bytes, 0);
    }
}
