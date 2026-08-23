//! AVX2 kernels.
//!
//! Eight lanes per register, two accumulators, for the same reason as NEON: a fused
//! multiply-add has latency, and one accumulator chain serialises on it.
//!
//! Loads are unaligned (`loadu`). The slices reaching here are 4-byte aligned but not
//! necessarily 32-byte aligned, and on every microarchitecture that supports AVX2 an unaligned
//! load that does not straddle a cache line costs the same as an aligned one. Requiring 32-byte
//! alignment would mean a peeling loop for a benefit that no longer exists.
//!
//! **Untested on real hardware in this repository** — development is on aarch64, so these paths
//! are exercised by the CI x86 matrix. That is exactly why the differential tests in the parent
//! module compare every kernel against the scalar reference at every length: correctness here
//! rests on those, not on someone having run it locally.

use core::arch::x86_64::{
    __m256, _mm256_add_ps, _mm256_castps256_ps128, _mm256_extractf128_ps, _mm256_fmadd_ps,
    _mm256_loadu_ps, _mm256_setzero_ps, _mm256_sub_ps, _mm_add_ps, _mm_cvtss_f32, _mm_movehl_ps,
    _mm_shuffle_ps,
};

/// Inner product.
///
/// # Safety
/// The caller must have verified that AVX2 and FMA are available. The loop reads only
/// `a.len().min(b.len())` elements from each slice.
#[target_feature(enable = "avx2,fma")]
pub(crate) unsafe fn dot_avx2(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: the preconditions in this function's doc comment hold for
    // every call; the per-statement reasoning is in the comments below.
    unsafe {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();

        let mut i = 0;
        while i + 16 <= n {
            // SAFETY: `i + 16 <= n`, so all sixteen elements are inside both slices.
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc0);
            acc1 = _mm256_fmadd_ps(
                _mm256_loadu_ps(pa.add(i + 8)),
                _mm256_loadu_ps(pb.add(i + 8)),
                acc1,
            );
            i += 16;
        }
        if i + 8 <= n {
            // SAFETY: `i + 8 <= n`.
            acc0 = _mm256_fmadd_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)), acc0);
            i += 8;
        }

        let mut sum = horizontal(_mm256_add_ps(acc0, acc1));
        while i < n {
            // SAFETY: `i < n`.
            sum += *pa.add(i) * *pb.add(i);
            i += 1;
        }
        sum
    }
}

/// Squared Euclidean distance.
///
/// # Safety
/// As [`dot_avx2`].
#[target_feature(enable = "avx2,fma")]
pub(crate) unsafe fn l2_squared_avx2(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: the preconditions in this function's doc comment hold for
    // every call; the per-statement reasoning is in the comments below.
    unsafe {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = _mm256_setzero_ps();
        let mut acc1 = _mm256_setzero_ps();

        let mut i = 0;
        while i + 16 <= n {
            // SAFETY: `i + 16 <= n`.
            let d0 = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            acc0 = _mm256_fmadd_ps(d0, d0, acc0);
            let d1 = _mm256_sub_ps(
                _mm256_loadu_ps(pa.add(i + 8)),
                _mm256_loadu_ps(pb.add(i + 8)),
            );
            acc1 = _mm256_fmadd_ps(d1, d1, acc1);
            i += 16;
        }
        if i + 8 <= n {
            // SAFETY: `i + 8 <= n`.
            let d = _mm256_sub_ps(_mm256_loadu_ps(pa.add(i)), _mm256_loadu_ps(pb.add(i)));
            acc0 = _mm256_fmadd_ps(d, d, acc0);
            i += 8;
        }

        let mut sum = horizontal(_mm256_add_ps(acc0, acc1));
        while i < n {
            // SAFETY: `i < n`.
            let d = *pa.add(i) - *pb.add(i);
            sum += d * d;
            i += 1;
        }
        sum
    }
}

/// Sum the eight lanes.
///
/// # Safety
/// The caller must have verified AVX2. Operates on a register value; no memory is touched.
#[target_feature(enable = "avx2")]
unsafe fn horizontal(v: __m256) -> f32 {
    // `target_feature` already makes these calls safe within the function body; no block needed.
    {
        // Fold the upper 128 bits onto the lower, then fold within.
        let lo = _mm256_castps256_ps128(v);
        let hi = _mm256_extractf128_ps(v, 1);
        let sum = _mm_add_ps(lo, hi);
        let shuf = _mm_movehl_ps(sum, sum);
        let sum = _mm_add_ps(sum, shuf);
        let shuf = _mm_shuffle_ps(sum, sum, 0b01);
        _mm_cvtss_f32(_mm_add_ps(sum, shuf))
    }
}
