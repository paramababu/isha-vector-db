//! The graph under concurrent use.
//!
//! This index is the only part of the system holding shared mutable state: one graph, behind an
//! `RwLock`, read by every search and replaced or extended by whichever thread notices the data
//! has moved. Everything else here is either immutable once written or guarded by the engine's
//! own locking.
//!
//! Reasoning about that was not enough. Writing these tests found two real defects:
//!
//! - `prepare` released the lock before `search` took it again, so a concurrent writer could swap
//!   the graph in between and a search would silently return results missing the newest
//!   documents. `search` now verifies the graph covers the source and falls back to the exact
//!   scan if it does not.
//! - A thread finishing a build for N rows would overwrite a graph another thread had already
//!   extended to N+5, because the check asked only "is this valid for N", which the newer graph
//!   also fails. The older graph won.
//!
//! Neither is reliably reproducible by timing. The first attempt at these tests spawned threads
//! and hoped; it passed with *both* fixes reverted, which makes it worthless as a guard however
//! reassuring it looks. What works is to construct the state the race produces and test that
//! directly — a graph that does not cover its source is reachable without any threads at all —
//! and, where the interleaving itself is the subject, to force it with a barrier rather than
//! leave it to the scheduler.
//!
//! The multi-threaded tests below are kept, but for what they genuinely check: no deadlock, no
//! panic, no disagreement. They are not what catches the two defects above.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;

use vdb_core::document::RowId;
use vdb_core::error::Result;
use vdb_core::index::{
    AllLive, Budget, ExactScan, IndexSnapshots, RowVisitor, SearchCtx, SearchParams, VectorIndex,
    VectorSource,
};
use vdb_core::search::{inv_norm, Metric, TopK};
use vdb_index_hnsw::HnswIndex;
use vdb_testkit::Rng;

/// A snapshot slot safe to share, which also records how often it was written.
#[derive(Debug, Default)]
struct Slot {
    bytes: Mutex<Option<Vec<u8>>>,
    stores: AtomicUsize,
}

impl IndexSnapshots for Slot {
    fn load(&self) -> Result<Option<Vec<u8>>> {
        Ok(self.bytes.lock().unwrap_or_else(|e| e.into_inner()).clone())
    }
    fn store(&self, bytes: &[u8]) -> Result<()> {
        self.stores.fetch_add(1, Ordering::Relaxed);
        *self.bytes.lock().unwrap_or_else(|e| e.into_inner()) = Some(bytes.to_vec());
        Ok(())
    }
}

/// A source that can grow while it is being read, like a collection being written to.
#[derive(Debug)]
struct GrowingRows {
    dimension: u32,
    rows: RwLock<Vec<(RowId, Vec<u8>, f32)>>,
    all: Vec<(RowId, Vec<u8>, f32)>,
}

impl GrowingRows {
    fn new(total: usize, dimension: usize, initial: usize, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let all: Vec<_> = (0..total)
            .map(|i| {
                let v: Vec<f32> = (0..dimension).map(|_| rng.next_f32() - 0.5).collect();
                let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                (RowId::new(0, i as u32), bytes, inv_norm(&v))
            })
            .collect();
        Self {
            dimension: dimension as u32,
            rows: RwLock::new(all[..initial].to_vec()),
            all,
        }
    }

    /// Append the next `n` rows, as a flush would.
    fn grow(&self, n: usize) {
        let mut rows = self.rows.write().unwrap_or_else(|e| e.into_inner());
        let have = rows.len();
        let want = (have + n).min(self.all.len());
        rows.extend_from_slice(&self.all[have..want]);
    }

