//! The storage abstraction — the one place platform knowledge enters the engine.
//!
//! `vdb-core` never opens a file. It asks a [`Storage`] implementation to do it, and that
//! implementation is supplied by the embedder: `vdb-storage-os` on a desktop or phone,
//! `vdb-storage-opfs` in a browser, `vdb-storage-memory` in a test. The engine's whole knowledge
//! of the host is this trait.
//!
//! Three design choices are worth explaining, because they look unusual next to `std::fs`:
//!
//! **Positional I/O, not a seek cursor.** [`File::read_at`] takes an offset. A shared cursor
//! cannot be used safely from several reader threads at once, and positional reads map directly
//! onto `pread`, `FileHandle.read(at:)` and OPFS `read(buf, {at})`.
//!
//! **Capabilities are declared, not assumed.** A browser cannot do everything a POSIX filesystem
//! can. Rather than have every backend fake POSIX badly, [`StorageCapabilities`] states what is
//! genuinely available and the engine adapts its commit protocol. A backend must implement
//! honestly what it declares — `vdb-testkit`'s conformance suite checks exactly that.
//!
//! **Paths are [`DbPath`]s**, always relative to a root the backend owns. The engine cannot name
//! a file outside the database even if it tries.

mod caps;

pub use caps::StorageCapabilities;

use core::fmt::Debug;

use crate::error::Result;
use crate::path::DbPath;

/// How to open a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OpenMode {
    /// Open for reading. Fails if the file does not exist.
    Read,
    /// Open for reading and writing. Fails if the file does not exist.
    ReadWrite,
    /// Open for reading and writing, creating the file if it does not exist.
    Create,
    /// Create for reading and writing. Fails if the file already exists.
    CreateNew,
}

impl OpenMode {
    /// Whether this mode permits writes.
    pub fn is_writable(self) -> bool {
        !matches!(self, OpenMode::Read)
    }
}

/// Whether a directory entry is a file or a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
}

/// One entry from [`Storage::list_dir`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's name, relative to the directory that was listed.
    pub name: String,
    /// Whether it is a file or a directory.
    pub kind: EntryKind,
}

/// What [`Storage::metadata`] reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileMeta {
    /// Length in bytes. Zero for directories.
    pub len: u64,
    /// Whether the path is a file or a directory.
    pub kind: EntryKind,
}

/// A filesystem, as much of one as the engine needs.
///
/// Implementations must be safe to share across threads: readers call [`Storage::open_file`]
/// concurrently.
pub trait Storage: Debug + Send + Sync {
    /// A short name for this backend, used in error messages.
    fn name(&self) -> &'static str;

    /// Where this backend keeps its data, in whatever terms make sense for it.
    ///
    /// A filesystem backend returns a path; an in-memory one returns something like
    /// `"memory"`; a browser one might return an origin. The engine never interprets it — it
    /// only passes it through to errors, so that "no database at /Users/me/app/data" reaches
    /// the user instead of "no database at os".
    ///
    /// Defaults to [`Storage::name`], so a backend that has nothing useful to add says nothing.
    fn describe(&self) -> String {
        self.name().to_owned()
    }

    /// What this backend can actually do. Must be honest; the conformance suite verifies it.
    fn capabilities(&self) -> StorageCapabilities;

    /// Open a file.
    ///
    /// # Errors
    /// [`StorageError::NotFound`](crate::error::StorageError::NotFound) when the mode requires an
    /// existing file and there is none;
    /// [`StorageError::AlreadyExists`](crate::error::StorageError::AlreadyExists) for
    /// [`OpenMode::CreateNew`] against an existing file.
    fn open_file(&self, path: &DbPath, mode: OpenMode) -> Result<Box<dyn File>>;

    /// Delete a file. Deleting a path that does not exist is an error, so that a caller cannot
    /// mistake "already gone" for "removed".
    ///
    /// # Errors
    /// [`StorageError::NotFound`](crate::error::StorageError::NotFound) if there is no such file.
    fn remove_file(&self, path: &DbPath) -> Result<()>;

    /// Move a file, replacing any existing destination.
    ///
    /// Only meaningful when [`StorageCapabilities::atomic_rename`] is set; backends without it
    /// must return [`StorageError::Unsupported`](crate::error::StorageError::Unsupported) rather
    /// than emulate it with a non-atomic copy, because the engine's choice of commit protocol
    /// depends on knowing the difference.
    ///
    /// # Errors
    /// [`StorageError::NotFound`](crate::error::StorageError::NotFound) if the source is missing.
    fn rename(&self, from: &DbPath, to: &DbPath) -> Result<()>;

    /// Create a directory and any missing parents. Succeeds if it already exists.
    ///
    /// # Errors
    /// [`StorageError`](crate::error::StorageError) on backend failure.
    fn create_dir_all(&self, path: &DbPath) -> Result<()>;

    /// Recursively delete a directory and everything under it.
    ///
    /// # Errors
    /// [`StorageError::NotFound`](crate::error::StorageError::NotFound) if there is no such
    /// directory.
    fn remove_dir_all(&self, path: &DbPath) -> Result<()>;

