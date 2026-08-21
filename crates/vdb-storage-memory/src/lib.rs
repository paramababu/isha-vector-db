//! An in-memory [`Storage`] backend.
//!
//! This is not a toy. It has two jobs that matter:
//!
//! 1. **It is the reference implementation.** The semantics every other backend must match are
//!    defined by what this one does, and it is small enough to read in one sitting.
//! 2. **It makes the persistence test suite fast.** Every recovery and crash test in the engine
//!    runs against this backend, which means the whole fault-injection sweep finishes in seconds
//!    on any CI runner, with no filesystem, no temp directories, and no platform differences.
//!    A test suite that is slow gets skipped, and a skipped suite is worthless.
//!
//! It also models the one thing an in-memory store would otherwise hide: the difference between
//! *written* and *durable*. Writes land in a live buffer; [`File::sync_data`] copies that buffer
//! to a durable shadow; [`MemoryStorage::simulate_power_loss`] discards everything not synced.
//! That is what lets a test assert the WAL protocol is actually correct rather than accidentally
//! correct because memory never loses anything.

#![forbid(unsafe_code)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]
#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use vdb_core::error::{DbError, Result, StorageError, StorageOp};
use vdb_core::path::DbPath;
use vdb_core::storage::{
    DirEntry, EntryKind, File, FileLock, FileMeta, OpenMode, Storage, StorageCapabilities,
};

/// Bytes of one file, in both its live and its durable state.
#[derive(Debug, Default)]
struct FileData {
    /// What a reader sees now.
    live: Vec<u8>,
    /// What would survive a power cut.
    durable: Vec<u8>,
}

type SharedFile = Arc<Mutex<FileData>>;

#[derive(Debug)]
enum Entry {
    Dir,
    File(SharedFile),
}

#[derive(Debug, Default)]
struct Inner {
    /// Keyed by `DbPath::as_str()`. The root (`""`) is always present.
    entries: BTreeMap<String, Entry>,
    locks: BTreeSet<String>,
}

/// An in-memory filesystem.
///
/// Cloning shares the same contents, so a test can hold a handle to inspect state while the
/// engine holds another.
#[derive(Debug, Clone, Default)]
pub struct MemoryStorage {
    inner: Arc<Mutex<Inner>>,
}

