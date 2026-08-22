//! The crash sweep: crash at every I/O operation, reopen, assert consistency.
//!
//! From `docs/architecture/08-testing.md` §8.3. The driver runs one workload once per mutating
//! storage operation in it, injecting a failure at each index in turn, and after every one
//! asserts that reopening yields a state some prefix of the committed operations could have
//! produced.
//!
//! The bugs this finds are the ones nobody writes a test for by hand: a crash at operation 47
//! of 300, in a sequence no human would think to construct. Because it runs against in-memory
//! storage it finishes in seconds and can run on every push.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::sync::Arc;

use vdb_core::document::DocId;
use vdb_core::path::DbPath;
use vdb_core::persistence::{replay_into, wal::WalWriter, Durability};
use vdb_core::storage::Storage;
use vdb_core::vector::{VectorDType, VectorView};
use vdb_core::write::Memtable;
use vdb_format::{IdKind, WalOp};
use vdb_storage_memory::MemoryStorage;
use vdb_testkit::{Fault, FaultyStorage};

const ID_KIND: IdKind = IdKind::Str { max_len: 64 };
const DIM: u32 = 2;

fn wal_path() -> DbPath {
    DbPath::parse("wal/000001.wal").unwrap()
}

fn put(id: &str, values: [f32; 2]) -> WalOp {
    WalOp::Put {
        id: id.as_bytes().to_vec(),
        vector: VectorView::f32(&values).to_bytes(),
        metadata: vec![],
        content: None,
    }
}

/// The only live sets the workload can legally leave behind. Each operation, and the batch as a
/// whole, is atomic — so anything outside this list means a write was partially applied.
fn committed_prefixes() -> Vec<BTreeSet<String>> {
    [
        vec![],
        vec!["a"],
        vec!["a", "b"],
        vec!["b"],           // after deleting a
        vec!["b", "c", "d"], // after the atomic batch
        vec!["b", "c", "d", "e"],
    ]
    .into_iter()
    .map(|v| v.into_iter().map(str::to_owned).collect())
    .collect()
}

/// Perform the workload. Errors are expected — that is the point — so they are returned.
fn run_workload(storage: &dyn Storage, durability: Durability) -> vdb_core::Result<()> {
    storage.create_dir_all(&DbPath::parse("wal").unwrap())?;
    let mut w = WalWriter::open(storage, &wal_path(), 1, durability)?;
    w.append(put("a", [1.0, 0.0]))?;
    w.append(put("b", [0.0, 1.0]))?;
    w.append(WalOp::Delete { id: b"a".to_vec() })?;
    w.append_group(vec![put("c", [1.0, 1.0]), put("d", [2.0, 2.0])])?;
    w.append(put("e", [3.0, 3.0]))?;
    w.sync()?;
    Ok(())
}

fn recover(storage: &dyn Storage) -> vdb_core::Result<BTreeSet<String>> {
    let mut memtable = Memtable::new(DIM, VectorDType::F32);
    replay_into(storage, &wal_path(), ID_KIND, &mut memtable)?;
    Ok(memtable
        .live_rows()
        .iter()
        .map(|r| match &r.id {
            DocId::Str(s) => s.clone(),
            DocId::U64(v) => v.to_string(),
        })
        .collect())
}

fn assert_is_a_committed_prefix(live: &BTreeSet<String>, context: &str) {
    let legal = committed_prefixes();
    assert!(
        legal.contains(live),
        "{context}: recovered {live:?}, which is not any committed prefix.\n  legal: {legal:?}"
    );
}

/// How many mutating storage operations the workload performs, which sizes the sweep.
fn workload_op_count() -> u64 {
    let counting = FaultyStorage::counting(Arc::new(MemoryStorage::new()));
    run_workload(&counting, Durability::Batch).expect("the clean run must succeed");
    counting.op_count()
}

/// Run the sweep with one fault, asserting recovery at every operation index.
fn sweep(fault: Fault, power_loss: bool, label: &str) {
    let total = workload_op_count();
    assert!(
        total > 5,
        "the workload should perform a meaningful number of operations: {total}"
    );

    for index in 0..total {
        let mem = MemoryStorage::new();
        let faulty = FaultyStorage::failing_at(Arc::new(mem.clone()), index, fault);
        let _ = run_workload(&faulty, Durability::Batch);
        if power_loss {
            mem.simulate_power_loss();
        }
        let live = recover(&mem)
            .unwrap_or_else(|e| panic!("{label} at operation {index} left an unopenable log: {e}"));
        assert_is_a_committed_prefix(&live, &format!("{label} at operation {index}"));
    }
}

