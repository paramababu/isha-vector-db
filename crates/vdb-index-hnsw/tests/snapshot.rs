//! Persisting the graph.
//!
//! The point of a snapshot is that reopening a database does not cost 95 seconds. The risk is
//! that it silently answers from a graph that no longer describes the data, which would be worse
//! than the slow rebuild it replaces — so most of what follows is about the ways a snapshot can
//! be wrong, and confirming each of them is detected rather than trusted.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::print_stdout
)]

use std::sync::Mutex;

use vdb_core::document::RowId;
use vdb_core::error::Result;
use vdb_core::index::{
    AllLive, Budget, IndexSnapshots, RowVisitor, SearchCtx, SearchParams, VectorIndex, VectorSource,
};
use vdb_core::search::{inv_norm, Metric, TopK};
use vdb_index_hnsw::{HnswIndex, HnswParams};
use vdb_testkit::Rng;

/// A snapshot slot in memory, which also counts what happened to it.
#[derive(Debug, Default)]
struct Slot {
    bytes: Mutex<Option<Vec<u8>>>,
    stores: Mutex<usize>,
    loads: Mutex<usize>,
}

impl Slot {
    fn stored(&self) -> usize {
        *self.stores.lock().unwrap()
    }
    fn loaded(&self) -> usize {
        *self.loads.lock().unwrap()
    }
    fn size(&self) -> usize {
        self.bytes.lock().unwrap().as_ref().map_or(0, Vec::len)
    }
    fn corrupt(&self, f: impl Fn(&mut Vec<u8>)) {
        let mut guard = self.bytes.lock().unwrap();
        if let Some(bytes) = guard.as_mut() {
            f(bytes);
        }
    }
}

