//! Distance kernels and the scoring contract.
//!
//! # One rule: `score` is always higher-is-better
//!
//! Whatever the metric, a larger score means a better match. That single rule means top-K
//! selection, `min_score` thresholding, heap ordering and tie-breaking each have exactly one
//! implementation, and no index has to know which metrics invert.
//!
//! | metric   | `score`                        | `distance`            |
//! |----------|--------------------------------|-----------------------|
//! | `Cosine` | cosine similarity, `[-1, 1]`   | `1 - similarity`      |
//! | `Dot`    | inner product, unbounded       | none is defined       |
//! | `L2`     | `-squared_l2`                  | `sqrt(squared_l2)`    |
//!
//! Squared L2 is used throughout the inner loop; the square root happens once per returned hit,
//! not once per candidate. Negating it is what keeps the "higher is better" rule total.
//!
//! # Kernels take the candidate as bytes
//!
//! A stored vector arrives as `&[u8]` from a segment file, and this crate forbids `unsafe`, so
//! it cannot be reinterpreted as `&[f32]` without a copy. The kernels therefore decode as they
//! go, in a shape that autovectorises reasonably. The SIMD versions — and the audited pointer
//! cast that makes them zero-copy — belong in `vdb-index-flat`, where `unsafe` is permitted and
//! every kernel is differential-tested against the scalar reference here.

pub use vdb_format::Metric;

/// Bytes per `f32` component.
const F32_SIZE: usize = 4;

/// Inner product of a query and a stored row.
///
/// Components beyond the shorter of the two are ignored rather than panicking; callers validate
/// dimensions before they get here, and a scan is not the place to re-check on every row.
pub fn dot_bytes(query: &[f32], row: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    for (q, chunk) in query.iter().zip(row.chunks_exact(F32_SIZE)) {
        if let Ok(bytes) = <[u8; F32_SIZE]>::try_from(chunk) {
            sum += q * f32::from_le_bytes(bytes);
        }
    }
    sum
}

/// Squared Euclidean distance between a query and a stored row.
pub fn l2_squared_bytes(query: &[f32], row: &[u8]) -> f32 {
    let mut sum = 0.0f32;
    for (q, chunk) in query.iter().zip(row.chunks_exact(F32_SIZE)) {
        if let Ok(bytes) = <[u8; F32_SIZE]>::try_from(chunk) {
            let d = q - f32::from_le_bytes(bytes);
            sum += d * d;
        }
    }
    sum
}

/// Inner product of two float slices. The reference kernel.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Squared Euclidean distance between two float slices. The reference kernel.
pub fn l2_squared(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// A query prepared for repeated scoring against many rows.
///
/// Holds the query's reciprocal norm so cosine costs a dot product and two multiplies per
/// candidate rather than a square root.
#[derive(Debug, Clone, Copy)]
pub struct Scorer<'a> {
    metric: Metric,
    query: &'a [f32],
    query_inv_norm: f32,
}

impl<'a> Scorer<'a> {
    /// Prepare a query.
    pub fn new(metric: Metric, query: &'a [f32]) -> Self {
        let norm = query.iter().map(|v| v * v).sum::<f32>().sqrt();
        // A zero-length query has no direction, so its cosine against anything is 0 rather than
        // NaN. Same convention as a stored zero vector.
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

    /// The metric this scorer applies.
    pub fn metric(&self) -> Metric {
        self.metric
    }

    /// The query vector.
    pub fn query(&self) -> &'a [f32] {
        self.query
    }

    /// Score a stored row. `row_inv_norm` is the value cached in the row directory, and is
    /// ignored for metrics that do not need it.
    pub fn score_bytes(&self, row: &[u8], row_inv_norm: f32) -> f32 {
        match self.metric {
            Metric::Cosine => dot_bytes(self.query, row) * self.query_inv_norm * row_inv_norm,
            Metric::Dot => dot_bytes(self.query, row),
            Metric::L2 => -l2_squared_bytes(self.query, row),
            // `Metric` is #[non_exhaustive]; a metric this build does not know scores nothing
            // rather than silently ranking by the wrong function.
            _ => f32::NEG_INFINITY,
        }
    }

    /// Score against float components, for tests and the reference path.
    pub fn score(&self, row: &[f32], row_inv_norm: f32) -> f32 {
        match self.metric {
            Metric::Cosine => dot(self.query, row) * self.query_inv_norm * row_inv_norm,
            Metric::Dot => dot(self.query, row),
            Metric::L2 => -l2_squared(self.query, row),
            _ => f32::NEG_INFINITY,
        }
    }
}

/// The metric-native distance for a score, where one is defined.
///
/// `None` for the inner product: it is a similarity with no corresponding distance, and
/// inventing one would be worse than admitting it.
pub fn distance_from_score(metric: Metric, score: f32) -> Option<f32> {
    match metric {
        Metric::Cosine => Some(1.0 - score),
        Metric::L2 => Some((-score).max(0.0).sqrt()),
        Metric::Dot => None,
        _ => None,
    }
}