impl MemoryStorage {
    /// An empty filesystem containing only the root directory.
    pub fn new() -> Self {
        let mut entries = BTreeMap::new();
        entries.insert(String::new(), Entry::Dir);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries,
                locks: BTreeSet::new(),
            })),
        }
    }

    /// Discard every byte written since the last [`File::sync_data`], on every file.
    ///
    /// This models power loss, not process death: a process that dies leaves its writes in the
    /// OS page cache, where they still reach the disk. Recovery tests use this to prove the
    /// engine's durability claims hold under the harsher of the two failures.
    pub fn simulate_power_loss(&self) {
        let inner = self.lock_inner();
        for entry in inner.entries.values() {
            if let Entry::File(f) = entry {
                let mut data = f.lock().unwrap_or_else(PoisonError::into_inner);
                data.live = data.durable.clone();
            }
        }
    }

    /// Total live bytes across all files, for size assertions in tests.
    pub fn total_bytes(&self) -> u64 {
        let inner = self.lock_inner();
        inner
            .entries
            .values()
            .filter_map(|e| match e {
                Entry::File(f) => {
                    Some(f.lock().unwrap_or_else(PoisonError::into_inner).live.len() as u64)
                }
                Entry::Dir => None,
            })
            .sum()
    }

    /// Every file path currently present, sorted. Useful for asserting that a flush left no
    /// orphan segments behind.
    pub fn file_paths(&self) -> Vec<String> {
        let inner = self.lock_inner();
        inner
            .entries
            .iter()
            .filter(|(_, e)| matches!(e, Entry::File(_)))
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// A copy of a file's live bytes, for tests that inspect or corrupt the format directly.
    pub fn read_all(&self, path: &DbPath) -> Option<Vec<u8>> {
        let inner = self.lock_inner();
        match inner.entries.get(path.as_str()) {
            Some(Entry::File(f)) => Some(
                f.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .live
                    .clone(),
            ),
            _ => None,
        }
    }

    /// Overwrite a file's live bytes, creating it if necessary.
    ///
    /// The corruption tests use this to flip a bit or truncate a file and then assert the
    /// engine reports a `CorruptionError` rather than panicking.
    pub fn write_all(&self, path: &DbPath, bytes: Vec<u8>) {
        let mut inner = self.lock_inner();
        let shared = match inner.entries.get(path.as_str()) {
            Some(Entry::File(f)) => Arc::clone(f),
            _ => {
                let f: SharedFile = Arc::new(Mutex::new(FileData::default()));
                inner
                    .entries
                    .insert(path.as_str().to_owned(), Entry::File(Arc::clone(&f)));
                f
            }
        };
        drop(inner);
        let mut data = shared.lock().unwrap_or_else(PoisonError::into_inner);
        data.live.clone_from(&bytes);
        data.durable = bytes;
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn not_found(path: &DbPath) -> DbError {
    StorageError::NotFound { path: path.clone() }.into()
}

fn io_err(path: &DbPath, operation: StorageOp, detail: &str) -> DbError {
    StorageError::Io {
        path: path.clone(),
        operation,
        detail: detail.to_owned(),
    }
    .into()
}

impl Inner {
    /// Whether `path` names a directory that exists.
    fn is_dir(&self, path: &str) -> bool {
        matches!(self.entries.get(path), Some(Entry::Dir))
    }

    /// The engine must not be able to create a file in a directory that does not exist; letting
    /// it would hide path-construction bugs that a real filesystem would catch.
    fn require_parent_dir(&self, path: &DbPath) -> Result<()> {
        let parent = path.parent().unwrap_or_else(DbPath::root);
        if self.is_dir(parent.as_str()) {
            Ok(())
        } else {
            Err(not_found(&parent))
        }
    }

    fn children_of(&self, dir: &str) -> Vec<DirEntry> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };
        let mut out = Vec::new();
        for (path, entry) in &self.entries {
            if path.is_empty() || !path.starts_with(&prefix) {
                continue;
            }
            let rest = &path[prefix.len()..];
            if rest.is_empty() || rest.contains('/') {
                continue;
            }
            out.push(DirEntry {
                name: rest.to_owned(),
                kind: match entry {
                    Entry::Dir => EntryKind::Directory,
                    Entry::File(_) => EntryKind::File,
                },
            });
        }
        out
    }
}

