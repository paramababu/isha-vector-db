//! AArch64 Advanced SIMD kernels.
//!
//! Four lanes per register, two accumulators. Two rather than one because a fused
//! multiply-add has several cycles of latency and a single accumulator serialises on it — the
//! second chain keeps the pipeline fed. Four accumulators help on some cores and cost register
//! pressure on others; two is the safe point without per-core tuning nobody will maintain.

use core::arch::aarch64::{
    float32x4_t, vaddq_f32, vaddvq_f32, vdupq_n_f32, vfmaq_f32, vld1q_f32, vsubq_f32,
};

/// Inner product.
///
/// # Safety
/// NEON is part of the base aarch64 architecture, so the intrinsics are always available. The
/// loop reads only `a.len().min(b.len())` elements from each slice.
#[target_feature(enable = "neon")]
pub(crate) unsafe fn dot_neon(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: the preconditions in this function's doc comment hold for
    // every call; the per-statement reasoning is in the comments below.
    unsafe {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = vdupq_n_f32(0.0);
        let mut acc1 = vdupq_n_f32(0.0);

        let mut i = 0;
        while i + 8 <= n {
            // SAFETY: `i + 8 <= n <= a.len()` and likewise for `b`, so all eight lanes of each load
            // are inside both slices.
            acc0 = vfmaq_f32(acc0, vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            acc1 = vfmaq_f32(acc1, vld1q_f32(pa.add(i + 4)), vld1q_f32(pb.add(i + 4)));
            i += 8;
        }
        if i + 4 <= n {
            // SAFETY: `i + 4 <= n`, so all four lanes are in bounds.
            acc0 = vfmaq_f32(acc0, vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            i += 4;
        }

        let mut sum = horizontal(vaddq_f32(acc0, acc1));
        while i < n {
            // SAFETY: `i < n`, so both reads are in bounds.
            sum += *pa.add(i) * *pb.add(i);
            i += 1;
        }
        sum
    }
}

/// Squared Euclidean distance.
///
/// # Safety
/// As [`dot_neon`].
#[target_feature(enable = "neon")]
pub(crate) unsafe fn l2_squared_neon(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: the preconditions in this function's doc comment hold for
    // every call; the per-statement reasoning is in the comments below.
    unsafe {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc0 = vdupq_n_f32(0.0);
        let mut acc1 = vdupq_n_f32(0.0);

        let mut i = 0;
        while i + 8 <= n {
            // SAFETY: bounded by `i + 8 <= n`, as above.
            let d0 = vsubq_f32(vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            acc0 = vfmaq_f32(acc0, d0, d0);
            let d1 = vsubq_f32(vld1q_f32(pa.add(i + 4)), vld1q_f32(pb.add(i + 4)));
            acc1 = vfmaq_f32(acc1, d1, d1);
            i += 8;
        }
        if i + 4 <= n {
            // SAFETY: bounded by `i + 4 <= n`.
            let d = vsubq_f32(vld1q_f32(pa.add(i)), vld1q_f32(pb.add(i)));
            acc0 = vfmaq_f32(acc0, d, d);
            i += 4;
        }

        let mut sum = horizontal(vaddq_f32(acc0, acc1));
        while i < n {
            // SAFETY: `i < n`.
            let d = *pa.add(i) - *pb.add(i);
            sum += d * d;
            i += 1;
        }
        sum
    }
}

/// Sum the four lanes.
///
/// # Safety
/// Operates on a register value; no memory is touched. Still `unsafe` because the intrinsic
/// carries a target-feature requirement, which NEON satisfies unconditionally on aarch64.
#[inline]
#[target_feature(enable = "neon")]
unsafe fn horizontal(v: float32x4_t) -> f32 {
    // `target_feature` already makes this call safe within the function body; no block needed.
    vaddvq_f32(v)
}