    /// List a directory's immediate children, in unspecified order.
    ///
    /// # Errors
    /// [`StorageError::NotFound`](crate::error::StorageError::NotFound) if there is no such
    /// directory.
    fn list_dir(&self, path: &DbPath) -> Result<Vec<DirEntry>>;

    /// Metadata for a path, or `None` if nothing is there.
    ///
    /// Absence is `Ok(None)`, not an error: "does this exist?" is a question, not a failure.
    ///
    /// # Errors
    /// [`StorageError`](crate::error::StorageError) if the backend cannot answer.
    fn metadata(&self, path: &DbPath) -> Result<Option<FileMeta>>;

    /// Whether anything exists at `path`.
    ///
    /// # Errors
    /// As [`Storage::metadata`].
    fn exists(&self, path: &DbPath) -> Result<bool> {
        Ok(self.metadata(path)?.is_some())
    }

    /// Flush a directory's own entries to durable storage.
    ///
    /// On POSIX, a `rename` is not durable until the containing directory is synced — a detail
    /// that costs data on power loss when forgotten. Backends where this is meaningless
    /// implement it as a no-op.
    ///
    /// # Errors
    /// [`StorageError::Io`](crate::error::StorageError::Io) if the sync fails.
    fn sync_dir(&self, path: &DbPath) -> Result<()>;

    /// Take the single-writer lock, held until the returned guard is dropped.
    ///
    /// Advisory only. This prevents accidents — two instances of an app, a debug tool left open —
    /// and is not a security boundary.
    ///
    /// # Errors
    /// [`StorageError::LockUnavailable`](crate::error::StorageError::LockUnavailable) if another
    /// holder has it.
    fn try_lock(&self, path: &DbPath) -> Result<Box<dyn FileLock>>;
}

/// An open file, addressed by offset.
pub trait File: Debug + Send + Sync {
    /// Read into `buf` starting at `offset`, returning how many bytes were read.
    ///
    /// A short read means end-of-file, not an error. Callers that need exactly `buf.len()` bytes
    /// use [`File::read_exact_at`].
    ///
    /// # Errors
    /// [`StorageError::Io`](crate::error::StorageError::Io) on backend failure.
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize>;

    /// Write `buf` at `offset`, extending the file if necessary.
    ///
    /// # Errors
    /// [`StorageError`](crate::error::StorageError) on backend failure, including a read-only
    /// handle.
    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()>;

    /// Append `buf`, returning the offset it was written at.
    ///
    /// # Errors
    /// As [`File::write_at`].
    fn append(&mut self, buf: &[u8]) -> Result<u64>;

    /// Set the file's length, truncating or zero-extending.
    ///
    /// # Errors
    /// As [`File::write_at`].
    fn truncate(&mut self, len: u64) -> Result<()>;

    /// The file's current length.
    ///
    /// # Errors
    /// [`StorageError::Io`](crate::error::StorageError::Io) on backend failure.
    fn len(&self) -> Result<u64>;

    /// Whether the file is empty.
    ///
    /// # Errors
    /// As [`File::len`].
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Flush written data to durable storage.
    ///
    /// This must be a real durability point on backends that declare
    /// [`StorageCapabilities::durable_sync`]. On those that do not, it is best-effort and the
    /// engine reports the degradation rather than pretending.
    ///
    /// # Errors
    /// [`StorageError::Io`](crate::error::StorageError::Io) if the flush fails.
    fn sync_data(&mut self) -> Result<()>;

    /// Map the whole file read-only, or `None` if this backend cannot map.
    ///
    /// Returning `None` is normal — the browser has no `mmap` — and callers must have a
    /// buffered fallback.
    ///
    /// # Errors
    /// [`StorageError`](crate::error::StorageError) if mapping was attempted and failed.
    fn map_readonly(&self) -> Result<Option<Box<dyn MappedRegion>>> {
        Ok(None)
    }

    /// Read exactly `buf.len()` bytes, or fail.
    ///
    /// The common case for format decoding, where a short read means the file is truncated.
    ///
    /// # Errors
    /// [`CorruptionError::TruncatedFile`](crate::error::CorruptionError::TruncatedFile) if the
    /// file ends early.
    fn read_exact_at(&self, buf: &mut [u8], offset: u64, path: &DbPath) -> Result<()> {
        let n = self.read_at(buf, offset)?;
        if n != buf.len() {
            let actual = self.len().unwrap_or(offset.saturating_add(n as u64));
            return Err(crate::error::CorruptionError::TruncatedFile {
                path: path.clone(),
                expected_len: offset.saturating_add(buf.len() as u64),
                actual_len: actual,
            }
            .into());
        }
        Ok(())
    }
}

/// A read-only memory mapping of a whole file.
pub trait MappedRegion: Debug + Send + Sync {
    /// The mapped bytes.
    fn as_slice(&self) -> &[u8];
}

/// The single-writer lock. Released on drop.
pub trait FileLock: Debug + Send + Sync {
    /// A description of the holder, recorded in the lock so a conflict message can name it.
    fn holder(&self) -> &str;
}
