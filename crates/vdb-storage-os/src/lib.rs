//! The filesystem [`Storage`] backend.
//!
//! Everything platform-specific about talking to a disk lives here, and nothing else in the
//! engine knows a filesystem exists. That is not a slogan: `vdb-core` and `vdb-format` are
//! checked by CI to contain no `std::fs` at all, so this crate is the entire surface.
//!
//! The whole engine test suite — including the crash sweep, which crashes at every I/O
//! operation and asserts recovery — runs against this backend unchanged. If it did not, the
//! abstraction would be leaking, and it is much cheaper to discover that now than after several
//! SDKs depend on it.
//!
//! # Three platform details that are easy to get wrong
//!
//! **`fsync` is not enough on Darwin.** macOS and iOS return from `fsync` once the data reaches
//! the drive, not once the drive has committed it; `fcntl(F_FULLFSYNC)` is what actually flushes
//! the device cache. Using plain `fsync` there means a power cut can lose writes the engine was
//! told were durable — precisely the guarantee `Durability::Full` exists to make.
//!
//! **A rename is not durable until its directory is synced.** On POSIX the rename may be
//! reordered past a crash even though the file's own data was flushed. `sync_dir` exists for
//! that, and the persistence layer calls it.
//!
//! **Advisory locks must die with the process.** A lock file created with `create_new` outlives
//! a crash, so a killed application can never reopen its own database — an unacceptable failure
//! mode on mobile, where being killed is routine. `flock` is released by the kernel when the
//! process goes away, which is the behaviour we actually need.

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

mod lock;
mod sync;

use std::fs::{self, File as StdFile, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};

use vdb_core::error::{DbError, Result, StorageError, StorageOp};
use vdb_core::path::DbPath;
use vdb_core::storage::{
    DirEntry, EntryKind, File, FileLock, FileMeta, OpenMode, Storage, StorageCapabilities,
};

/// A database directory on a real filesystem.
#[derive(Debug, Clone)]
pub struct OsStorage {
    root: PathBuf,
}

impl OsStorage {
    /// Use `root` as the database directory, creating it if it does not exist.
    ///
    /// # Errors
    /// [`StorageError`] if the directory cannot be created or is not a directory.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        if let Err(e) = fs::create_dir_all(&root) {
            return Err(map_io(e, &DbPath::root(), StorageOp::CreateDir));
        }
        if !root.is_dir() {
            return Err(StorageError::Io {
                path: DbPath::root(),
                operation: StorageOp::Open,
                detail: format!("{} is not a directory", root.display()),
            }
            .into());
        }
        Ok(Self { root })
    }

    /// The database directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a database path to a host path.
    ///
    /// [`DbPath`] cannot represent a `..` component, so traversal is already impossible by
    /// construction. This checks again anyway: two independent checks, because one of them will
    /// eventually be bypassed by a code path nobody thought about, and the cost of being wrong
    /// here is writing outside the database directory.
    fn resolve(&self, path: &DbPath) -> Result<PathBuf> {
        let mut out = self.root.clone();
        for component in path.components() {
            let candidate = Path::new(component);
            let mut parts = candidate.components();
            match (parts.next(), parts.next()) {
                (Some(Component::Normal(part)), None) => out.push(part),
                _ => {
                    return Err(StorageError::Io {
                        path: path.clone(),
                        operation: StorageOp::Open,
                        detail: format!("path component {component:?} is not a plain name"),
                    }
                    .into())
                }
            }
        }
        Ok(out)
    }
}