#[test]
fn the_clean_run_applies_everything() {
    let mem = MemoryStorage::new();
    run_workload(&mem, Durability::Batch).unwrap();
    let expected: BTreeSet<String> = ["b", "c", "d", "e"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(recover(&mem).unwrap(), expected);
}

/// An OS kill: the process dies, but bytes already handed to the kernel still reach storage.
/// This is the common failure on mobile by a wide margin.
#[test]
fn crashing_at_every_io_operation_leaves_a_recoverable_prefix() {
    sweep(Fault::Crash, false, "crash");
}

/// The harsher failure: everything not synced is lost as well.
#[test]
fn power_loss_at_every_io_operation_leaves_a_recoverable_prefix() {
    sweep(Fault::Crash, true, "power loss");
}

/// A full disk must surface as an error and leave the database usable, not corrupt it.
#[test]
fn running_out_of_space_leaves_a_recoverable_prefix() {
    sweep(Fault::NoSpace, false, "ENOSPC");
}

/// A filesystem that reports a successful sync without making anything durable. The engine
/// cannot detect this, but it must not make matters worse: the log stays replayable.
#[test]
fn a_lying_sync_still_leaves_a_replayable_log() {
    sweep(Fault::DropSync, true, "dropped sync");
}

/// A write interrupted part-way through leaves a half-written frame — the case most likely to
/// be mistaken for corruption.
#[test]
fn a_torn_write_at_any_prefix_length_leaves_a_recoverable_prefix() {
    let total = workload_op_count();
    for index in 0..total {
        for prefix in [0usize, 1, 5, 13, 40, 97] {
            let mem = MemoryStorage::new();
            let faulty = FaultyStorage::failing_at(
                Arc::new(mem.clone()),
                index,
                Fault::TornWrite { prefix },
            );
            let _ = run_workload(&faulty, Durability::Batch);
            let live = recover(&mem).unwrap_or_else(|e| {
                panic!("torn write of {prefix} bytes at operation {index}: {e}")
            });
            assert_is_a_committed_prefix(
                &live,
                &format!("torn write of {prefix} bytes at operation {index}"),
            );
        }
    }
}

/// The promise of `write_batch`, isolated: a group torn before its commit record applies
/// nothing at all, never half of itself.
#[test]
fn an_interrupted_batch_applies_none_of_itself() {
    let total = workload_op_count();
    let mut saw_an_incomplete_batch = false;

    for index in 0..total {
        for prefix in [0usize, 7, 25, 60] {
            let mem = MemoryStorage::new();
            let faulty = FaultyStorage::failing_at(
                Arc::new(mem.clone()),
                index,
                Fault::TornWrite { prefix },
            );
            let _ = run_workload(&faulty, Durability::Batch);
            let live = recover(&mem).unwrap();

            let c = live.contains("c");
            let d = live.contains("d");
            assert_eq!(
                c, d,
                "batch members disagreed at operation {index}, prefix {prefix}"
            );
            if !c {
                saw_an_incomplete_batch = true;
            }
        }
    }
    assert!(
        saw_an_incomplete_batch,
        "the sweep never crashed before the batch committed, so it proved nothing"
    );
}

/// Recovery must distinguish a torn tail from damage. A bit flipped inside a complete frame is
/// damage, and must be reported rather than silently truncated away.
#[test]
fn corruption_inside_a_complete_frame_is_reported_not_discarded() {
    let mem = MemoryStorage::new();
    run_workload(&mem, Durability::Batch).unwrap();
    let clean = mem.read_all(&wal_path()).unwrap();

    let mut damaged = clean.clone();
    damaged[20] ^= 0xFF;
    mem.write_all(&wal_path(), damaged);

    match recover(&mem) {
        Err(e) => {
            assert!(e.is_corruption(), "expected a corruption error, got {e}");
            assert!(
                e.to_string().contains("wal"),
                "the error should name the file: {e}"
            );
        }
        Ok(live) => panic!("corruption was silently accepted, recovering {live:?}"),
    }

    // Whereas simply cutting the file short is a torn tail, and recovers cleanly.
    let mut short = clean;
    short.truncate(short.len() - 5);
    mem.write_all(&wal_path(), short);
    let live = recover(&mem).expect("a torn tail must not be treated as corruption");
    assert_is_a_committed_prefix(&live, "truncated tail");
}

#[test]
fn replaying_a_missing_log_is_not_an_error() {
    let mem = MemoryStorage::new();
    assert!(recover(&mem)
        .expect("no log means nothing was written")
        .is_empty());
}

#[test]
fn replay_reports_what_it_rolled_back() {
    let mem = MemoryStorage::new();
    mem.create_dir_all(&DbPath::parse("wal").unwrap()).unwrap();
    {
        let mut w = WalWriter::open(&mem, &wal_path(), 1, Durability::Batch).unwrap();
        w.append(put("kept", [1.0, 1.0])).unwrap();
        w.sync().unwrap();
    }
    // Append an uncommitted group by hand: two operations and no commit record.
    let mut bytes = mem.read_all(&wal_path()).unwrap();
    let mut buf = vdb_format::Writer::new();
    for (seq, id) in [(10u64, "orphan-1"), (11, "orphan-2")] {
        vdb_format::WalFrame::in_txn(seq, 99, put(id, [0.0, 0.0]))
            .append_to(&mut buf)
            .unwrap();
    }
    bytes.extend_from_slice(buf.as_slice());
    mem.write_all(&wal_path(), bytes);

    let mut memtable = Memtable::new(DIM, VectorDType::F32);
    let report = replay_into(&mem, &wal_path(), ID_KIND, &mut memtable).unwrap();
    assert_eq!(report.applied, 1);
    assert_eq!(
        report.rolled_back, 2,
        "both orphaned operations should be discarded"
    );
    assert!(!report.truncated_tail);
    assert_eq!(memtable.len(), 1);
}
