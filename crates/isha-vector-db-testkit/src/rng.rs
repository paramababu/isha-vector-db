//! A tiny deterministic RNG.
//!
//! Every generated test input in this project comes from a seeded generator, so a failure is
//! reproducible from the seed printed in the failure message. Pulling in `rand` for this would
//! add a dependency whose output is explicitly *not* guaranteed stable across versions — which
//! would make golden tests flaky on a dependency bump.
//!
//! xorshift64*, which is not cryptographic and is not trying to be.

/// A seeded, reproducible pseudo-random source.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// Seed the generator. A zero seed is remapped, since xorshift is degenerate at zero.
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// Next raw 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n`. Returns 0 when `n` is 0.
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }

    /// Uniform in `[0, 1)`.
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u32 << 24) as f32
    }

    /// Roughly standard-normal, via the central limit theorem over six uniforms. Good enough to
    /// generate plausible embedding-shaped data; not good enough for statistics.
    pub fn next_gaussian(&mut self) -> f32 {
        let sum: f32 = (0..6).map(|_| self.next_f32()).sum();
        (sum - 3.0) * core::f32::consts::FRAC_1_SQRT_2
    }

    /// A buffer of `len` pseudo-random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| (self.next_u64() >> 24) as u8).collect()
    }

    /// A vector of `dim` gaussian components.
    pub fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_gaussian()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_gives_the_same_sequence() {
        let a: Vec<u64> = (0..64)
            .scan(Rng::new(42), |r, _| Some(r.next_u64()))
            .collect();
        let b: Vec<u64> = (0..64)
            .scan(Rng::new(42), |r, _| Some(r.next_u64()))
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_diverge() {
        let a: Vec<u64> = (0..8)
            .scan(Rng::new(1), |r, _| Some(r.next_u64()))
            .collect();
        let b: Vec<u64> = (0..8)
            .scan(Rng::new(2), |r, _| Some(r.next_u64()))
            .collect();
        assert_ne!(a, b);
    }

    #[test]
    fn zero_seed_is_not_degenerate() {
        let mut r = Rng::new(0);
        let first = r.next_u64();
        assert_ne!(first, 0);
        assert_ne!(r.next_u64(), first);
    }

    #[test]
    fn floats_stay_in_range() {
        let mut r = Rng::new(7);
        for _ in 0..10_000 {
            let f = r.next_f32();
            assert!((0.0..1.0).contains(&f), "{f}");
        }
    }

    #[test]
    fn below_respects_its_bound() {
        let mut r = Rng::new(9);
        for n in [1usize, 2, 7, 100] {
            for _ in 0..1000 {
                assert!(r.below(n) < n);
            }
        }
        assert_eq!(r.below(0), 0);
    }
}