impl Storage for OsStorage {
    fn name(&self) -> &'static str {
        "os"
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::minimal()
            .with_atomic_rename(true)
            .with_durable_sync(true)
            .with_sparse_files(true)
            .with_file_locking(cfg!(unix))
        // `mmap` stays false. Memory-mapping the vector block is a performance change with real
        // hazards — SIGBUS on a truncated file, and dirty mapped pages counting against the iOS
        // jetsam footprint — so it lands as its own change, measured against this baseline,
        // rather than being smuggled in with the backend itself.
    }

    fn open_file(&self, path: &DbPath, mode: OpenMode) -> Result<Box<dyn File>> {
        let host = self.resolve(path)?;
        let mut options = OpenOptions::new();
        match mode {
            OpenMode::Read => options.read(true),
            OpenMode::ReadWrite => options.read(true).write(true),
            OpenMode::Create => options.read(true).write(true).create(true),
            // `create_new` is the atomic "must not already exist" the mode promises. Checking
            // for existence first and then creating would be a race.
            OpenMode::CreateNew => options.read(true).write(true).create_new(true),
            _ => options.read(true),
        };
        let file = options
            .open(&host)
            .map_err(|e| map_io(e, path, StorageOp::Open))?;

        // POSIX lets you open a directory for reading — you get a descriptor, and every read
        // then fails with EISDIR somewhere much deeper. The in-memory backend refuses outright,
        // and the conformance suite caught the divergence. Checking the handle rather than the
        // path avoids a time-of-check race: whatever we opened is what we are inspecting.
        let meta = file
            .metadata()
            .map_err(|e| map_io(e, path, StorageOp::Metadata))?;
        if meta.is_dir() {
            return Err(StorageError::Io {
                path: path.clone(),
                operation: StorageOp::Open,
                detail: "path is a directory".to_owned(),
            }
            .into());
        }
        Ok(Box::new(OsFile {
            file,
            path: path.clone(),
            writable: mode.is_writable(),
        }))
    }

    fn remove_file(&self, path: &DbPath) -> Result<()> {
        let host = self.resolve(path)?;
        fs::remove_file(&host).map_err(|e| map_io(e, path, StorageOp::Remove))
    }

    fn rename(&self, from: &DbPath, to: &DbPath) -> Result<()> {
        let from_host = self.resolve(from)?;
        let to_host = self.resolve(to)?;
        // `fs::rename` replaces an existing destination on both POSIX and Windows, which is the
        // all-or-nothing behaviour the capability promises.
        fs::rename(&from_host, &to_host).map_err(|e| map_io(e, from, StorageOp::Rename))
    }

    fn create_dir_all(&self, path: &DbPath) -> Result<()> {
        let host = self.resolve(path)?;
        fs::create_dir_all(&host).map_err(|e| map_io(e, path, StorageOp::CreateDir))
    }

    fn remove_dir_all(&self, path: &DbPath) -> Result<()> {
        let host = self.resolve(path)?;
        if path.is_root() {
            // Emptying the root must leave the root itself, or nothing can be created after.
            for entry in fs::read_dir(&host).map_err(|e| map_io(e, path, StorageOp::ListDir))? {
                let entry = entry.map_err(|e| map_io(e, path, StorageOp::ListDir))?;
                let result = if entry.path().is_dir() {
                    fs::remove_dir_all(entry.path())
                } else {
                    fs::remove_file(entry.path())
                };
                result.map_err(|e| map_io(e, path, StorageOp::Remove))?;
            }
            return Ok(());
        }
        fs::remove_dir_all(&host).map_err(|e| map_io(e, path, StorageOp::Remove))
    }

    fn list_dir(&self, path: &DbPath) -> Result<Vec<DirEntry>> {
        let host = self.resolve(path)?;
        let mut out = Vec::new();
        for entry in fs::read_dir(&host).map_err(|e| map_io(e, path, StorageOp::ListDir))? {
            let entry = entry.map_err(|e| map_io(e, path, StorageOp::ListDir))?;
            let kind = if entry.path().is_dir() {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            out.push(DirEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                kind,
            });
        }
        Ok(out)
    }

    fn metadata(&self, path: &DbPath) -> Result<Option<FileMeta>> {
        let host = self.resolve(path)?;
        match fs::metadata(&host) {
            Ok(m) => Ok(Some(FileMeta {
                len: if m.is_dir() { 0 } else { m.len() },
                kind: if m.is_dir() {
                    EntryKind::Directory
                } else {
                    EntryKind::File
                },
            })),
            // Absence is a question's answer, not a failure.
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(map_io(e, path, StorageOp::Metadata)),
        }
    }

    fn sync_dir(&self, path: &DbPath) -> Result<()> {
        let host = self.resolve(path)?;
        sync::sync_directory(&host).map_err(|e| map_io(e, path, StorageOp::Sync))
    }

    fn try_lock(&self, path: &DbPath) -> Result<Box<dyn FileLock>> {
        let host = self.resolve(path)?;
        lock::acquire(&host, path)
    }
}

