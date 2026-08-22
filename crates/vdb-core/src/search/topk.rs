//! Bounded top-K selection.
//!
//! A binary min-heap of at most `k` entries: the worst candidate kept is at the root, so
//! deciding whether a new candidate belongs is one comparison, and inserting is `O(log k)`.
//! Total cost is `O(n log k)` rather than the `O(n log n)` of sorting everything, which matters
//! because `n` is every vector in the collection and `k` is usually ten.
//!
//! # Determinism
//!
//! Equal scores are broken by ascending [`RowId`], so the same query against the same database
//! returns the same rows in the same order, every time.
//!
//! Note what this does *not* do, because it is easy to expect otherwise: two vectors that are
//! mathematically tied usually do not produce bit-identical scores. `[1, 1]` and `[10, 10]` both
//! have a cosine of exactly 1 against `[1, 1]`, but computed through different magnitudes they
//! come out as `0.99999994` and `1.0000001`, so the tie-break never fires and the larger one
//! ranks first. That is deterministic — the same arithmetic gives the same answer on every
//! machine — merely not intuitive.
//!
//! An earlier version of this file quantised scores before comparing them, to absorb exactly
//! that kind of rounding difference. It was removed: it does not help here (the difference is
//! reproducible, not noise), and no quantisation can be both transitive and free of bucket-edge
//! artefacts — the two values above straddle an exponent boundary, so they land in different
//! buckets however the buckets are drawn. Cross-architecture bit-identity, which is a real
//! concern once SIMD kernels land and different lane counts sum in different orders, is
//! addressed where it arises: by the opt-in deterministic-kernel schedule in ADR-0013, not by
//! blurring the comparison here.
//!
//! One honest caveat, documented rather than hidden: when *more* than `k` documents tie at the
//! cutoff, which of them is returned is decided by row order. That is stable for a given
//! database, but it is unrelated to id order and can change after a compaction rewrites rows.
//! Any tie-break at a cutoff is arbitrary; this one is at least reproducible.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use crate::document::RowId;

/// One candidate.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// The row.
    pub row: RowId,
    /// Its score, higher being better.
    pub score: f32,
}

/// Ordering used inside the heap.
///
/// Deliberately inverted: `BinaryHeap` is a max-heap, and we want the *worst* candidate at the
/// root so it can be evicted. Within equal scores, the larger row compares "greater" so it is
/// the one evicted first, which keeps the smaller row and makes the result deterministic.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Worst(Candidate);

impl Eq for Worst {}

impl Ord for Worst {
    fn cmp(&self, other: &Self) -> Ordering {
        // NaN cannot reach here — vectors are validated finite on write and the kernels cannot
        // manufacture one from finite input — but total_cmp keeps this a total order regardless,
        // rather than risking a panic or an inconsistent heap if one ever did.
        match other.0.score.total_cmp(&self.0.score) {
            Ordering::Equal => self.0.row.cmp(&other.0.row),
            ordering => ordering,
        }
    }
}

impl PartialOrd for Worst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Collects the best `k` candidates seen.
#[derive(Debug)]
pub struct TopK {
    k: usize,
    heap: BinaryHeap<Worst>,
    min_score: Option<f32>,
    considered: u64,
}

