//! Manifest commit and slot selection, driven against a real storage backend.
//!
//! These live here rather than beside the code because `vdb-storage-memory` is a dev-dependency
//! cycle back onto `vdb-core`: the lib's own unit-test build would see two copies of the crate
//! and the `Storage` impls would not match. Integration tests link one copy, so they work.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use vdb_core::persistence::{layout, ManifestStore};
use vdb_core::storage::Storage as _;
use vdb_format::{CollectionEntry, Slot, SlotStatus};
use vdb_storage_memory::MemoryStorage;

const UUID: [u8; 16] = [9u8; 16];

#[test]
fn a_database_that_has_never_committed_reports_nothing() {
    let mem = MemoryStorage::new();
    assert!(ManifestStore::load(&mem).unwrap().is_none());
}

#[test]
fn the_first_manifest_goes_to_slot_a() {
    let mem = MemoryStorage::new();
    let store = ManifestStore::create(&mem, UUID, 1000).unwrap();
    assert_eq!(store.slot(), Slot::A);
    assert_eq!(store.current().sequence, 1);
    assert!(mem.exists(&layout::manifest(Slot::A).unwrap()).unwrap());
    assert!(!mem.exists(&layout::manifest(Slot::B).unwrap()).unwrap());
}

/// The load-bearing property: a commit never overwrites the slot it would fall back to.
#[test]
fn commits_alternate_between_the_slots() {
    let mem = MemoryStorage::new();
    let mut store = ManifestStore::create(&mem, UUID, 1000).unwrap();
    let mut expected = Slot::A;
    for i in 0..6 {
        let next = store.current().clone();
        store.commit(&mem, next, 2000 + i).unwrap();
        expected = expected.other();
        assert_eq!(store.slot(), expected, "commit {i} wrote to the wrong slot");
        assert_eq!(store.current().sequence, i + 2);
    }
}

#[test]
fn reload_adopts_the_higher_sequence() {
    let mem = MemoryStorage::new();
    let mut store = ManifestStore::create(&mem, UUID, 1000).unwrap();
    for i in 0..5 {
        let next = store.current().clone();
        store.commit(&mem, next, 2000 + i).unwrap();
    }
    let reloaded = ManifestStore::load(&mem).unwrap().unwrap();
    assert_eq!(reloaded.current().sequence, store.current().sequence);
    assert_eq!(reloaded.slot(), store.slot());
    assert_eq!(reloaded.current().db_uuid, UUID);
}

#[test]
fn commit_preserves_identity_and_creation_time_and_advances_the_clock() {
    let mem = MemoryStorage::new();
    let mut store = ManifestStore::create(&mem, UUID, 1000).unwrap();
    let mut next = store.current().clone();
    // A caller that gets these wrong must not be able to rewrite the database's identity.
    next.db_uuid = [0u8; 16];
    next.created_at_ms = 0;
    store.commit(&mem, next, 5000).unwrap();

    assert_eq!(store.current().db_uuid, UUID);
    assert_eq!(store.current().created_at_ms, 1000);
    assert_eq!(store.current().updated_at_ms, 5000);
}

/// The crash-during-commit case, end to end.
#[test]
fn a_damaged_newer_slot_falls_back_to_the_intact_older_one() {
    let mem = MemoryStorage::new();
    let mut store = ManifestStore::create(&mem, UUID, 1000).unwrap();
    let next = store.current().clone();
    store.commit(&mem, next, 2000).unwrap();
    assert_eq!(store.slot(), Slot::B);
    assert_eq!(store.current().sequence, 2);

    // Damage the newer slot, as a torn write would.
    let path = layout::manifest(Slot::B).unwrap();
    let mut bytes = mem.read_all(&path).unwrap();
    bytes.truncate(bytes.len() / 2);
    mem.write_all(&path, bytes);

    let reloaded = ManifestStore::load(&mem).unwrap().unwrap();
    assert_eq!(reloaded.current().sequence, 1, "should fall back to slot A");
    assert_eq!(reloaded.slot(), Slot::A);
}

#[test]
fn two_unreadable_slots_report_what_was_wrong_with_each() {
    let mem = MemoryStorage::new();
    mem.write_all(
        &layout::manifest(Slot::A).unwrap(),
        b"not a manifest".to_vec(),
    );
    mem.write_all(&layout::manifest(Slot::B).unwrap(), vec![]);

    match ManifestStore::load(&mem) {
        Err(e) => {
            let text = e.to_string();
            assert!(e.is_corruption(), "{e}");
            assert!(text.contains("slot A"), "{text}");
            assert!(text.contains("slot B"), "{text}");
        }
        Ok(other) => panic!("expected NoValidManifest, got {other:?}"),
    }
}

#[test]
fn one_missing_slot_is_not_an_error_if_the_other_is_readable() {
    let mem = MemoryStorage::new();
    ManifestStore::create(&mem, UUID, 1000).unwrap();
    let scan = ManifestStore::scan(&mem).unwrap();
    assert!(matches!(scan.a, SlotStatus::Valid(_)));
    assert_eq!(scan.b, SlotStatus::Missing);
    assert!(ManifestStore::load(&mem).unwrap().is_some());
}

/// A slot previously holding a longer manifest must not leave stale bytes behind.
#[test]
fn a_shrinking_manifest_does_not_leave_a_stale_tail() {
    let mem = MemoryStorage::new();
    let mut store = ManifestStore::create(&mem, UUID, 1000).unwrap();

    let mut big = store.current().clone();
    big.collections = (0..20)
        .map(|i| CollectionEntry {
            name: format!("collection-with-a-long-name-{i:03}"),
            segments: vec![],
            index_snapshot: None,
            last_applied_wal: 0,
            live_count: 0,
            total_rows: 0,
        })
        .collect();
    store.commit(&mem, big, 2000).unwrap();
    let long_len = mem
        .read_all(&layout::manifest(store.slot()).unwrap())
        .unwrap()
        .len();

    let mut small = store.current().clone();
    small.collections.clear();
    store.commit(&mem, small.clone(), 3000).unwrap();
    // Two more commits so the shrunken manifest lands in the slot that held the big one.
    store.commit(&mem, small.clone(), 4000).unwrap();
    let short_len = mem
        .read_all(&layout::manifest(store.slot()).unwrap())
        .unwrap()
        .len();

    assert!(
        short_len < long_len,
        "the file should have shrunk: {short_len} vs {long_len}"
    );
    assert!(ManifestStore::load(&mem).unwrap().is_some());
}