impl Storage for MemoryStorage {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::minimal()
            .with_atomic_rename(true)
            // Vacuously true: there is no medium to lose. Declaring it true is deliberate — it
            // makes the engine take exactly the same commit path it takes on a real filesystem,
            // which is the entire reason this backend is a valid place to test durability.
            .with_durable_sync(true)
            .with_file_locking(true)
    }

    fn open_file(&self, path: &DbPath, mode: OpenMode) -> Result<Box<dyn File>> {
        let mut inner = self.lock_inner();
        if inner.is_dir(path.as_str()) {
            return Err(io_err(path, StorageOp::Open, "path is a directory"));
        }
        let existing = match inner.entries.get(path.as_str()) {
            Some(Entry::File(f)) => Some(Arc::clone(f)),
            _ => None,
        };
        let shared = match (existing, mode) {
            (Some(_), OpenMode::CreateNew) => {
                return Err(StorageError::AlreadyExists { path: path.clone() }.into())
            }
            (Some(f), _) => f,
            (None, OpenMode::Read | OpenMode::ReadWrite) => return Err(not_found(path)),
            (None, _) => {
                inner.require_parent_dir(path)?;
                let f: SharedFile = Arc::new(Mutex::new(FileData::default()));
                inner
                    .entries
                    .insert(path.as_str().to_owned(), Entry::File(Arc::clone(&f)));
                f
            }
        };
        Ok(Box::new(MemoryFile {
            path: path.clone(),
            data: shared,
            writable: mode.is_writable(),
        }))
    }

    fn remove_file(&self, path: &DbPath) -> Result<()> {
        let mut inner = self.lock_inner();
        match inner.entries.get(path.as_str()) {
            Some(Entry::File(_)) => {
                inner.entries.remove(path.as_str());
                Ok(())
            }
            Some(Entry::Dir) => Err(io_err(path, StorageOp::Remove, "path is a directory")),
            None => Err(not_found(path)),
        }
    }

    fn rename(&self, from: &DbPath, to: &DbPath) -> Result<()> {
        let mut inner = self.lock_inner();
        match inner.entries.get(from.as_str()) {
            Some(Entry::File(_)) => {}
            Some(Entry::Dir) => {
                return Err(io_err(from, StorageOp::Rename, "source is a directory"))
            }
            None => return Err(not_found(from)),
        }
        if inner.is_dir(to.as_str()) {
            return Err(io_err(to, StorageOp::Rename, "destination is a directory"));
        }
        inner.require_parent_dir(to)?;
        // Atomic by construction: the whole swap happens under one lock, so no reader can
        // observe a state where the destination is half-written.
        let Some(entry) = inner.entries.remove(from.as_str()) else {
            return Err(not_found(from));
        };
        inner.entries.insert(to.as_str().to_owned(), entry);
        Ok(())
    }

    fn create_dir_all(&self, path: &DbPath) -> Result<()> {
        let mut inner = self.lock_inner();
        let mut current = DbPath::root();
        for component in path.components() {
            current = current.join(component)?;
            match inner.entries.get(current.as_str()) {
                Some(Entry::Dir) => {}
                Some(Entry::File(_)) => {
                    return Err(io_err(&current, StorageOp::CreateDir, "path is a file"))
                }
                None => {
                    inner
                        .entries
                        .insert(current.as_str().to_owned(), Entry::Dir);
                }
            }
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &DbPath) -> Result<()> {
        let mut inner = self.lock_inner();
        if !inner.is_dir(path.as_str()) {
            return Err(not_found(path));
        }
        let prefix = if path.is_root() {
            String::new()
        } else {
            format!("{}/", path.as_str())
        };
        let doomed: Vec<String> = inner
            .entries
            .keys()
            .filter(|p| !p.is_empty() && (p.as_str() == path.as_str() || p.starts_with(&prefix)))
            .cloned()
            .collect();
        for p in doomed {
            inner.entries.remove(&p);
        }
        if path.is_root() {
            inner.entries.insert(String::new(), Entry::Dir);
        }
        Ok(())
    }

    fn list_dir(&self, path: &DbPath) -> Result<Vec<DirEntry>> {
        let inner = self.lock_inner();
        if !inner.is_dir(path.as_str()) {
            return Err(not_found(path));
        }
        Ok(inner.children_of(path.as_str()))
    }

    fn metadata(&self, path: &DbPath) -> Result<Option<FileMeta>> {
        let inner = self.lock_inner();
        Ok(match inner.entries.get(path.as_str()) {
            Some(Entry::Dir) => Some(FileMeta {
                len: 0,
                kind: EntryKind::Directory,
            }),
            Some(Entry::File(f)) => {
                let len = f.lock().unwrap_or_else(PoisonError::into_inner).live.len() as u64;
                Some(FileMeta {
                    len,
                    kind: EntryKind::File,
                })
            }
            None => None,
        })
    }

    fn sync_dir(&self, path: &DbPath) -> Result<()> {
        let inner = self.lock_inner();
        if inner.is_dir(path.as_str()) {
            Ok(())
        } else {
            Err(not_found(path))
        }
    }

    fn try_lock(&self, path: &DbPath) -> Result<Box<dyn FileLock>> {
        let mut inner = self.lock_inner();
        if !inner.locks.insert(path.as_str().to_owned()) {
            return Err(StorageError::LockUnavailable { path: path.clone() }.into());
        }
        Ok(Box::new(MemoryLock {
            storage: Arc::clone(&self.inner),
            path: path.as_str().to_owned(),
            holder: "in-process".to_owned(),
        }))
    }
}

