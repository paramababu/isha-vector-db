//! The index abstraction.
//!
//! [`VectorIndex`] makes exactly three assumptions, all of which hold for a flat scan, IVF,
//! HNSW, PQ and DiskANN alike:
//!
//! 1. An index can produce approximate-or-exact nearest neighbours for a query.
//! 2. It can be built from a stream and snapshotted to opaque blocks.
//! 3. It respects a live set and an optional row predicate at query time.
//!
//! It does *not* assume the index fits in memory, that inserts are cheap, that deletes are
//! supported natively, or that results are exact — [`VectorIndex::is_exact`] is surfaced in the
//! search statistics so a caller can tell whether it got ground truth.
//!
//! The test of whether this abstraction is real is whether adding HNSW later touches only the
//! new crate, one registry entry and one parameter variant. That is checked by the conformance
//! suite in `vdb-testkit`, which any implementation must pass.

use core::fmt::Debug;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::document::RowId;
use crate::error::{DbError, Result};
use crate::search::{Metric, TopK};

/// Which algorithm an index implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexKind {
    /// Exact brute-force scan.
    Flat,
}

impl IndexKind {
    /// A stable lowercase name, used in file names and statistics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Flat => "flat",
        }
    }
}

/// Per-query knobs. Indexes ignore what does not apply to them.
///
/// One struct rather than a per-index type so a caller can pass the same request to any index
/// and get the best that index can do, instead of failing to compile when the index changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct SearchParams {
    /// Candidate list size for graph indexes. Ignored by a flat scan.
    pub ef_search: Option<usize>,
    /// Partitions to probe for inverted-file indexes. Ignored by a flat scan.
    pub n_probe: Option<usize>,
}

impl SearchParams {
    /// Set the candidate list size for graph indexes.
    #[must_use]
    pub const fn with_ef_search(mut self, ef: usize) -> Self {
        self.ef_search = Some(ef);
        self
    }

    /// Set the number of partitions to probe for inverted-file indexes.
    #[must_use]
    pub const fn with_n_probe(mut self, probes: usize) -> Self {
        self.n_probe = Some(probes);
        self
    }
}

/// Whether a row is still alive.
///
/// Passed in rather than held inside an index, so a delete is `O(1)` everywhere and only
/// compaction pays to reclaim the space. It also means an approximate index that cannot remove
/// a node — HNSW, for one — still returns correct results.
pub trait LiveSet: Send + Sync {
    /// Whether this row is live.
    fn is_live(&self, row: RowId) -> bool;
}

/// Everything is live. Used when a caller has already filtered the rows it offers.
#[derive(Debug, Clone, Copy)]
pub struct AllLive;

impl LiveSet for AllLive {
    fn is_live(&self, _row: RowId) -> bool {
        true
    }
}

/// An arbitrary per-row test, such as a compiled metadata filter.
pub trait RowPredicate: Send + Sync {
    /// Whether this row should be considered.
    fn matches(&self, row: RowId) -> bool;
}

impl<F: Fn(RowId) -> bool + Send + Sync> RowPredicate for F {
    fn matches(&self, row: RowId) -> bool {
        self(row)
    }
}

/// Cooperative cancellation and a scan ceiling.
///
/// The core spawns no threads, so a long search cannot be interrupted from outside — there is
/// nothing to interrupt. Instead the index checks this periodically. That is what lets a mobile
/// SDK abandon a search when the user navigates away, without the engine owning a runtime.
#[derive(Debug, Default)]
pub struct Budget {
    cancelled: AtomicBool,
    scanned: AtomicU64,
    max_scanned: Option<u64>,
}

impl Budget {
    /// An unlimited budget.
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// A budget that gives up after examining `max` candidates.
    pub fn with_max_scanned(max: u64) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            scanned: AtomicU64::new(0),
            max_scanned: Some(max),
        }
    }

    /// Ask the current operation to stop. Safe to call from another thread.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Candidates examined so far.
    pub fn scanned(&self) -> u64 {
        self.scanned.load(Ordering::Relaxed)
    }

    /// Record progress and check whether to continue.
    ///
    /// # Errors
    /// [`DbError::Cancelled`] if cancellation was requested or the ceiling was reached.
    pub fn charge(&self, candidates: u64) -> Result<()> {
        let total = self.scanned.fetch_add(candidates, Ordering::Relaxed) + candidates;
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(DbError::Cancelled);
        }
        if let Some(max) = self.max_scanned {
            if total > max {
                return Err(DbError::Cancelled);
            }
        }
        Ok(())
    }
}

/// Visitor handed each row during a scan: its id, its raw bytes, and its cached inverse norm.
pub type RowVisitor<'v> = dyn FnMut(RowId, &[u8], f32) -> Result<()> + 'v;

/// Rows an index can score, and how to fetch one.
///
/// Vectors are handed over as raw bytes with their cached reciprocal norm. Bytes rather than
/// `&[f32]` because that is how they sit in a segment file, and reinterpreting them without a
/// copy needs a pointer cast this crate forbids — the index crate does it, under audit.
pub trait VectorSource: Send + Sync {
    /// The dimension every row has.
    fn dimension(&self) -> u32;

    /// How many rows are available.
    fn len(&self) -> usize;