impl TopK {
    /// Keep the best `k`. A `k` of zero collects nothing.
    pub fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::with_capacity(k.min(1024)),
            min_score: None,
            considered: 0,
        }
    }

    /// Discard candidates scoring below `min_score`, inclusive of the threshold itself.
    #[must_use]
    pub fn with_min_score(mut self, min_score: Option<f32>) -> Self {
        self.min_score = min_score;
        self
    }

    /// How many candidates were offered, including those rejected.
    pub fn considered(&self) -> u64 {
        self.considered
    }

    /// Candidates currently kept.
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Whether nothing has been kept.
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Whether the collector is full, so the cutoff is meaningful.
    pub fn is_full(&self) -> bool {
        self.heap.len() >= self.k
    }

    /// The lowest score currently kept, once full.
    ///
    /// An index can use this to prune: a candidate that cannot beat this cannot be in the
    /// result. `None` until the collector is full, because until then everything qualifies.
    pub fn cutoff(&self) -> Option<f32> {
        if self.is_full() {
            self.heap.peek().map(|w| w.0.score)
        } else {
            None
        }
    }

    /// Offer a candidate. Returns whether it was kept.
    pub fn offer(&mut self, row: RowId, score: f32) -> bool {
        self.considered += 1;
        if self.k == 0 {
            return false;
        }
        if let Some(min) = self.min_score {
            if score < min {
                return false;
            }
        }
        let candidate = Worst(Candidate { row, score });
        if self.heap.len() < self.k {
            self.heap.push(candidate);
            return true;
        }
        // The root is the worst kept. Replace it only if the newcomer beats it under the same
        // total order the heap uses, so tie-breaking at the cutoff stays consistent.
        match self.heap.peek() {
            Some(worst) if candidate < *worst => {
                self.heap.pop();
                self.heap.push(candidate);
                true
            }
            _ => false,
        }
    }

    /// Drain into a vector sorted best-first, ties broken by ascending row.
    pub fn into_sorted(self) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = self.heap.into_iter().map(|w| w.0).collect();
        out.sort_by(|a, b| match b.score.total_cmp(&a.score) {
            Ordering::Equal => a.row.cmp(&b.row),
            ordering => ordering,
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(i: u32) -> RowId {
        RowId::new(0, i)
    }

    #[test]
    fn keeps_the_highest_scores() {
        let mut top = TopK::new(3);
        for (i, score) in [0.1f32, 0.9, 0.5, 0.7, 0.2].into_iter().enumerate() {
            top.offer(row(i as u32), score);
        }
        let out = top.into_sorted();
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|c| c.score).collect::<Vec<_>>(),
            vec![0.9, 0.7, 0.5]
        );
    }

    #[test]
    fn returns_everything_when_fewer_candidates_than_k() {
        let mut top = TopK::new(10);
        top.offer(row(0), 1.0);
        top.offer(row(1), 2.0);
        let out = top.into_sorted();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].score, 2.0);
    }

    #[test]
    fn an_empty_collector_yields_nothing() {
        assert!(TopK::new(5).into_sorted().is_empty());
        assert!(TopK::new(0).into_sorted().is_empty());
    }

    /// A `k` of zero must collect nothing rather than panicking or keeping one.
    #[test]
    fn zero_k_keeps_nothing() {
        let mut top = TopK::new(0);
        assert!(!top.offer(row(0), 1.0));
        assert!(top.is_empty());
        assert_eq!(top.considered(), 1);
    }

    /// Determinism: equal scores resolve by ascending row, in a fixed order, every time.
    #[test]
    fn ties_break_on_ascending_row() {
        let mut top = TopK::new(3);
        for i in [7u32, 2, 9, 1, 5] {
            top.offer(row(i), 0.5);
        }
        let out = top.into_sorted();
        assert_eq!(
            out.iter().map(|c| c.row.row()).collect::<Vec<_>>(),
            vec![1, 2, 5]
        );
    }

    #[test]
    fn the_same_candidates_in_any_order_give_the_same_result() {
        let scores: Vec<(u32, f32)> = (0..50)
            .map(|i| (i, ((i * 37) % 11) as f32 / 11.0))
            .collect();

        let mut forward = TopK::new(7);
        for (i, s) in &scores {
            forward.offer(row(*i), *s);
        }
        let mut backward = TopK::new(7);
        for (i, s) in scores.iter().rev() {
            backward.offer(row(*i), *s);
        }
        assert_eq!(forward.into_sorted(), backward.into_sorted());
    }

    #[test]
    fn min_score_is_inclusive() {
        let mut top = TopK::new(10).with_min_score(Some(0.5));
        assert!(top.offer(row(0), 0.5), "the threshold itself must qualify");
        assert!(!top.offer(row(1), 0.49));
        assert!(top.offer(row(2), 0.6));
        let out = top.into_sorted();
        assert_eq!(out.len(), 2);
        assert_eq!(top_scores(&out), vec![0.6, 0.5]);
        // Rejected candidates still count as considered, which is what makes the statistic
        // useful for judging filter selectivity.
    }

    #[test]
    fn considered_counts_every_offer() {
        let mut top = TopK::new(1).with_min_score(Some(10.0));
        for i in 0..25 {
            top.offer(row(i), 0.0);
        }
        assert_eq!(top.considered(), 25);
        assert!(top.is_empty());
    }

    #[test]
    fn the_cutoff_appears_only_once_full_and_tracks_the_worst_kept() {
        let mut top = TopK::new(2);
        assert_eq!(top.cutoff(), None);
        top.offer(row(0), 1.0);
        assert_eq!(top.cutoff(), None, "not full yet");
        top.offer(row(1), 3.0);
        assert_eq!(top.cutoff(), Some(1.0));
        top.offer(row(2), 2.0);
        assert_eq!(
            top.cutoff(),
            Some(2.0),
            "the cutoff should rise as better rows arrive"
        );
        assert!(top.is_full());
    }

    /// Mathematically-tied vectors of different magnitude are not bit-identical, and rank by
    /// their computed scores rather than by row. Pinned so the behaviour is a decision rather
    /// than an accident.
    #[test]
    fn scores_that_differ_by_rounding_still_rank_by_score() {
        let mut top = TopK::new(2);
        top.offer(row(5), 1.000_000_1);
        top.offer(row(2), 0.999_999_94);
        let out = top.into_sorted();
        assert_eq!(
            out[0].row.row(),
            5,
            "the marginally higher score ranks first"
        );

        // Whatever the order of arrival.
        let mut reversed = TopK::new(2);
        reversed.offer(row(2), 0.999_999_94);
        reversed.offer(row(5), 1.000_000_1);
        assert_eq!(reversed.into_sorted()[0].row.row(), 5);
    }

    #[test]
    fn exactly_equal_scores_do_resolve_by_row() {
        let mut top = TopK::new(2);
        top.offer(row(9), 0.5);
        top.offer(row(3), 0.5);
        assert_eq!(top.into_sorted()[0].row.row(), 3);
    }

    #[test]
    fn negative_scores_order_correctly() {
        // L2 produces negative scores throughout, so this is the normal case for that metric.
        let mut top = TopK::new(2);
        top.offer(row(0), -100.0);
        top.offer(row(1), -1.0);
        top.offer(row(2), -50.0);
        let out = top.into_sorted();
        assert_eq!(top_scores(&out), vec![-1.0, -50.0]);
    }

    #[test]
    fn rows_from_different_segments_order_by_segment_then_row() {
        let mut top = TopK::new(3);
        top.offer(RowId::new(1, 0), 0.5);
        top.offer(RowId::new(0, 9), 0.5);
        top.offer(RowId::new(0, 1), 0.5);
        let out = top.into_sorted();
        assert_eq!(
            out.iter()
                .map(|c| (c.row.segment(), c.row.row()))
                .collect::<Vec<_>>(),
            vec![(0, 1), (0, 9), (1, 0)]
        );
    }

    /// Not reachable through the public API — vectors are validated finite — but the heap must
    /// remain a total order regardless, rather than panicking or corrupting its invariant.
    #[test]
    fn a_nan_score_does_not_break_the_heap() {
        let mut top = TopK::new(2);
        top.offer(row(0), f32::NAN);
        top.offer(row(1), 1.0);
        top.offer(row(2), 2.0);
        let out = top.into_sorted();
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|c| c.score == 2.0));
    }

    fn top_scores(out: &[Candidate]) -> Vec<f32> {
        out.iter().map(|c| c.score).collect()
    }
}
