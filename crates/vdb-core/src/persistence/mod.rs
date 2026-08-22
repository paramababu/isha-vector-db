//! Durability: the log, recovery, and the policy that decides when bytes must be safe.

pub mod layout;
pub mod manifest;
pub mod recovery;
pub mod segment;
pub mod wal;

pub use manifest::ManifestStore;
pub use recovery::{replay_into, ReplayReport};
pub use segment::{flush_memtable, FlushResult, SegmentData};
pub use wal::WalWriter;

/// How aggressively writes are made durable.
///
/// The distinction that drives the default: a **process crash** loses nothing in any of these
/// modes, because the bytes are already in the operating system's page cache and will reach the
/// disk regardless. Only **power loss or a kernel panic** can lose an unsynced write.
///
/// On a phone, process death is routine — the OS kills applications constantly — and power loss
/// is rare. Paying an fsync per write to defend against the rare case, on flash storage where
/// fsync is expensive and battery matters, is the wrong trade for almost every application.
/// Hence [`Durability::Batch`] as the default rather than [`Durability::Full`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Durability {
    /// Sync after every operation. Loses nothing, including to power loss. Slow on mobile flash.
    Full,
    /// Sync on batch commit, on explicit flush, and on close.
    ///
    /// The default. Loses only writes made since the last sync, and only to power loss.
    #[default]
    Batch,
    /// Sync only on explicit flush and on close. For bulk import, where the caller can simply
    /// redo the import.
    Relaxed,
}

impl Durability {
    /// Whether a single, non-batched write should sync immediately.
    pub fn syncs_every_write(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether committing a batch should sync.
    pub fn syncs_on_commit(self) -> bool {
        matches!(self, Self::Full | Self::Batch)
    }
}