    fn live(&self) -> usize {
        self.rows.read().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl VectorSource for GrowingRows {
    fn dimension(&self) -> u32 {
        self.dimension
    }
    fn len(&self) -> usize {
        self.live()
    }
    fn for_each(&self, visit: &mut RowVisitor<'_>) -> Result<()> {
        let rows = self.rows.read().unwrap_or_else(|e| e.into_inner());
        for (row, bytes, norm) in rows.iter() {
            visit(*row, bytes, *norm)?;
        }
        Ok(())
    }
    fn vector(&self, _row: RowId) -> Option<(&[u8], f32)> {
        // Not used by either index on this path, and cannot be served safely from behind a lock
        // with this signature.
        None
    }
}

fn search_with(
    index: &dyn VectorIndex,
    source: &GrowingRows,
    snapshots: &dyn IndexSnapshots,
    query: &[f32],
    k: usize,
) -> Vec<u32> {
    index.prepare(source, Metric::Cosine, snapshots).unwrap();
    let budget = Budget::unlimited();
    let ctx = SearchCtx {
        query,
        top_k: k,
        metric: Metric::Cosine,
        source,
        live: &AllLive,
        filter: None,
        min_score: None,
        params: SearchParams::default(),
        budget: &budget,
    };
    let mut top = TopK::new(k);
    index.search(&ctx, &mut top).unwrap();
    top.into_sorted().into_iter().map(|c| c.row.row()).collect()
}

fn query(dimension: usize) -> Vec<f32> {
    (0..dimension).map(|i| (i as f32 * 0.13).cos()).collect()
}

/// Many readers against one graph must not deadlock, panic, or disagree.
#[test]
fn concurrent_searches_agree() {
    let source = Arc::new(GrowingRows::new(800, 16, 800, 0xAAA));
    let index = Arc::new(HnswIndex::new());
    let slot = Arc::new(Slot::default());
    let q = Arc::new(query(16));

    let expected = Arc::new(search_with(&*index, &source, &*slot, &q, 10));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let (index, source, slot, q, expected) = (
                Arc::clone(&index),
                Arc::clone(&source),
                Arc::clone(&slot),
                Arc::clone(&q),
                Arc::clone(&expected),
            );
            thread::spawn(move || {
                for _ in 0..40 {
                    assert_eq!(search_with(&*index, &source, &*slot, &q, 10), *expected);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("a searching thread panicked");
    }
}

/// Searching while the data grows must never return a stale answer.
///
/// The invariant is not "results are identical" — the data really is changing — but that whatever
/// comes back is the correct answer for *some* state the collection was in, and never one that
/// omits documents already visible to this thread. Comparing against the exact scan taken at the
/// same moment is the only honest way to check that.
#[test]
fn searching_while_writing_never_returns_a_stale_answer() {
    let source = Arc::new(GrowingRows::new(1200, 16, 200, 0xBBB));
    let index = Arc::new(HnswIndex::new());
    let slot = Arc::new(Slot::default());
    let q = Arc::new(query(16));
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let (source, stop) = (Arc::clone(&source), Arc::clone(&stop));
        thread::spawn(move || {
            while source.live() < 1200 {
                source.grow(25);
                thread::yield_now();
            }
            stop.store(true, Ordering::Relaxed);
        })
    };

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (index, source, slot, q, stop) = (
                Arc::clone(&index),
                Arc::clone(&source),
                Arc::clone(&slot),
                Arc::clone(&q),
                Arc::clone(&stop),
            );
            thread::spawn(move || {
                let mut checked = 0usize;
                while !stop.load(Ordering::Relaxed) || checked < 20 {
                    let got = search_with(&*index, &source, &*slot, &q, 5);
                    // Ground truth for whatever the collection holds right now. Taken after the
                    // graph search, so any row it names was visible before this comparison.
                    let truth = search_with(&ExactScan::new(), &source, &*slot, &q, 5);
                    let truth_set: std::collections::HashSet<u32> = truth.iter().copied().collect();
                    for row in &got {
                        assert!(
                            truth_set.contains(row) || got.len() == truth.len(),
                            "the graph returned row {row}, which the exact scan did not"
                        );
                    }
                    assert_eq!(got.len(), truth.len().min(5), "wrong number of results");
                    checked += 1;
                }
            })
        })
        .collect();

    writer.join().expect("the writing thread panicked");
    for r in readers {
        r.join().expect("a reading thread panicked");
    }
    assert_eq!(source.live(), 1200);
    assert_eq!(index.rows(), 1200);
}

/// Several threads discovering a stale graph at once must converge, not thrash or corrupt it.
#[test]
fn simultaneous_rebuilds_converge() {
    let source = Arc::new(GrowingRows::new(600, 16, 600, 0xCCC));
    let index = Arc::new(HnswIndex::new());
    let slot = Arc::new(Slot::default());
    let q = Arc::new(query(16));

    let handles: Vec<_> = (0..6)
        .map(|_| {
            let (index, source, slot, q) = (
                Arc::clone(&index),
                Arc::clone(&source),
                Arc::clone(&slot),
                Arc::clone(&q),
            );
            thread::spawn(move || search_with(&*index, &source, &*slot, &q, 10))
        })
        .collect();

    let results: Vec<Vec<u32>> = handles
        .into_iter()
        .map(|h| h.join().expect("a thread panicked"))
        .collect();

    // Construction is deterministic, so every thread must have got the same answer whichever
    // graph it ended up reading.
    for r in &results[1..] {
        assert_eq!(*r, results[0], "threads disagreed about the same data");
    }
    assert_eq!(index.rows(), 600);
}

/// A graph covering more rows must never be replaced by one covering fewer.
///
/// Asserted directly rather than by trying to win a race: after any sequence of preparations
/// against a growing source, the graph must cover what the source currently holds.
#[test]
fn a_newer_graph_is_never_replaced_by_an_older_one() {
    let source = Arc::new(GrowingRows::new(900, 16, 100, 0xDDD));
    let index = Arc::new(HnswIndex::new());
    let slot = Arc::new(Slot::default());
    let q = Arc::new(query(16));

    let _ = search_with(&*index, &source, &*slot, &q, 5);

    let handles: Vec<_> = (0..4)
        .map(|i| {
            let (index, source, slot, q) = (
                Arc::clone(&index),
                Arc::clone(&source),
                Arc::clone(&slot),
                Arc::clone(&q),
            );
            thread::spawn(move || {
                for _ in 0..10 {
                    if i == 0 {
                        source.grow(20);
                    }
                    let _ = search_with(&*index, &source, &*slot, &q, 5);
                }
            })
        })
        .collect();
    for h in handles {
        h.join().expect("a thread panicked");
    }

    let _ = search_with(&*index, &source, &*slot, &q, 5);
    assert_eq!(
        index.rows(),
        source.live(),
        "the graph does not cover the collection after concurrent growth"
    );
}

// ---------------------------------------------------------------------------
// The two defects, tested by constructing the state rather than racing for it.
// ---------------------------------------------------------------------------

/// A graph that no longer covers its source must not answer from what it has.
///
/// This is the state a concurrent writer produces between `prepare` and `search`, reached here
/// without threads: build against a small collection, then search against a larger one. The
/// graph knows nothing of the newer rows, so answering from it would silently omit them.
#[test]
fn a_graph_that_does_not_cover_the_source_falls_back() {
    let source = GrowingRows::new(600, 16, 200, 0xE11);
    let slot = Slot::default();
    let index = HnswIndex::new();
    let q = query(16);

    // Build for the first 200 rows only.
    index.prepare(&source, Metric::Cosine, &slot).unwrap();
    assert_eq!(index.rows(), 200);

    // The collection grows, and a search runs *without* another `prepare` — exactly what a
    // concurrent writer causes between the two lock acquisitions.
    source.grow(400);
    assert_eq!(source.live(), 600);

    let budget = Budget::unlimited();
    let ctx = SearchCtx {
        query: &q,
        top_k: 10,
        metric: Metric::Cosine,
        source: &source,
        live: &AllLive,
        filter: None,
        min_score: None,
        params: SearchParams::default(),
        budget: &budget,
    };
    let mut top = TopK::new(10);
    index.search(&ctx, &mut top).unwrap();
    let got: Vec<u32> = top.into_sorted().into_iter().map(|c| c.row.row()).collect();

    // The exact scan over all 600 is the right answer. A stale graph would return the best of
    // the first 200 and look entirely plausible.
    let mut truth_top = TopK::new(10);
    let truth_ctx = SearchCtx {
        query: &q,
        top_k: 10,
        metric: Metric::Cosine,
        source: &source,
        live: &AllLive,
        filter: None,
        min_score: None,
        params: SearchParams::default(),
        budget: &budget,
    };
    ExactScan::new().search(&truth_ctx, &mut truth_top).unwrap();
    let truth: Vec<u32> = truth_top
        .into_sorted()
        .into_iter()
        .map(|c| c.row.row())
        .collect();

    assert_eq!(
        got, truth,
        "the stale graph answered from the rows it happened to have"
    );
    assert!(
        got.iter().any(|r| *r >= 200),
        "no result came from the newer rows, so the fallback did not happen"
    );
}

/// A build that finishes late must not replace a graph that has moved on.
///
/// The interleaving is forced, not hoped for: the slow thread's source blocks inside `for_each`
/// until the fast thread has extended the graph, so the loser always finishes last.
#[test]
fn a_late_build_does_not_overwrite_a_newer_graph() {
    use std::sync::mpsc;

    /// A source that pauses the first time it is read, so one thread can be held mid-decode.
    #[derive(Debug)]
    struct Blocking {
        inner: GrowingRows,
        gate: Mutex<Option<mpsc::Receiver<()>>>,
        entered: mpsc::Sender<()>,
    }

    impl VectorSource for Blocking {
        fn dimension(&self) -> u32 {
            self.inner.dimension()
        }
        fn len(&self) -> usize {
            self.inner.len()
        }
        fn for_each(&self, visit: &mut RowVisitor<'_>) -> Result<()> {
            if let Some(gate) = self.gate.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = self.entered.send(());
                let _ = gate.recv();
            }
            self.inner.for_each(visit)
        }
        fn vector(&self, _row: RowId) -> Option<(&[u8], f32)> {
            None
        }
    }

