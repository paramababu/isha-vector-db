//! Construction and search knobs.

/// How a graph is built.
///
/// The defaults are the ones the HNSW paper and every mature implementation converge on. They
/// are not tuned for this codebase specifically, and the benchmark in `benches/` is what would
/// justify changing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct HnswParams {
    /// Neighbours kept per node above layer 0. Layer 0 keeps `2 * m`.
    ///
    /// Higher means better recall and a larger graph. The memory cost is roughly
    /// `rows * m * 3 * 4` bytes.
    pub m: usize,

    /// Candidate list size while building.
    ///
    /// The single most important quality knob: it costs build time once and improves recall for
    /// every query afterwards.
    pub ef_construction: usize,

    /// Default candidate list size while searching, when a query does not say.
    pub ef_search: usize,

    /// Seed for level assignment.
    ///
    /// Levels come from hashing this with the row's identity rather than from a running random
    /// number generator, so the graph is a pure function of the rows and the parameters. Two
    /// builds of the same data produce the same graph, which is what makes a recall figure
    /// reproducible and a bug re-triggerable.
    pub seed: u64,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 200,
            ef_search: 64,
            seed: 0x5EED_1234_ABCD_0001,
        }
    }
}

impl HnswParams {
    /// Neighbours per node at layer 0.
    ///
    /// Layer 0 holds every node and carries the final, most precise hop, so it is given twice
    /// the degree of the sparser layers above it.
    pub const fn m0(&self) -> usize {
        self.m * 2
    }

    /// Set the neighbour count.
    pub const fn with_m(mut self, m: usize) -> Self {
        self.m = m;
        self
    }

    /// Set the build-time candidate list size.
    pub const fn with_ef_construction(mut self, ef: usize) -> Self {
        self.ef_construction = ef;
        self
    }

    /// Set the default search-time candidate list size.
    pub const fn with_ef_search(mut self, ef: usize) -> Self {
        self.ef_search = ef;
        self
    }

    /// Set the level-assignment seed.
    pub const fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Whether these parameters are usable.
    pub(crate) fn is_valid(&self) -> bool {
        self.m >= 2 && self.ef_construction >= 1 && self.ef_search >= 1
    }
}
