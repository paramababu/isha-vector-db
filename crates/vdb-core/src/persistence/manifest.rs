//! Reading and committing the database root.
//!
//! The protocol, and why it is two slots rather than write-temp-and-rename, is in
//! `docs/architecture/05-storage-and-persistence.md` §5.4. This module is the part that drives
//! it against a [`Storage`].
//!
//! Committing is: encode, write to the slot that is **not** currently authoritative, sync. A
//! crash at any instant leaves at least one intact slot — either the new write completed and
//! wins on sequence, or it did not and fails its checksum, so the old one wins. Writing to the
//! inactive slot is the load-bearing detail: overwriting the live slot would mean a torn write
//! destroys the very state we would otherwise fall back to.

use vdb_format::{Manifest, Slot, SlotScan};

use crate::error::{CorruptionError, Result};
use crate::path::DbPath;
use crate::persistence::layout;
use crate::storage::{OpenMode, Storage};

/// Holds the authoritative manifest and knows where the next commit goes.
#[derive(Debug, Clone)]
pub struct ManifestStore {
    current: Manifest,
    /// The slot `current` was read from, or written to.
    slot: Slot,
}

impl ManifestStore {
    /// Read both slots and adopt the authoritative one.
    ///
    /// Returns `Ok(None)` when neither slot exists, which is a database that has never
    /// committed — not a failure.
    ///
    /// # Errors
    /// [`CorruptionError::NoValidManifest`], describing *both* slots, when at least one file is
    /// present but none is readable. Reporting both matters: "the manifest is corrupt" without
    /// saying what was wrong with each slot leaves a user with no way to judge whether their
    /// data is recoverable.
    pub fn load(storage: &dyn Storage) -> Result<Option<Self>> {
        let a = read_slot(storage, Slot::A)?;
        let b = read_slot(storage, Slot::B)?;
        if a.is_none() && b.is_none() {
            return Ok(None);
        }
        let scan = Manifest::scan_slots(a.as_deref(), b.as_deref());
        match scan.chosen {
            Some((slot, current)) => Ok(Some(Self { current, slot })),
            None => Err(no_valid_manifest(&scan)),
        }
    }

    /// Examine both slots without adopting one, for `verify` and the `inspect` command.
    ///
    /// # Errors
    /// Any storage error.
    pub fn scan(storage: &dyn Storage) -> Result<SlotScan> {
        let a = read_slot(storage, Slot::A)?;
        let b = read_slot(storage, Slot::B)?;
        Ok(Manifest::scan_slots(a.as_deref(), b.as_deref()))
    }

    /// Create the first manifest for a new database.
    ///
    /// # Errors
    /// Any storage or format error.
    pub fn create(storage: &dyn Storage, db_uuid: [u8; 16], now_ms: u64) -> Result<Self> {
        let manifest = Manifest::new(db_uuid, now_ms);
        let mut store = Self {
            current: manifest,
            slot: Slot::B,
        };
        // Written to A, because `commit` targets the slot opposite the recorded one.
        store.write(storage, store.current.clone())?;
        Ok(store)
    }

    /// The authoritative manifest.
    pub fn current(&self) -> &Manifest {
        &self.current
    }

    /// Which slot it came from.
    pub fn slot(&self) -> Slot {
        self.slot
    }

    /// Commit a new manifest, bumping the sequence and writing to the other slot.
    ///
    /// # Errors
    /// Any storage or format error. On failure the in-memory state is left unchanged, so a
    /// caller that retries commits the same thing rather than skipping a sequence number.
    pub fn commit(&mut self, storage: &dyn Storage, mut next: Manifest, now_ms: u64) -> Result<()> {
        next.sequence = self.current.sequence.saturating_add(1);
        next.db_uuid = self.current.db_uuid;
        next.created_at_ms = self.current.created_at_ms;
        next.updated_at_ms = now_ms;
        self.write(storage, next)
    }

    fn write(&mut self, storage: &dyn Storage, next: Manifest) -> Result<()> {
        let target = self.slot.other();
        let bytes = next.encode().map_err(|e| {
            crate::error::from_format_at(
                e,
                &layout::manifest(target).unwrap_or_else(|_| DbPath::root()),
            )
        })?;
        let path = layout::manifest(target)?;

        let mut file = storage.open_file(&path, OpenMode::Create)?;
        // Truncate first: a slot previously holding a longer manifest would otherwise leave a
        // tail of stale bytes after the new one. The header bounds the read, so those bytes are
        // harmless — but they would confuse anyone reading the file with a hex editor, and
        // `verify` would have to learn to expect them.
        file.truncate(0)?;
        file.write_at(&bytes, 0)?;
        file.sync_data()?;

        self.current = next;
        self.slot = target;
        Ok(())
    }
}

fn read_slot(storage: &dyn Storage, slot: Slot) -> Result<Option<Vec<u8>>> {
    let path = layout::manifest(slot)?;
    let Some(meta) = storage.metadata(&path)? else {
        return Ok(None);
    };
    // A zero-length slot is *absent*, not corrupt. A slot is written as truncate-then-write, so
    // a crash between the two leaves an empty file — and on a real filesystem, creating a file
    // and dying before writing to it is an ordinary outcome. Reporting that as corruption made
    // a database that had not finished its very first commit permanently unopenable.
    //
    // Safe because a commit never targets the authoritative slot: the slot we would fall back
    // to is never the one being truncated. And `Database::open` separately refuses to create a
    // fresh database over a directory that already holds collections, so an empty slot can
    // never be the reason existing data is silently orphaned.
    if meta.len == 0 {
        return Ok(None);
    }
    let file = storage.open_file(&path, OpenMode::Read)?;
    let mut bytes = vec![0u8; meta.len as usize];
    let read = file.read_at(&mut bytes, 0)?;
    bytes.truncate(read);
    Ok(Some(bytes))
}

fn no_valid_manifest(scan: &SlotScan) -> crate::DbError {
    CorruptionError::NoValidManifest {
        path: DbPath::root(),
        slot_a: scan.a.describe(),
        slot_b: scan.b.describe(),
    }
    .into()
}