    /// Whether there are no rows.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Visit every row in a stable order.
    ///
    /// # Errors
    /// Whatever the visitor returns, or a storage error.
    fn for_each(&self, visit: &mut RowVisitor<'_>) -> Result<()>;

    /// Fetch one row, for indexes that traverse rather than scan.
    fn vector(&self, row: RowId) -> Option<(&[u8], f32)>;
}

/// Everything an index needs to answer one query.
pub struct SearchCtx<'a> {
    /// The query, already decoded to floats.
    pub query: &'a [f32],
    /// How many results to return.
    pub top_k: usize,
    /// The metric to score with.
    pub metric: Metric,
    /// Rows to consider.
    pub source: &'a dyn VectorSource,
    /// Which of them are still alive.
    pub live: &'a dyn LiveSet,
    /// An optional additional test, such as a metadata filter.
    pub filter: Option<&'a dyn RowPredicate>,
    /// Lowest score to keep, inclusive.
    pub min_score: Option<f32>,
    /// Per-index knobs.
    pub params: SearchParams,
    /// Cancellation and scan ceiling.
    pub budget: &'a Budget,
}

impl Debug for SearchCtx<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SearchCtx")
            .field("dimension", &self.query.len())
            .field("top_k", &self.top_k)
            .field("metric", &self.metric)
            .field("rows", &self.source.len())
            .field("filtered", &self.filter.is_some())
            .finish()
    }
}

impl SearchCtx<'_> {
    /// Whether a row should be scored at all.
    pub fn admits(&self, row: RowId) -> bool {
        // `map_or` rather than `is_none_or`, which is newer than our MSRV.
        self.live.is_live(row) && self.filter.map_or(true, |f| f.matches(row))
    }
}

/// What an index reports about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct IndexStats {
    /// Which algorithm.
    pub kind: IndexKind,
    /// Rows the index covers.
    pub rows: usize,
    /// Approximate memory the index itself occupies, excluding the vectors.
    pub memory_bytes: usize,
}

impl IndexStats {
    /// Report an index's counters.
    ///
    /// A constructor rather than a struct literal because this type is `#[non_exhaustive]`, so
    /// adding a field later is not a breaking change — which also means code outside this crate
    /// cannot write a literal for it. Third-party indexes must be able to build one, so the
    /// constructor is part of the contract.
    pub const fn new(kind: IndexKind, rows: usize, memory_bytes: usize) -> Self {
        Self {
            kind,
            rows,
            memory_bytes,
        }
    }
}

/// A nearest-neighbour index.
pub trait VectorIndex: Debug + Send + Sync {
    /// Which algorithm this is.
    fn kind(&self) -> IndexKind;

    /// Whether results are ground truth.
    ///
    /// Surfaced to callers, so an approximate result is never mistaken for an exact one.
    fn is_exact(&self) -> bool;

    /// Find the best `ctx.top_k` rows, offering them to `out`.
    ///
    /// # Errors
    /// [`DbError::Cancelled`] if the budget was exhausted, or any storage error.
    fn search(&self, ctx: &SearchCtx<'_>, out: &mut TopK) -> Result<()>;

    /// Counters.
    fn stats(&self) -> IndexStats;
}

/// The always-available exact scan.
///
/// Every row is scored. That is the correctness baseline the whole project is measured against,
/// so it lives in `vdb-core` alongside the trait it implements, for the same reason
/// `vdb-storage-memory` is the reference storage backend: the thing everything else is compared
/// to should be the simplest possible implementation, in the crate that forbids `unsafe`, with
/// nothing clever in it to be wrong.
///
/// `vdb-index-flat` is where the SIMD-accelerated version of this scan lives, because that needs
/// the pointer casts this crate does not permit. It delegates here today and is differential-
/// tested against this implementation, which is what will keep the fast path honest once it
/// diverges.
///
/// It holds no state and stores no vectors: it scans whatever [`VectorSource`] the query
/// supplies. Index memory is therefore `O(1)` in the row count, there is nothing to keep in
/// sync with the data, and a collection is searchable the instant it opens — including straight
/// after a crash.
#[derive(Debug, Default, Clone, Copy)]
pub struct ExactScan;

impl ExactScan {
    /// Create one. It holds no state.
    pub const fn new() -> Self {
        Self
    }
}

/// Candidates examined between budget checks.
///
/// Small enough that a cancelled search stops promptly, large enough to keep the atomic out of
/// the inner loop.
const BUDGET_STRIDE: u64 = 1024;

impl VectorIndex for ExactScan {
    fn kind(&self) -> IndexKind {
        IndexKind::Flat
    }

    fn is_exact(&self) -> bool {
        true
    }

    fn search(&self, ctx: &SearchCtx<'_>, out: &mut TopK) -> Result<()> {
        if ctx.top_k == 0 {
            return Ok(());
        }
        let scorer = crate::search::Scorer::new(ctx.metric, ctx.query);
        let mut since_check = 0u64;
        ctx.source.for_each(&mut |row, bytes, row_inv_norm| {
            // Dead and filtered-out rows are skipped before scoring, not after: the dot product
            // is the expensive part and there is no reason to pay it for a row that cannot be
            // returned.
            if !ctx.admits(row) {
                return Ok(());
            }
            out.offer(row, scorer.score_bytes(bytes, row_inv_norm));
            since_check += 1;
            if since_check >= BUDGET_STRIDE {
                ctx.budget.charge(since_check)?;
                since_check = 0;
            }
            Ok(())
        })?;
        ctx.budget.charge(since_check)
    }

    fn stats(&self) -> IndexStats {
        IndexStats::new(IndexKind::Flat, 0, 0)
    }
}
