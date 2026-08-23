//! What a backend can actually do.

/// Capabilities a [`Storage`](super::Storage) implementation declares about itself.
///
/// The engine reads these to choose a commit protocol. A backend that claims a capability it
/// does not really have will pass its own tests and lose data in the field, which is why
/// `isha-vector-db-testkit`'s conformance suite verifies each declaration behaviourally rather than
/// taking the struct at its word.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct StorageCapabilities {
    /// `rename` is observed by any reader as all-or-nothing.
    ///
    /// When false, the engine uses only the dual-slot manifest protocol, which needs nothing
    /// but durable positional writes.
    pub atomic_rename: bool,

    /// `map_readonly` can return a mapping.
    pub mmap: bool,

    /// `sync_data` is a real durability point against power loss.
    ///
    /// False for OPFS, where `flush()` is best-effort. The engine still uses the WAL — it
    /// protects against process death, which is the realistic failure mode there — but reports
    /// the weaker guarantee in `DatabaseStats` instead of overstating it.
    pub durable_sync: bool,

    /// Writing past the end of a file leaves an efficient hole rather than allocating zeroes.
    pub sparse_files: bool,

    /// Advisory locking works. False on filesystems where locks are unreliable, such as some
    /// network mounts.
    pub file_locking: bool,

    /// Largest file the backend can hold, if it has a limit.
    pub max_file_size: Option<u64>,

    /// The backend would rather have a few large files than many small ones.
    ///
    /// True for OPFS and IndexedDB, where per-file overhead dominates. The engine packs a
    /// segment's four sections into one file when this is set.
    pub prefers_few_large_files: bool,
}

impl StorageCapabilities {
    /// A conservative baseline: durable positional writes and nothing else.
    ///
    /// Backends start here and enable what they genuinely support, so a forgotten field
    /// under-promises rather than over-promises.
    pub const fn minimal() -> Self {
        Self {
            atomic_rename: false,
            mmap: false,
            durable_sync: false,
            sparse_files: false,
            file_locking: false,
            max_file_size: None,
            prefers_few_large_files: false,
        }
    }

    /// Enable atomic `rename`.
    pub const fn with_atomic_rename(mut self, yes: bool) -> Self {
        self.atomic_rename = yes;
        self
    }

    /// Enable memory mapping.
    pub const fn with_mmap(mut self, yes: bool) -> Self {
        self.mmap = yes;
        self
    }

    /// Declare that `sync_data` is a real durability point.
    pub const fn with_durable_sync(mut self, yes: bool) -> Self {
        self.durable_sync = yes;
        self
    }

    /// Declare sparse-file support.
    pub const fn with_sparse_files(mut self, yes: bool) -> Self {
        self.sparse_files = yes;
        self
    }

    /// Declare working advisory locks.
    pub const fn with_file_locking(mut self, yes: bool) -> Self {
        self.file_locking = yes;
        self
    }

    /// Declare a maximum file size.
    pub const fn with_max_file_size(mut self, max: Option<u64>) -> Self {
        self.max_file_size = max;
        self
    }

    /// Declare a preference for few large files.
    pub const fn with_prefers_few_large_files(mut self, yes: bool) -> Self {
        self.prefers_few_large_files = yes;
        self
    }

    /// What a POSIX filesystem provides.
    pub const fn posix() -> Self {
        Self {
            atomic_rename: true,
            mmap: true,
            durable_sync: true,
            sparse_files: true,
            file_locking: true,
            max_file_size: None,
            prefers_few_large_files: false,
        }
    }
}

impl Default for StorageCapabilities {
    fn default() -> Self {
        Self::minimal()
    }
}
