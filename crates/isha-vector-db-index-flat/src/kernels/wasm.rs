//! WebAssembly 128-bit SIMD kernels.
//!
//! Four lanes, and no fused multiply-add: `simd128` has no FMA instruction, so a multiply and an
//! add are issued separately. The gain over scalar is correspondingly smaller than on native
//! targets — worth having, but not the same order.
//!
//! Built only when `simd128` is enabled at compile time, because WebAssembly has no runtime
//! feature detection — a module either contains the instructions or it does not.
//!
//! **The module `scripts/build-web.sh` produces does not enable it**, so the shipped web SDK runs
//! the scalar kernels. Turning it on is one `RUSTFLAGS` away and `simd128` is supported by every
//! browser that supports OPFS, so it is very likely worth doing; it is not done yet because
//! nothing has measured it on a browser, and this project does not claim a speed-up it has not
//! measured. An earlier version of this comment claimed the SDK shipped two modules and chose
//! between them at load time. It never did.

use core::arch::wasm32::{
    f32x4_add, f32x4_extract_lane, f32x4_mul, f32x4_splat, f32x4_sub, v128, v128_load,
};

/// Inner product.
///
/// # Safety
/// `simd128` is a compile-time feature here. The loop reads only `a.len().min(b.len())` elements
/// from each slice.
#[target_feature(enable = "simd128")]
pub(crate) unsafe fn dot_simd128(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: the preconditions in this function's doc comment hold for
    // every call; the per-statement reasoning is in the comments below.
    unsafe {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = f32x4_splat(0.0);

        let mut i = 0;
        while i + 4 <= n {
            // SAFETY: `i + 4 <= n`, so all four lanes are inside both slices.
            let x = v128_load(pa.add(i).cast::<v128>());
            let y = v128_load(pb.add(i).cast::<v128>());
            acc = f32x4_add(acc, f32x4_mul(x, y));
            i += 4;
        }

        let mut sum = horizontal(acc);
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
/// As [`dot_simd128`].
#[target_feature(enable = "simd128")]
pub(crate) unsafe fn l2_squared_simd128(a: &[f32], b: &[f32]) -> f32 {
    // SAFETY: the preconditions in this function's doc comment hold for
    // every call; the per-statement reasoning is in the comments below.
    unsafe {
        let n = a.len().min(b.len());
        let (pa, pb) = (a.as_ptr(), b.as_ptr());
        let mut acc = f32x4_splat(0.0);

        let mut i = 0;
        while i + 4 <= n {
            // SAFETY: `i + 4 <= n`.
            let x = v128_load(pa.add(i).cast::<v128>());
            let y = v128_load(pb.add(i).cast::<v128>());
            let d = f32x4_sub(x, y);
            acc = f32x4_add(acc, f32x4_mul(d, d));
            i += 4;
        }

        let mut sum = horizontal(acc);
        while i < n {
            // SAFETY: `i < n`.
            let d = *pa.add(i) - *pb.add(i);
            sum += d * d;
            i += 1;
        }
        sum
    }
}

fn horizontal(v: v128) -> f32 {
    f32x4_extract_lane::<0>(v)
        + f32x4_extract_lane::<1>(v)
        + f32x4_extract_lane::<2>(v)
        + f32x4_extract_lane::<3>(v)
}
