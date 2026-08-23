//! Vectorised distance kernels.
//!
//! The scalar reference lives in `isha_vector_db_core::search::metric` and is the definition of what these
//! must compute. Everything here is an optimisation, and every one of them is
//! differential-tested against that reference — which is the only reason it is safe to write
//! this kind of code at all.
//!
//! # Reading stored bytes as floats
//!
//! A stored vector arrives as `&[u8]` from a segment file. Reinterpreting it as `&[f32]` is done
//! with [`slice::align_to`], which is sound here for two reasons: every bit pattern is a valid
//! `f32`, so there is no invalid value to construct; and `align_to` handles alignment itself,
//! returning any unaligned prefix separately rather than producing a misaligned reference.
//!
//! In practice the prefix is always empty — allocations are at least 8-byte aligned, memory maps
//! are page-aligned, and the vector block starts at a 64-byte offset with a stride that is a
//! multiple of four — so the aligned path is the one that runs. The unaligned fallback exists
//! because "always" is not "guaranteed", and being wrong here would mean reading a neighbouring
//! row's bytes as this row's vector.
//!
//! Native-order reinterpretation is only correct on a little-endian target, which `isha-vector-db-format`
//! enforces with a `compile_error!`.
//!
//! # Which kernel runs
//!
//! Chosen once per query, not per row. On `aarch64`, NEON is part of the base architecture and
//! is always available. On `x86_64`, AVX2 and FMA are detected at runtime, so one binary runs
//! everywhere and uses what the machine has.

#[cfg(target_arch = "aarch64")]
mod neon;
#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "x86_64")]
mod x86;

/// Which implementation is in use, for reporting and for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Backend {
    /// The portable reference.
    Scalar,
    /// AVX2 with fused multiply-add.
    Avx2,
    /// AArch64 Advanced SIMD.
    Neon,
    /// WebAssembly 128-bit SIMD.
    Simd128,
}

impl Backend {
    /// A stable lowercase name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Scalar => "scalar",
            Self::Avx2 => "avx2",
            Self::Neon => "neon",
            Self::Simd128 => "simd128",
        }
    }
}

/// The best kernel this machine can run.
pub fn backend() -> Backend {
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
        {
            return Backend::Avx2;
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // NEON is mandatory on aarch64, so there is nothing to detect.
        return Backend::Neon;
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        return Backend::Simd128;
    }
    #[allow(unreachable_code)]
    Backend::Scalar
}

/// Reinterpret stored bytes as floats, or `None` if they are not aligned for it.
fn as_f32(row: &[u8]) -> Option<&[f32]> {
    // SAFETY: every bit pattern is a valid `f32`, so no invalid value can be produced, and
    // `align_to` is responsible for the alignment. We require an empty prefix so the returned
    // slice starts at the row's first byte; a non-empty prefix means this buffer is not
    // 4-byte aligned and the caller takes the scalar path instead.
    let (prefix, floats, _suffix) = unsafe { row.align_to::<f32>() };
    if prefix.is_empty() {
        Some(floats)
    } else {
        None
    }
}

/// Inner product of a query and a stored row.
pub fn dot(query: &[f32], row: &[u8]) -> f32 {
    match as_f32(row) {
        Some(values) => dot_f32(query, values),
        None => isha_vector_db_core::search::metric::dot_bytes(query, row),
    }
}

/// Squared Euclidean distance between a query and a stored row.
pub fn l2_squared(query: &[f32], row: &[u8]) -> f32 {
    match as_f32(row) {
        Some(values) => l2_squared_f32(query, values),
        None => isha_vector_db_core::search::metric::l2_squared_bytes(query, row),
    }
}

/// Inner product of two float slices, vectorised where possible.
pub fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if matches!(backend(), Backend::Avx2) {
            // SAFETY: the AVX2 and FMA features were detected immediately above, and the kernel
            // reads only `a.len().min(b.len())` elements from each slice.
            return unsafe { x86::dot_avx2(a, b) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: NEON is part of the base aarch64 architecture, so the intrinsics are always
        // available, and the kernel reads only `a.len().min(b.len())` elements from each slice.
        return unsafe { neon::dot_neon(a, b) };
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        // SAFETY: `simd128` is a compile-time feature here, and the kernel is bounded by the
        // shorter of the two slices.
        return unsafe { wasm::dot_simd128(a, b) };
    }
    #[allow(unreachable_code)]
    isha_vector_db_core::search::metric::dot(a, b)
}

