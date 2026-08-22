//! A hierarchical navigable small world index.
//!
//! # What this is for
//!
//! The exact scan in `vdb-core` compares the query against every live vector. That is the right
//! answer up to a point — it is cache-friendly, it is trivially correct, and with the SIMD
//! kernels in `vdb-index-flat` it handles tens of thousands of vectors in single-digit
//! milliseconds. Past a few hundred thousand it stops being viable, and no amount of kernel work
//! fixes an algorithm that is linear in the corpus.
//!
//! This index is the alternative: a navigable graph, searched in roughly logarithmic time, whose
//! results are **approximate**. [`HnswIndex::is_exact`] returns `false` and the engine reports
//! that to callers, because an approximate result silently presented as exact is a correctness
//! bug wearing a performance costume.
//!
//! # Determinism
//!
//! The graph is a pure function of the rows and the parameters. Levels come from hashing each
//! row's identity with the seed rather than from a running random number generator, candidate
//! ordering breaks ties on node index, and rows are inserted in the source's own stable order.
//! Two builds of the same data produce byte-identical graphs, so a recall figure is reproducible
//! and a bad graph can be recreated for debugging.
//!
//! # Filters
//!
//! A filtered search traverses through rows the filter rejects but only *returns* ones it
//! accepts. Skipping rejected rows entirely would disconnect the graph and collapse recall, and
//! the effect is worst exactly when a filter is most selective. See `docs/api/filters.md` for
//! what that costs and when the engine is better off scanning.

#![forbid(unsafe_code)]

mod build;
mod graph;
mod params;
mod score;
mod search;
mod snapshot;

pub use params::HnswParams;

use std::sync::RwLock;

use vdb_core::error::Result;
use vdb_core::index::{
    IndexKind, IndexSnapshots, IndexStats, SearchCtx, VectorIndex, VectorSource,
};
use vdb_core::search::{Metric, TopK};

use graph::Graph;

/// An approximate nearest-neighbour index over a navigable graph.
#[derive(Debug)]
pub struct HnswIndex {
    params: HnswParams,
    /// Built lazily by [`VectorIndex::prepare`] and shared by every concurrent search.
    ///
    /// A `RwLock` rather than a `Mutex`: searches are the common case and are read-only, so they
    /// proceed in parallel; only a rebuild takes the write side.
    graph: RwLock<Graph>,
}

impl Default for HnswIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl HnswIndex {
    /// An empty index with default parameters.
    pub fn new() -> Self {
        Self::with_params(HnswParams::default())
    }

    /// An empty index with the given parameters.
    ///
    /// Parameters that cannot produce a usable graph — fewer than two neighbours, an empty
    /// candidate list — are replaced by the defaults rather than accepted and later divided by.
    pub fn with_params(params: HnswParams) -> Self {
        let params = if params.is_valid() {
            params
        } else {
            HnswParams::default()
        };
        Self {
            params,
            graph: RwLock::new(Graph::default()),
        }
    }

    /// The parameters in force.
    pub fn params(&self) -> HnswParams {
        self.params
    }

    /// How many rows the built graph covers. Zero before the first search.
    pub fn rows(&self) -> usize {
        self.read().map_or(0, |g| g.len())
    }

    /// Read the graph, recovering from a poisoned lock.
    ///
    /// A panic in a search would otherwise leave the index permanently unusable. The graph is
    /// only ever replaced wholesale, never mutated in place during a search, so a poisoned lock
    /// cannot be guarding a half-written structure.
    fn read(&self) -> Option<std::sync::RwLockReadGuard<'_, Graph>> {
        Some(self.graph.read().unwrap_or_else(|e| e.into_inner()))
    }
}

impl VectorIndex for HnswIndex {
    fn kind(&self) -> IndexKind {
        IndexKind::Hnsw
    }

    fn is_exact(&self) -> bool {
        false
    }

    fn prepare(
        &self,
        source: &dyn VectorSource,
        metric: Metric,
        snapshots: &dyn IndexSnapshots,
    ) -> Result<()> {
        let dimension = source.dimension() as usize;
        let rows = source.len();

        // The overwhelmingly common case: nothing has changed since the last search. Taking only
        // the read lock here keeps concurrent searches from serialising on a check that almost
        // always says "no work to do".
        if let Some(g) = self.read() {
            if g.is_valid_for(rows, dimension, metric) {
                return Ok(());
            }
        }

        // Read the rows once. Both paths need them: restoring has to confirm the snapshot still
        // describes this data, and building needs them anyway.
        let decoded = build::decode_rows(source)?;

        let restored = match snapshots.load()? {
            Some(bytes) => snapshot::decode_header(&bytes, &self.params)
                .filter(|h| {
                    h.metric == metric
                        && h.dimension == dimension
                        && h.nodes == rows
                        && h.params_match
                })
                .and_then(|header| snapshot::decode(&bytes, &header, &decoded)),
            None => None,
        };

        let (graph, is_new) = match restored {
            Some(g) => (g, false),
            None => (
                build::build_from(decoded, dimension, metric, &self.params),
                true,
            ),
        };

        // Written before the lock is taken, so a slow write does not block concurrent searches
        // on a graph that is already correct in memory.
        if is_new {
            // A snapshot that cannot be written is not a reason to fail a search: the index is
            // built and usable, and the only cost is rebuilding it next time.
            let _ = snapshots.store(&snapshot::encode(&graph, &self.params));
        }

        let mut guard = self.graph.write().unwrap_or_else(|e| e.into_inner());
        // Another thread may have built it while this one was working. Both graphs are identical
        // — construction is deterministic — so either is correct; this simply avoids discarding
        // the newer one.
        if !guard.is_valid_for(rows, dimension, metric) {
            *guard = graph;
        }
        Ok(())
    }

    fn search(&self, ctx: &SearchCtx<'_>, out: &mut TopK) -> Result<()> {
        let guard = self.read();
        search::search(&self.params, guard.as_deref(), ctx, out)
    }

    fn stats(&self) -> IndexStats {
        let (rows, memory) = self.read().map_or((0, 0), |g| (g.len(), g.memory_bytes()));
        IndexStats::new(IndexKind::Hnsw, rows, memory)
    }
}