/// One open file.
#[derive(Debug)]
struct OsFile {
    file: StdFile,
    path: DbPath,
    writable: bool,
}

impl OsFile {
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

impl File for OsFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        sync::read_at(&self.file, buf, offset).map_err(|e| map_io(e, &self.path, StorageOp::Read))
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        self.require_writable(StorageOp::Write)?;
        sync::write_all_at(&self.file, buf, offset)
            .map_err(|e| map_io(e, &self.path, StorageOp::Write))
    }

    fn append(&mut self, buf: &[u8]) -> Result<u64> {
        self.require_writable(StorageOp::Append)?;
        // The length is read and then written to, rather than relying on O_APPEND, because the
        // trait promises the offset the bytes landed at. The single-writer lock is what makes
        // that safe; the engine also checks the returned offset against its own idea of the
        // file length, so a second writer is detected rather than silently interleaving.
        let end = self
            .file
            .metadata()
            .map_err(|e| map_io(e, &self.path, StorageOp::Metadata))?
            .len();
        sync::write_all_at(&self.file, buf, end)
            .map_err(|e| map_io(e, &self.path, StorageOp::Append))?;
        Ok(end)
    }

    fn truncate(&mut self, len: u64) -> Result<()> {
        self.require_writable(StorageOp::Truncate)?;
        self.file
            .set_len(len)
            .map_err(|e| map_io(e, &self.path, StorageOp::Truncate))
    }

    fn len(&self) -> Result<u64> {
        Ok(self
            .file
            .metadata()
            .map_err(|e| map_io(e, &self.path, StorageOp::Metadata))?
            .len())
    }

    fn sync_data(&mut self) -> Result<()> {
        self.require_writable(StorageOp::Sync)?;
        sync::sync_file(&self.file).map_err(|e| map_io(e, &self.path, StorageOp::Sync))
    }
}

/// Translate an OS error into the engine's vocabulary.
///
/// Classification matters: the engine's `Recoverability` is derived from the variant, so a full
/// disk that arrived as a generic I/O error would be reported to the user as unrecoverable when
/// it is the most recoverable failure there is.
fn map_io(e: io::Error, path: &DbPath, operation: StorageOp) -> DbError {
    match e.kind() {
        io::ErrorKind::NotFound => StorageError::NotFound { path: path.clone() }.into(),
        io::ErrorKind::AlreadyExists => StorageError::AlreadyExists { path: path.clone() }.into(),
        io::ErrorKind::PermissionDenied => StorageError::PermissionDenied {
            path: path.clone(),
            operation,
        }
        .into(),
        _ => {
            // ENOSPC and EDQUOT have no stable `ErrorKind` on our MSRV, so they are recognised
            // by errno. Getting this wrong would tell a user their database is corrupt when
            // their disk is merely full.
            #[cfg(unix)]
            if matches!(e.raw_os_error(), Some(libc::ENOSPC) | Some(libc::EDQUOT)) {
                return StorageError::InsufficientStorage {
                    required: 0,
                    available: None,
                }
                .into();
            }
            StorageError::Io {
                path: path.clone(),
                operation,
                detail: e.to_string(),
            }
            .into()
        }
    }
}
