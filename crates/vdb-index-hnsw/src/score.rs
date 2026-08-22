//! Scoring for graph traversal.
//!
//! `vdb_core::search::Scorer` is the reference implementation and is deliberately scalar: it
//! lives in a crate that forbids `unsafe` and takes no dependencies. That is right for a
//! definition and wrong for the inner loop of a graph search, which computes tens of millions of
//! distances while building and thousands per query.
//!
//! This routes the same arithmetic through the vectorised kernels in `vdb-index-flat`, which are
//! differentially tested against the scalar reference. Both operate on `f32` slices, because a
//! graph holds decoded vectors rather than the raw row bytes a scan streams.

use vdb_core::search::Metric;
use vdb_index_flat::kernels;

/// A prepared query.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GraphScorer<'a> {
    metric: Metric,
    query: &'a [f32],
    query_inv_norm: f32,
}

impl<'a> GraphScorer<'a> {
    /// Prepare `query` under `metric`.
    pub(crate) fn new(metric: Metric, query: &'a [f32]) -> Self {
        let norm = kernels::dot_f32(query, query).sqrt();
        // A zero-length query has no direction, so its cosine is zero rather than NaN. The same
        // convention as the rest of the engine.
        let query_inv_norm = if norm > 0.0 && norm.is_finite() {
            1.0 / norm
        } else {
            0.0
        };
        Self {
            metric,
            query,
            query_inv_norm,
        }
    }

    /// Score a stored vector. Higher is always better, matching the engine's contract.
    pub(crate) fn score(&self, row: &[f32], row_inv_norm: f32) -> f32 {
        match self.metric {
            Metric::Cosine => {
                kernels::dot_f32(self.query, row) * self.query_inv_norm * row_inv_norm
            }
            Metric::Dot => kernels::dot_f32(self.query, row),
            Metric::L2 => -kernels::l2_squared_f32(self.query, row),
            // `Metric` is non-exhaustive. A metric this build does not know scores nothing rather
            // than silently ranking by the wrong function — the same choice the reference makes.
            _ => f32::NEG_INFINITY,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vdb_core::search::{inv_norm, Scorer};

    /// The fast path must agree with the reference it replaces.
    ///
    /// Without this the graph could rank by something subtly different from the exact scan, and
    /// the only symptom would be recall that never quite reaches 1.0 however wide the beam — a
    /// number everybody attributes to tuning.
    #[test]
    fn it_agrees_with_the_scalar_reference() {
        let mut seed = 42u64;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((seed >> 33) as f32 / (1u32 << 31) as f32) - 0.5
        };
        for dimension in [1usize, 3, 4, 7, 8, 15, 16, 64, 129] {
            let a: Vec<f32> = (0..dimension).map(|_| next()).collect();
            let b: Vec<f32> = (0..dimension).map(|_| next()).collect();
            let bn = inv_norm(&b);
            for metric in [Metric::Cosine, Metric::L2, Metric::Dot] {
                let fast = GraphScorer::new(metric, &a).score(&b, bn);
                let reference = Scorer::new(metric, &a).score(&b, bn);
                let tolerance = reference.abs().max(1.0) * 1e-5;
                assert!(
                    (fast - reference).abs() <= tolerance,
                    "{metric:?} at {dimension} dimensions: {fast} vs {reference}"
                );
            }
        }
    }

    #[test]
    fn a_zero_vector_scores_zero_rather_than_nan() {
        let zero = vec![0.0f32; 8];
        let other = vec![1.0f32; 8];
        let s = GraphScorer::new(Metric::Cosine, &zero).score(&other, inv_norm(&other));
        assert_eq!(s, 0.0);
        assert!(!s.is_nan());
    }
}