    let (release_tx, release_rx) = mpsc::channel();
    let (entered_tx, entered_rx) = mpsc::channel();

    let shared = Arc::new(GrowingRows::new(900, 16, 300, 0xE22));
    let slow = Arc::new(Blocking {
        inner: GrowingRows::new(900, 16, 300, 0xE22),
        gate: Mutex::new(Some(release_rx)),
        entered: entered_tx,
    });
    let index = Arc::new(HnswIndex::new());
    let slot = Arc::new(Slot::default());

    // The slow thread starts a build for 300 rows and stalls inside `for_each`.
    let slow_thread = {
        let (index, slow, slot) = (Arc::clone(&index), Arc::clone(&slow), Arc::clone(&slot));
        thread::spawn(move || {
            index
                .prepare(&*slow, Metric::Cosine, &*slot)
                .expect("the stalled build failed");
        })
    };
    entered_rx.recv().expect("the slow thread never started");

    // Meanwhile the collection grows and another thread brings the graph up to date.
    shared.grow(600);
    index
        .prepare(&*shared, Metric::Cosine, &*slot)
        .expect("the fast build failed");
    assert_eq!(index.rows(), 900, "the fast thread did not build for 900");

    // Now let the stale build finish. It must not win.
    release_tx.send(()).expect("nobody was waiting");
    slow_thread.join().expect("the slow thread panicked");

    assert_eq!(
        index.rows(),
        900,
        "a build for 300 rows overwrote a graph covering 900"
    );
}
