//! The graph itself: layers of neighbour lists over a dense node numbering.

// Indices into the graph's own arrays are used directly rather than through `get`.
//
// This crate is not in the same risk class as `vdb-format`, which parses bytes an attacker may
// have written and must never panic on them. Every index here is a node number this crate
// allocated, into a vector this crate sized, and the invariant — every per-node array has
// exactly one entry per node, on every layer — is asserted in `Graph::push_node` under
// `debug_assert`. Node numbers that arrive from outside, such as a neighbour read during
// traversal, are still bounds-checked at the point they enter.
//
// The alternative is a fallible lookup in the innermost loop of a search that runs tens of
// millions of times per build, written to handle a case that would be a bug in this file.
#![allow(clippy::indexing_slicing)]

use vdb_core::document::RowId;
use vdb_core::search::Metric;

use crate::params::HnswParams;

/// A node's index within the graph. Dense, unlike [`RowId`], which is sparse by construction.
pub(crate) type Node = u32;

/// The built structure.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    /// Row each node stands for, in build order.
    pub(crate) rows: Vec<RowId>,
    /// Each node's vector, copied in. See the note on `vectors` below.
    pub(crate) vectors: Vec<f32>,
    /// Each node's inverse norm, so cosine does not recompute it per comparison.
    pub(crate) inv_norms: Vec<f32>,
    /// Highest layer each node appears in.
    pub(crate) levels: Vec<u8>,
    /// `layers[l][node]` is the neighbour list of `node` at layer `l`. A node absent from a
    /// layer has an empty list there.
    pub(crate) layers: Vec<Vec<Vec<Node>>>,
    /// Where a search starts: the node with the highest level.
    pub(crate) entry: Option<Node>,
    /// Dimension every vector has.
    pub(crate) dimension: usize,
    /// The metric this graph was built for. A graph is only valid for one.
    pub(crate) metric: Option<Metric>,
}

impl Graph {
    /// Nodes held.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// One node's vector.
    ///
    /// Vectors are copied into the graph rather than read back through the source on every
    /// comparison. A graph search is a pointer chase — it touches a few hundred scattered
    /// vectors per query rather than streaming all of them — and going through the source for
    /// each one would mean a segment lookup and a bounds check per distance. The cost is one
    /// extra copy of the vectors in memory, which is the trade every HNSW implementation makes.
    pub(crate) fn vector(&self, node: Node) -> &[f32] {
        let start = node as usize * self.dimension;
        // The slice is in range by construction: `push_node` extends `vectors` by exactly
        // `dimension` floats for every node, and nodes are only ever appended.
        &self.vectors[start..start + self.dimension]
    }

    /// Neighbours of `node` at `layer`, or an empty slice if it is not present there.
    pub(crate) fn neighbours(&self, layer: usize, node: Node) -> &[Node] {
        self.layers
            .get(layer)
            .and_then(|l| l.get(node as usize))
            .map_or(&[][..], Vec::as_slice)
    }

    /// Add a node, returning its index.
    pub(crate) fn push_node(
        &mut self,
        row: RowId,
        vector: &[f32],
        inv_norm: f32,
        level: u8,
    ) -> Node {
        let node = self.rows.len() as Node;
        self.rows.push(row);
        self.vectors.extend_from_slice(vector);
        self.inv_norms.push(inv_norm);
        self.levels.push(level);
        // Every layer must hold an entry for every node, so that `layers[l][node]` is always in
        // range. A layer created now starts life needing entries for all the nodes that already
        // exist — without them its indices are shifted, and because both `neighbours` and
        // `connect` go through `get`/`get_mut`, the mistake does not panic: it silently drops
        // every edge on that layer and quietly costs about half the recall.
        while self.layers.len() <= level as usize {
            let mut layer = Vec::with_capacity(node as usize + 1);
            layer.resize_with(node as usize, Vec::new);
            self.layers.push(layer);
        }
        for layer in &mut self.layers {
            layer.push(Vec::new());
        }
        debug_assert!(
            self.layers.iter().all(|l| l.len() == self.rows.len()),
            "every layer must have one neighbour list per node"
        );
        node
    }

    /// Roughly how much memory the graph occupies, excluding the copied vectors.
    pub(crate) fn memory_bytes(&self) -> usize {
        let links: usize = self
            .layers
            .iter()
            .map(|l| l.iter().map(|n| n.len() * 4).sum::<usize>())
            .sum();
        links + self.rows.len() * (8 + 1 + 4)
    }

    /// Whether this graph covers the first `self.len()` of `rows`, in the same order.
    ///
    /// When it does, the rest can simply be appended instead of rebuilding everything. This is
    /// the common shape of a write to an append-only store: existing segments keep their rows at
    /// their existing positions and a new segment adds more on the end.
    ///
    /// Deletes do not break it — a tombstoned row stays in the graph and is filtered at search
    /// time by the live set. Compaction does break it, because it renumbers rows, and that is
    /// correct: after a compaction the graph genuinely describes something that no longer exists.
    pub(crate) fn is_prefix_of(
        &self,
        rows: &[(RowId, Vec<f32>, f32)],
        dimension: usize,
        metric: Metric,
    ) -> bool {
        if self.metric != Some(metric) || self.dimension != dimension {
            return false;
        }
        if self.len() > rows.len() {
            return false;
        }
        // Compared by row, not by count. Two collections can hold the same number of rows and
        // not be the same rows at all.
        self.rows
            .iter()
            .zip(rows.iter())
            .all(|(have, (want, _, _))| have == want)
    }

    /// Whether this graph can answer a query over `rows` rows of `dimension` under `metric`.
    ///
    /// Deliberately strict. A graph built for cosine ranks differently from one built for L2,
    /// and a graph missing rows would silently never return them — both are wrong answers rather
    /// than slow ones, so any mismatch forces a rebuild.
    pub(crate) fn is_valid_for(&self, rows: usize, dimension: usize, metric: Metric) -> bool {
        self.metric == Some(metric) && self.dimension == dimension && self.len() == rows
    }
}

/// The layer a node belongs to.
///
/// The paper draws this from an exponential distribution, normally with a running random number
/// generator. Here it is a hash of the node's row and the parameter seed, which gives the same
/// distribution while making the level a pure function of the data: rebuild in a different order,
/// or on a different machine, and every node lands in the same layer. That is what lets a recall
/// number be reproduced and a bad graph be re-created for debugging.
pub(crate) fn level_for(row: RowId, params: &HnswParams) -> u8 {
    // SplitMix64: a small, well-distributed finaliser. The graph's quality depends on these
    // being uniform, not on them being cryptographic.
    let mut z = row
        .as_u64()
        .wrapping_add(params.seed)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;

    // Map to [0,1) and take -ln(u) * mL, the paper's level distribution.
    let u = ((z >> 11) as f64) / ((1u64 << 53) as f64);
    let u = if u <= f64::MIN_POSITIVE {
        f64::MIN_POSITIVE
    } else {
        u
    };
    let m_l = 1.0 / (params.m as f64).ln();
    let level = (-u.ln() * m_l).floor();

    // Capped so a freak draw cannot create a tower of empty layers, each of which costs a
    // vector push per node for the life of the graph.
    level.clamp(0.0, MAX_LEVEL as f64) as u8
}

/// Highest layer a node may occupy.
pub(crate) const MAX_LEVEL: u8 = 16;