/// The reciprocal of a vector's L2 norm, zero for the zero vector.
pub fn inv_norm(values: &[f32]) -> f32 {
    let n = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if n > 0.0 && n.is_finite() {
        1.0 / n
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    #[test]
    fn byte_kernels_agree_with_the_float_reference() {
        let cases: [(&[f32], &[f32]); 5] = [
            (&[1.0, 2.0, 3.0], &[4.0, 5.0, 6.0]),
            (&[0.0, 0.0], &[0.0, 0.0]),
            (&[-1.5, 2.25], &[3.0, -4.0]),
            (&[1e-8, 1e8], &[1e8, 1e-8]),
            (&[1.0], &[1.0]),
        ];
        for (a, b) in cases {
            let raw = bytes(b);
            assert!(
                (dot(a, b) - dot_bytes(a, &raw)).abs() < 1e-3,
                "dot for {a:?}·{b:?}"
            );
            assert!(
                (l2_squared(a, b) - l2_squared_bytes(a, &raw)).abs() < 1e-3,
                "l2 for {a:?}·{b:?}"
            );
        }
    }

    #[test]
    fn cosine_of_a_vector_with_itself_is_one() {
        let v = [3.0f32, 4.0, 0.0, -1.0];
        let s = Scorer::new(Metric::Cosine, &v);
        let score = s.score_bytes(&bytes(&v), inv_norm(&v));
        assert!((score - 1.0).abs() < 1e-5, "self-similarity was {score}");
        assert!(distance_from_score(Metric::Cosine, score).unwrap().abs() < 1e-5);
    }

    #[test]
    fn cosine_ignores_magnitude() {
        let q = [1.0f32, 0.0];
        let near = [5.0f32, 0.0];
        let s = Scorer::new(Metric::Cosine, &q);
        assert!((s.score_bytes(&bytes(&near), inv_norm(&near)) - 1.0).abs() < 1e-5);

        let orthogonal = [0.0f32, 7.0];
        assert!(
            s.score_bytes(&bytes(&orthogonal), inv_norm(&orthogonal))
                .abs()
                < 1e-5
        );

        let opposite = [-2.0f32, 0.0];
        assert!((s.score_bytes(&bytes(&opposite), inv_norm(&opposite)) + 1.0).abs() < 1e-5);
    }

    /// The zero vector has no direction, so scoring it must not produce NaN — which would then
    /// poison every comparison it takes part in.
    #[test]
    fn the_zero_vector_scores_zero_rather_than_nan() {
        let zero = [0.0f32, 0.0, 0.0];
        let other = [1.0f32, 2.0, 3.0];

        let s = Scorer::new(Metric::Cosine, &other);
        let score = s.score_bytes(&bytes(&zero), inv_norm(&zero));
        assert_eq!(score, 0.0);
        assert!(score.is_finite());

        let s = Scorer::new(Metric::Cosine, &zero);
        let score = s.score_bytes(&bytes(&other), inv_norm(&other));
        assert_eq!(score, 0.0);
        assert!(score.is_finite());
    }

    /// The contract that makes every metric usable by one selection algorithm.
    #[test]
    fn a_nearer_vector_always_scores_higher_whatever_the_metric() {
        let query = [1.0f32, 0.0, 0.0];
        let near = [0.9f32, 0.1, 0.0];
        let far = [-1.0f32, 0.0, 0.0];

        for metric in Metric::ALL {
            let s = Scorer::new(metric, &query);
            let near_score = s.score_bytes(&bytes(&near), inv_norm(&near));
            let far_score = s.score_bytes(&bytes(&far), inv_norm(&far));
            assert!(
                near_score > far_score,
                "{metric:?}: near scored {near_score}, far scored {far_score}"
            );
        }
    }

    #[test]
    fn l2_score_is_negated_distance_and_converts_back() {
        let query = [0.0f32, 0.0];
        let point = [3.0f32, 4.0];
        let s = Scorer::new(Metric::L2, &query);
        let score = s.score_bytes(&bytes(&point), 1.0);
        assert!(
            (score + 25.0).abs() < 1e-4,
            "score should be -squared_l2, got {score}"
        );
        let d = distance_from_score(Metric::L2, score).unwrap();
        assert!(
            (d - 5.0).abs() < 1e-4,
            "distance should be the true Euclidean 5.0, got {d}"
        );
    }

    #[test]
    fn the_inner_product_has_no_distance() {
        assert_eq!(distance_from_score(Metric::Dot, 7.0), None);
    }

    #[test]
    fn cosine_distance_spans_zero_to_two() {
        for (a, b, expected) in [
            ([1.0f32, 0.0], [1.0f32, 0.0], 0.0f32),
            ([1.0, 0.0], [0.0, 1.0], 1.0),
            ([1.0, 0.0], [-1.0, 0.0], 2.0),
        ] {
            let s = Scorer::new(Metric::Cosine, &a);
            let score = s.score_bytes(&bytes(&b), inv_norm(&b));
            let d = distance_from_score(Metric::Cosine, score).unwrap();
            assert!(
                (d - expected).abs() < 1e-5,
                "{a:?} vs {b:?}: distance {d}, want {expected}"
            );
        }
    }

    #[test]
    fn inv_norm_matches_the_definition_and_handles_zero() {
        assert!((inv_norm(&[3.0, 4.0]) - 0.2).abs() < 1e-6);
        assert_eq!(inv_norm(&[0.0, 0.0]), 0.0);
        assert_eq!(inv_norm(&[]), 0.0);
    }

    #[test]
    fn a_short_row_is_scored_over_what_it_has_rather_than_panicking() {
        // Dimensions are validated before a scan, so this can only happen with a corrupt
        // segment — where returning a poor score beats taking the process down.
        let q = [1.0f32, 1.0, 1.0];
        let short = bytes(&[1.0, 1.0]);
        let s = Scorer::new(Metric::Dot, &q);
        assert_eq!(s.score_bytes(&short, 1.0), 2.0);
    }
}