impl IndexSnapshots for Slot {
    fn load(&self) -> Result<Option<Vec<u8>>> {
        *self.loads.lock().unwrap() += 1;
        Ok(self.bytes.lock().unwrap().clone())
    }
    fn store(&self, bytes: &[u8]) -> Result<()> {
        *self.stores.lock().unwrap() += 1;
        *self.bytes.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}

#[derive(Debug)]
struct Rows {
    dimension: u32,
    rows: Vec<(RowId, Vec<u8>, f32)>,
}

impl Rows {
    fn random(n: usize, dimension: usize, seed: u64) -> Self {
        let mut rng = Rng::new(seed);
        let rows = (0..n)
            .map(|i| {
                let v: Vec<f32> = (0..dimension).map(|_| rng.next_f32() - 0.5).collect();
                let bytes: Vec<u8> = v.iter().flat_map(|x| x.to_le_bytes()).collect();
                (RowId::new(0, i as u32), bytes, inv_norm(&v))
            })
            .collect();
        Self {
            dimension: dimension as u32,
            rows,
        }
    }
}

impl VectorSource for Rows {
    fn dimension(&self) -> u32 {
        self.dimension
    }
    fn len(&self) -> usize {
        self.rows.len()
    }
    fn for_each(&self, visit: &mut RowVisitor<'_>) -> Result<()> {
        for (row, bytes, norm) in &self.rows {
            visit(*row, bytes, *norm)?;
        }
        Ok(())
    }
    fn vector(&self, row: RowId) -> Option<(&[u8], f32)> {
        self.rows
            .iter()
            .find(|(r, _, _)| *r == row)
            .map(|(_, b, n)| (b.as_slice(), *n))
    }
}

fn results(index: &HnswIndex, source: &Rows, slot: &Slot, query: &[f32], k: usize) -> Vec<u32> {
    index.prepare(source, Metric::Cosine, slot).unwrap();
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
    (0..dimension).map(|i| (i as f32 * 0.07).sin()).collect()
}

#[test]
fn a_restored_graph_gives_the_same_answers() {
    let source = Rows::random(1500, 32, 0xF00D);
    let slot = Slot::default();
    let q = query(32);

    let first = results(&HnswIndex::new(), &source, &slot, &q, 20);
    assert_eq!(slot.stored(), 1, "the first build should save a snapshot");
    assert!(slot.size() > 0);

    // A completely fresh index, with only the snapshot to go on.
    let second_index = HnswIndex::new();
    let second = results(&second_index, &source, &slot, &q, 20);
    assert_eq!(
        first, second,
        "the restored graph answered differently from the one that was built"
    );
    assert_eq!(
        slot.stored(),
        1,
        "restoring must not rewrite the snapshot it just read"
    );
    assert_eq!(second_index.rows(), 1500);
}

/// The whole point: restoring must be enormously cheaper than building.
#[test]
fn restoring_is_much_faster_than_building() {
    let source = Rows::random(4000, 64, 0xBEEF);
    let slot = Slot::default();
    let q = query(64);

    let built = std::time::Instant::now();
    let _ = results(&HnswIndex::new(), &source, &slot, &q, 10);
    let build_time = built.elapsed();

    let restored = std::time::Instant::now();
    let _ = results(&HnswIndex::new(), &source, &slot, &q, 10);
    let restore_time = restored.elapsed();

    println!("build {build_time:?}, restore {restore_time:?}");
    assert!(
        restore_time * 5 < build_time,
        "restoring took {restore_time:?} against a build of {build_time:?}; \
         it is supposed to be the cheap path"
    );
}

/// A snapshot that no longer describes the data must be rejected, not answered from.
#[test]
fn a_snapshot_for_different_data_is_rejected() {
    let slot = Slot::default();
    let q = query(32);

    let first = Rows::random(500, 32, 1);
    let _ = results(&HnswIndex::new(), &first, &slot, &q, 10);
    assert_eq!(slot.stored(), 1);

    // Same shape, different vectors and different row ids: the saved graph describes rows that
    // are not these.
    let second = Rows::random(500, 32, 2);
    let index = HnswIndex::new();
    let got = results(&index, &second, &slot, &q, 10);
    assert_eq!(index.rows(), 500);
    assert_eq!(got.len(), 10);

    // The rows happen to have the same ids here, so what protects us is that the index rebuilt
    // and the answers are right for the new data.
    let reference = results(&HnswIndex::new(), &second, &Slot::default(), &q, 10);
    assert_eq!(got, reference, "answers came from a graph for the old data");
}

#[test]
fn a_snapshot_with_the_wrong_row_count_is_rejected() {
    let slot = Slot::default();
    let q = query(16);
    let small = Rows::random(200, 16, 5);
    let _ = results(&HnswIndex::new(), &small, &slot, &q, 5);

    let larger = Rows::random(300, 16, 5);
    let index = HnswIndex::new();
    let got = results(&index, &larger, &slot, &q, 5);
    assert_eq!(index.rows(), 300, "the stale snapshot was used");
    assert_eq!(got.len(), 5);
}

#[test]
fn a_snapshot_for_another_metric_is_rejected() {
    let source = Rows::random(300, 16, 7);
    let slot = Slot::default();
    let index = HnswIndex::new();
    let q = query(16);

    index.prepare(&source, Metric::Cosine, &slot).unwrap();
    assert_eq!(slot.stored(), 1);

    // L2 ranks by a different function, so the cosine graph is the wrong shape for it.
    index.prepare(&source, Metric::L2, &slot).unwrap();
    assert_eq!(slot.stored(), 2, "the graph was not rebuilt for L2");
    let _ = q;
}

#[test]
fn a_snapshot_built_with_other_parameters_is_rejected() {
    let source = Rows::random(400, 16, 11);
    let slot = Slot::default();
    let q = query(16);

    let _ = results(&HnswIndex::new(), &source, &slot, &q, 5);
    assert_eq!(slot.stored(), 1);

    let different = HnswIndex::with_params(HnswParams::default().with_m(8));
    let _ = results(&different, &source, &slot, &q, 5);
    assert_eq!(
        slot.stored(),
        2,
        "a graph built with different parameters is not the graph this index asked for"
    );
}

/// Damage must produce a rebuild, never a panic and never a wrong answer.
#[test]
fn damaged_snapshots_are_discarded_rather_than_trusted() {
    let source = Rows::random(600, 16, 13);
    let q = query(16);
    let reference = results(&HnswIndex::new(), &source, &Slot::default(), &q, 10);

    /// A named way of damaging a snapshot.
    type Mutation = (&'static str, Box<dyn Fn(&mut Vec<u8>)>);

    let mutations: Vec<Mutation> = vec![
        (
            "truncated to nothing",
            Box::new(|b: &mut Vec<u8>| b.clear()),
        ),
        (
            "truncated by half",
            Box::new(|b: &mut Vec<u8>| {
                let half = b.len() / 2;
                b.truncate(half);
            }),
        ),
        (
            "truncated by one byte",
            Box::new(|b: &mut Vec<u8>| {
                b.pop();
            }),
        ),
        ("wrong magic", Box::new(|b: &mut Vec<u8>| b[0] ^= 0xFF)),
        ("wrong version", Box::new(|b: &mut Vec<u8>| b[4] ^= 0xFF)),
        (
            "absurd node count",
            Box::new(|b: &mut Vec<u8>| {
                b[28..32].fill(0xFF);
            }),
        ),
        (
            "garbage in the middle",
            Box::new(|b: &mut Vec<u8>| {
                let mid = b.len() / 2;
                let end = (mid + 64).min(b.len());
                for byte in &mut b[mid..end] {
                    *byte ^= 0xA5;
                }
            }),
        ),
        (
            "garbage at the end",
            Box::new(|b: &mut Vec<u8>| {
                let n = b.len();
                for byte in &mut b[n.saturating_sub(32)..] {
                    *byte ^= 0x5A;
                }
            }),
        ),
    ];

    for (name, mutate) in mutations {
        let slot = Slot::default();
        let _ = results(&HnswIndex::new(), &source, &slot, &q, 10);
        slot.corrupt(&*mutate);

        let index = HnswIndex::new();
        let got = results(&index, &source, &slot, &q, 10);
        assert_eq!(index.rows(), 600, "{name}: the graph is unusable");
        assert_eq!(
            got, reference,
            "{name}: a damaged snapshot changed the answers"
        );
    }
}

/// Every single-byte flip must be survivable. Damage is not always convenient.
#[test]
fn no_single_byte_flip_can_produce_a_wrong_answer() {
    // Small on purpose. Every byte is flipped and the graph rebuilt for each, so the cost is
    // the snapshot's length times the build time; a corpus large enough to be interesting here
    // would put this test into the minutes and it would stop being run.
    let source = Rows::random(40, 4, 17);
    let q = query(4);
    let reference = results(&HnswIndex::new(), &source, &Slot::default(), &q, 5);

    let seed = Slot::default();
    let _ = results(&HnswIndex::new(), &source, &seed, &q, 5);
    let original = seed.bytes.lock().unwrap().clone().unwrap();

    // Every byte, one bit each: enough to cover every field boundary without taking minutes.
    for i in 0..original.len() {
        let mut bytes = original.clone();
        bytes[i] ^= 0x01;
        let slot = Slot::default();
        slot.store(&bytes).unwrap();

        let index = HnswIndex::new();
        let got = results(&index, &source, &slot, &q, 5);
        assert_eq!(
            got, reference,
            "flipping a bit at offset {i} changed the answers"
        );
    }
}

#[test]
fn an_empty_collection_round_trips() {
    let source = Rows::random(0, 8, 1);
    let slot = Slot::default();
    assert!(results(&HnswIndex::new(), &source, &slot, &query(8), 5).is_empty());
    assert!(results(&HnswIndex::new(), &source, &slot, &query(8), 5).is_empty());
}

#[test]
fn a_missing_snapshot_is_not_an_error() {
    let source = Rows::random(200, 16, 19);
    let slot = Slot::default();
    let got = results(&HnswIndex::new(), &source, &slot, &query(16), 5);
    assert_eq!(got.len(), 5);
    assert_eq!(slot.loaded(), 1, "the slot should have been consulted");
    assert_eq!(slot.stored(), 1);
}