/// A handle to one file in a [`MemoryStorage`].
struct MemoryFile {
    path: DbPath,
    data: SharedFile,
    writable: bool,
}

impl fmt::Debug for MemoryFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryFile")
            .field("path", &self.path)
            .field("writable", &self.writable)
            .finish()
    }
}

impl MemoryFile {
    fn require_writable(&self, operation: StorageOp) -> Result<()> {
        if self.writable {
            Ok(())
        } else {
            Err(StorageError::PermissionDenied {
                path: self.path.clone(),
                operation,
            }
            .into())
        }
    }
}

impl File for MemoryFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        let data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(offset) = usize::try_from(offset) else {
            return Ok(0); // beyond anything addressable: end of file
        };
        let Some(available) = data.live.get(offset..) else {
            return Ok(0); // offset is past the end: end of file, not an error
        };
        let n = available.len().min(buf.len());
        match (buf.get_mut(..n), available.get(..n)) {
            (Some(dst), Some(src)) => {
                dst.copy_from_slice(src);
                Ok(n)
            }
            _ => Ok(0),
        }
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        self.require_writable(StorageOp::Write)?;
        let mut data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        let offset = usize::try_from(offset).map_err(|_| {
            io_err(
                &self.path,
                StorageOp::Write,
                "offset exceeds addressable memory",
            )
        })?;
        let end = offset.checked_add(buf.len()).ok_or_else(|| {
            io_err(
                &self.path,
                StorageOp::Write,
                "write extends past usize::MAX",
            )
        })?;
        if data.live.len() < end {
            data.live.resize(end, 0);
        }
        match data.live.get_mut(offset..end) {
            Some(dst) => {
                dst.copy_from_slice(buf);
                Ok(())
            }
            None => Err(io_err(
                &self.path,
                StorageOp::Write,
                "write range is out of bounds",
            )),
        }
    }

    fn append(&mut self, buf: &[u8]) -> Result<u64> {
        self.require_writable(StorageOp::Append)?;
        let mut data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        let offset = data.live.len() as u64;
        data.live.extend_from_slice(buf);
        Ok(offset)
    }

    fn truncate(&mut self, len: u64) -> Result<()> {
        self.require_writable(StorageOp::Truncate)?;
        let mut data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        let len = usize::try_from(len).map_err(|_| {
            io_err(
                &self.path,
                StorageOp::Truncate,
                "length exceeds addressable memory",
            )
        })?;
        data.live.resize(len, 0);
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        let data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(data.live.len() as u64)
    }

    fn sync_data(&mut self) -> Result<()> {
        self.require_writable(StorageOp::Sync)?;
        let mut data = self.data.lock().unwrap_or_else(PoisonError::into_inner);
        data.durable = data.live.clone();
        Ok(())
    }
}

/// Guard returned by [`MemoryStorage::try_lock`].
struct MemoryLock {
    storage: Arc<Mutex<Inner>>,
    path: String,
    holder: String,
}

impl fmt::Debug for MemoryLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemoryLock")
            .field("path", &self.path)
            .finish()
    }
}

impl FileLock for MemoryLock {
    fn holder(&self) -> &str {
        &self.holder
    }
}