/// Squared Euclidean distance between two float slices, vectorised where possible.
pub fn l2_squared_f32(a: &[f32], b: &[f32]) -> f32 {
    #[cfg(target_arch = "x86_64")]
    {
        if matches!(backend(), Backend::Avx2) {
            // SAFETY: as for `dot_f32`.
            return unsafe { x86::l2_squared_avx2(a, b) };
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        // SAFETY: as for `dot_f32`.
        return unsafe { neon::l2_squared_neon(a, b) };
    }
    #[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
    {
        // SAFETY: as for `dot_f32`.
        return unsafe { wasm::l2_squared_simd128(a, b) };
    }
    #[allow(unreachable_code)]
    isha_vector_db_core::search::metric::l2_squared(a, b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use isha_vector_db_core::search::metric;

    fn bytes(values: &[f32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Deterministic values spanning several orders of magnitude, so the comparison is not all
    /// on numbers that happen to sum cleanly.
    fn sample(n: usize, seed: u64) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let unit = (s >> 40) as f32 / (1u32 << 24) as f32;
                (unit - 0.5) * 20.0
            })
            .collect()
    }

    /// The property that makes an optimised kernel safe to ship: it agrees with the reference.
    ///
    /// The tolerance is relative, because floating-point summation in a different order gives a
    /// different — not a wrong — answer, and the difference grows with the magnitude of the
    /// running total. An absolute tolerance would either fail on large inputs or pass on
    /// genuinely broken small ones.
    fn assert_close(got: f32, want: f32, context: &str) {
        let scale = want.abs().max(1.0);
        let error = (got - want).abs() / scale;
        assert!(
            error < 1e-5,
            "{context}: got {got}, reference {want} (relative error {error:e})"
        );
    }

    #[test]
    fn kernels_agree_with_the_scalar_reference_at_every_length() {
        // Every length through two vector widths and beyond, so the tail handling is covered
        // at each possible remainder — the classic place a vectorised kernel goes wrong.
        for n in 0..40 {
            let a = sample(n, 0xA11CE);
            let b = sample(n, 0xB0B);
            assert_close(dot_f32(&a, &b), metric::dot(&a, &b), &format!("dot n={n}"));
            assert_close(
                l2_squared_f32(&a, &b),
                metric::l2_squared(&a, &b),
                &format!("l2 n={n}"),
            );
        }
    }

    #[test]
    fn kernels_agree_at_realistic_dimensions() {
        for n in [64usize, 128, 384, 768, 1536, 3072] {
            let a = sample(n, 0xC0FFEE);
            let b = sample(n, 0xDECAF);
            assert_close(dot_f32(&a, &b), metric::dot(&a, &b), &format!("dot n={n}"));
            assert_close(
                l2_squared_f32(&a, &b),
                metric::l2_squared(&a, &b),
                &format!("l2 n={n}"),
            );
        }
    }

    #[test]
    fn the_byte_entry_points_agree_with_the_reference() {
        for n in [0usize, 1, 3, 8, 17, 384] {
            let a = sample(n, 1);
            let b = sample(n, 2);
            let raw = bytes(&b);
            assert_close(
                dot(&a, &raw),
                metric::dot_bytes(&a, &raw),
                &format!("dot n={n}"),
            );
            assert_close(
                l2_squared(&a, &raw),
                metric::l2_squared_bytes(&a, &raw),
                &format!("l2 n={n}"),
            );
        }
    }

    /// The fallback that exists because "always aligned in practice" is not a guarantee. If it
    /// were wrong, an unaligned buffer would be read as if it began one to three bytes earlier —
    /// producing plausible-looking garbage rather than a crash.
    #[test]
    fn an_unaligned_buffer_falls_back_and_still_agrees() {
        let a = sample(16, 7);
        let b = sample(16, 9);
        let mut padded = vec![0u8];
        padded.extend_from_slice(&bytes(&b));
        let unaligned = padded.get(1..).unwrap();

        // Whether this buffer is actually unaligned depends on the allocator, so the test asserts
        // agreement either way rather than asserting which path ran.
        assert_close(dot(&a, unaligned), metric::dot(&a, &b), "unaligned dot");
        assert_close(
            l2_squared(&a, unaligned),
            metric::l2_squared(&a, &b),
            "unaligned l2",
        );
    }

    #[test]
    fn mismatched_lengths_are_bounded_by_the_shorter_slice() {
        let long = sample(32, 11);
        let short = sample(8, 13);
        // Dimensions are validated long before a scan, so this can only happen with a corrupt
        // segment — where computing over what exists beats reading past the end of a buffer.
        let expected = metric::dot(&long, &short);
        assert_close(dot_f32(&long, &short), expected, "long query");
        assert_close(dot_f32(&short, &long), expected, "long row");
    }

    #[test]
    fn extreme_but_finite_values_do_not_produce_infinities() {
        let a = vec![f32::MAX.sqrt() / 8.0; 16];
        let b = vec![1.0f32; 16];
        assert!(dot_f32(&a, &b).is_finite());
        assert!(l2_squared_f32(&a, &b).is_finite());

        let tiny = vec![f32::MIN_POSITIVE; 16];
        assert!(dot_f32(&tiny, &tiny).is_finite());
    }

    #[test]
    fn the_backend_is_the_expected_one_for_this_target() {
        let b = backend();
        if cfg!(target_arch = "aarch64") {
            assert_eq!(b, Backend::Neon, "NEON is mandatory on aarch64");
        }
        assert!(!b.name().is_empty());
    }
}