impl Drop for MemoryLock {
    fn drop(&mut self) {
        let mut inner = self.storage.lock().unwrap_or_else(PoisonError::into_inner);
        inner.locks.remove(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> DbPath {
        DbPath::parse(s).unwrap()
    }

    /// The behaviour that makes this backend worth having: unsynced writes do not survive.
    #[test]
    fn power_loss_discards_writes_made_since_the_last_sync() {
        let s = MemoryStorage::new();
        let path = p("wal");
        let mut f = s.open_file(&path, OpenMode::Create).unwrap();
        f.append(b"committed").unwrap();
        f.sync_data().unwrap();
        f.append(b"-in-flight").unwrap();

        assert_eq!(s.read_all(&path).unwrap(), b"committed-in-flight");
        s.simulate_power_loss();
        assert_eq!(
            s.read_all(&path).unwrap(),
            b"committed",
            "everything after the last sync should be gone"
        );
    }

    #[test]
    fn power_loss_on_a_never_synced_file_empties_it() {
        let s = MemoryStorage::new();
        let path = p("scratch");
        let mut f = s.open_file(&path, OpenMode::Create).unwrap();
        f.append(b"nothing durable here").unwrap();
        s.simulate_power_loss();
        assert_eq!(s.read_all(&path).unwrap(), b"");
    }

    #[test]
    fn power_loss_applies_to_every_file_at_once() {
        let s = MemoryStorage::new();
        for name in ["a", "b", "c"] {
            let mut f = s.open_file(&p(name), OpenMode::Create).unwrap();
            f.append(b"durable").unwrap();
            f.sync_data().unwrap();
            f.append(b"volatile").unwrap();
        }
        s.simulate_power_loss();
        for name in ["a", "b", "c"] {
            assert_eq!(s.read_all(&p(name)).unwrap(), b"durable", "{name}");
        }
    }

    /// Test helpers must be able to corrupt a file in place; that is how the format's error
    /// paths get exercised without a real disk.
    #[test]
    fn write_all_and_read_all_let_tests_corrupt_bytes_directly() {
        let s = MemoryStorage::new();
        let path = p("victim");
        s.write_all(&path, vec![1, 2, 3, 4]);
        assert_eq!(s.read_all(&path).unwrap(), vec![1, 2, 3, 4]);

        let mut bytes = s.read_all(&path).unwrap();
        bytes[2] ^= 0xFF;
        s.write_all(&path, bytes);
        assert_eq!(s.read_all(&path).unwrap(), vec![1, 2, 0xFC, 4]);

        // write_all makes the bytes durable too, so a corrupted fixture survives a power cut.
        s.simulate_power_loss();
        assert_eq!(s.read_all(&path).unwrap(), vec![1, 2, 0xFC, 4]);
    }

    #[test]
    fn read_all_on_a_missing_file_is_none() {
        let s = MemoryStorage::new();
        assert!(s.read_all(&p("ghost")).is_none());
    }

    #[test]
    fn clones_share_one_filesystem() {
        let a = MemoryStorage::new();
        let b = a.clone();
        a.write_all(&p("shared"), b"hello".to_vec());
        assert_eq!(b.read_all(&p("shared")).unwrap(), b"hello");
    }

    #[test]
    fn total_bytes_and_file_paths_report_the_contents() {
        let s = MemoryStorage::new();
        s.create_dir_all(&p("d")).unwrap();
        s.write_all(&p("d/one"), vec![0; 10]);
        s.write_all(&p("d/two"), vec![0; 22]);
        assert_eq!(s.total_bytes(), 32);
        assert_eq!(s.file_paths(), vec!["d/one".to_owned(), "d/two".to_owned()]);
    }

    #[test]
    fn remove_dir_all_on_the_root_leaves_a_usable_filesystem() {
        let s = MemoryStorage::new();
        s.write_all(&p("x"), b"x".to_vec());
        s.remove_dir_all(&DbPath::root()).unwrap();
        assert!(s.file_paths().is_empty());
        // The root must still exist, or nothing can be created afterwards.
        s.create_dir_all(&p("fresh")).unwrap();
        assert!(s.exists(&p("fresh")).unwrap());
    }

    #[test]
    fn capabilities_are_what_the_conformance_suite_verifies() {
        let caps = MemoryStorage::new().capabilities();
        assert!(caps.atomic_rename);
        assert!(caps.durable_sync);
        assert!(caps.file_locking);
        assert!(
            !caps.mmap,
            "memory storage cannot map; the suite checks map_readonly is None"
        );
    }
}
