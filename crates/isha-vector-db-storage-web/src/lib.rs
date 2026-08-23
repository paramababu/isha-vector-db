//! Storage for the web: a [`Storage`] implementation that delegates to host functions the
//! embedder provides, so the engine can run on OPFS without knowing OPFS exists.
//!
//! # Where this sits
//!
//! `isha-vector-db-core` performs no I/O and knows nothing about any platform. This crate is one of its
//! storage backends, the sibling of `isha-vector-db-storage-os`. It contains no browser code either — it
//! calls the imports declared in [`host`], and something on the other side of the WebAssembly
//! boundary implements them. In a browser that is `sdk/web/src/host.js` driving OPFS synchronous
//! access handles inside a Worker. In tests it is a Rust implementation, which is what lets the
//! storage conformance suite run against this translation layer natively.
//!
//! # Durability
//!
//! OPFS `flush()` is best-effort: it is not a barrier against power loss the way `F_FULLFSYNC`
//! is. [`WebStorage::capabilities`] therefore reports `durable_sync: false`, and the engine
//! downgrades what it promises rather than claiming a guarantee the platform cannot keep. The
//! write-ahead log still protects against the failure that actually happens in a browser — the
//! tab going away — because that loses no writes the host has accepted.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::missing_errors_doc)]

pub mod host;
// Compiled on every target except wasm. On wasm the host imports are supplied by the embedder,
// so a second definition would collide; everywhere else this crate exists only to be tested, and
// having the double always present is what lets `cargo test --workspace` run the conformance
// suite without anyone remembering a feature flag.
#[cfg(not(target_arch = "wasm32"))]
pub mod test_host;

use std::fmt;

use isha_vector_db_core::error::{DbError, StorageError, StorageOp};
use isha_vector_db_core::path::DbPath;
use isha_vector_db_core::storage::{
    DirEntry, EntryKind, File, FileLock, FileMeta, OpenMode, Storage, StorageCapabilities,
};
use isha_vector_db_core::Result;

use host::{code, mode};

/// Largest offset an `f64` represents exactly. See [`host`] for why offsets are doubles.
const MAX_EXACT: u64 = 1 << 53;

/// Convert an offset for the host, refusing anything an `f64` cannot represent exactly rather
/// than silently rounding it into a different part of the file.
fn offset_to_f64(offset: u64, path: &DbPath) -> Result<f64> {
    if offset > MAX_EXACT {
        return Err(StorageError::Io {
            path: path.clone(),
            operation: StorageOp::Metadata,
            detail: "offset exceeds 2^53, which this backend cannot address exactly".to_owned(),
        }
        .into());
    }
    Ok(offset as f64)
}

/// The engine's storage over a web host.
pub struct WebStorage {
    root: String,
}

impl fmt::Debug for WebStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebStorage")
            .field("root", &self.root)
            .finish()
    }
}

// Every host call is made from the single thread that owns the WebAssembly instance. The engine
// requires `Send + Sync` because it is written once for every platform, not because this backend
// is ever used from two threads: `wasm32-unknown-unknown` has no threads unless the embedder
// deliberately builds them, and the web SDK runs the engine in one dedicated Worker.
unsafe impl Send for WebStorage {}
unsafe impl Sync for WebStorage {}

impl WebStorage {
    /// Open storage rooted at `root`, which the host interprets — a directory name within the
    /// origin's private filesystem, in the OPFS case.
    pub fn open(root: impl Into<String>) -> Self {
        Self { root: root.into() }
    }

    /// The absolute path the host should act on.
    fn resolve(&self, path: &DbPath) -> String {
        if path.is_root() {
            return self.root.clone();
        }
        format!("{}/{}", self.root, path.as_str())
    }

    /// Call a host function that takes one path and returns a status.
    fn with_path<F>(&self, path: &DbPath, op: StorageOp, f: F) -> Result<()>
    where
        F: FnOnce(*const u8, usize) -> i32,
    {
        let full = self.resolve(path);
        let rc = f(full.as_ptr(), full.len());
        if rc < 0 {
            return Err(host::to_error(rc, path, op));
        }
        Ok(())
    }
}

impl Storage for WebStorage {
    fn name(&self) -> &'static str {
        "web"
    }

    fn describe(&self) -> String {
        self.root.clone()
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::minimal()
            // OPFS has no rename that a concurrent reader observes atomically, which is exactly
            // the case the dual-slot manifest was designed for.
            .with_atomic_rename(false)
            // `flush()` on a sync access handle is best-effort, not a power-loss barrier.
            .with_durable_sync(false)
            // Locking is cooperative here: one Worker owns the database.
            .with_file_locking(true)
            // Per-file overhead dominates in OPFS, so the engine should prefer fewer, larger
            // files where it has the choice.
            .with_prefers_few_large_files(true)
            .with_max_file_size(Some(MAX_EXACT))
    }

    fn open_file(&self, path: &DbPath, open_mode: OpenMode) -> Result<Box<dyn File>> {
        let full = self.resolve(path);
        let m = match open_mode {
            OpenMode::Read => mode::READ,
            OpenMode::ReadWrite => mode::READ_WRITE,
            OpenMode::Create => mode::CREATE,
            OpenMode::CreateNew => mode::CREATE_NEW,
            // `OpenMode` is non-exhaustive. A mode this backend has never heard of must not be
            // silently downgraded to a weaker one, so it is refused.
            _ => {
                return Err(StorageError::Io {
                    path: path.clone(),
                    operation: StorageOp::Open,
                    detail: "this backend does not support the requested open mode".to_owned(),
                }
                .into())
            }
        };
        let rc = unsafe { host::vdb_host_open(full.as_ptr(), full.len(), m) };
        if rc < 0 {
            return Err(host::to_error(rc, path, StorageOp::Open));
        }
        Ok(Box::new(WebFile {
            handle: rc,
            path: path.clone(),
            writable: open_mode.is_writable(),
        }))
    }

    fn remove_file(&self, path: &DbPath) -> Result<()> {
        self.with_path(path, StorageOp::Remove, |p, n| unsafe {
            host::vdb_host_remove_file(p, n)
        })
    }

    /// Always refused.
    ///
    /// OPFS can move a file, but nothing guarantees a concurrent reader observes the move as
    /// all-or-nothing, and an emulated rename — copy, then delete — is precisely the
    /// non-atomic thing the dual-slot manifest exists to avoid depending on. Declaring the
    /// capability false and then quietly providing the operation anyway would let a caller
    /// build on a guarantee this platform cannot keep, so the operation is not offered at all.
    /// The conformance suite requires exactly this, and caught it when the first draft of this
    /// backend emulated the move.
    fn rename(&self, _from: &DbPath, _to: &DbPath) -> Result<()> {
        Err(StorageError::Unsupported {
            operation: StorageOp::Rename,
            backend: "web",
        }
        .into())
    }

    fn create_dir_all(&self, path: &DbPath) -> Result<()> {
        self.with_path(path, StorageOp::CreateDir, |p, n| unsafe {
            host::vdb_host_create_dir_all(p, n)
        })
    }

    fn remove_dir_all(&self, path: &DbPath) -> Result<()> {
        self.with_path(path, StorageOp::Remove, |p, n| unsafe {
            host::vdb_host_remove_dir_all(p, n)
        })
    }

    fn sync_dir(&self, path: &DbPath) -> Result<()> {
        self.with_path(path, StorageOp::Sync, |p, n| unsafe {
            host::vdb_host_sync_dir(p, n)
        })
    }

    fn metadata(&self, path: &DbPath) -> Result<Option<FileMeta>> {
        let full = self.resolve(path);
        let mut len = 0f64;
        let mut kind = 0u32;
        let rc = unsafe { host::vdb_host_metadata(full.as_ptr(), full.len(), &mut len, &mut kind) };
        if rc == code::NOT_FOUND {
            return Ok(None);
        }
        if rc < 0 {
            return Err(host::to_error(rc, path, StorageOp::Metadata));
        }
        Ok(Some(FileMeta {
            len: len as u64,
            kind: if kind == 1 {
                EntryKind::Directory
            } else {
                EntryKind::File
            },
        }))
    }

    fn try_lock(&self, path: &DbPath) -> Result<Box<dyn FileLock>> {
        let full = self.resolve(path);
        let rc = unsafe { host::vdb_host_lock(full.as_ptr(), full.len()) };
        if rc == code::LOCKED {
            return Err(StorageError::LockUnavailable { path: path.clone() }.into());
        }
        if rc < 0 {
            return Err(host::to_error(rc, path, StorageOp::Lock));
        }
        Ok(Box::new(WebLock {
            path: full,
            holder: "this worker".to_owned(),
        }))
    }

    fn list_dir(&self, path: &DbPath) -> Result<Vec<DirEntry>> {
        let full = self.resolve(path);
        // Grow rather than ask twice: a second call would race with a concurrent write in a
        // host that permits one, and the retry loop is bounded by the buffer doubling.
        let mut buf = vec![0u8; 4096];
        loop {
            let rc = unsafe {
                host::vdb_host_list_dir(full.as_ptr(), full.len(), buf.as_mut_ptr(), buf.len())
            };
            if rc == code::BUFFER_TOO_SMALL {
                if buf.len() >= 1 << 24 {
                    return Err(StorageError::Io {
                        path: path.clone(),
                        operation: StorageOp::ListDir,
                        detail: "directory listing exceeds 16 MiB".to_owned(),
                    }
                    .into());
                }
                buf.resize(buf.len() * 4, 0);
                continue;
            }
            if rc < 0 {
                return Err(host::to_error(rc, path, StorageOp::ListDir));
            }
            // The host is not trusted to respect the capacity it was given. A count larger
            // than the buffer is a host bug, but it must surface as an error rather than a
            // panic — this crate's whole job is to be defensive about the other side.
            let written = buf.get(..rc as usize).ok_or_else(|| {
                DbError::from(StorageError::Io {
                    path: path.clone(),
                    operation: StorageOp::ListDir,
                    detail: "the host wrote more listing bytes than the buffer holds".to_owned(),
                })
            })?;
            return parse_listing(written, path);
        }
    }
}

/// Parse the host's listing format: one `f`/`d` byte, the name, then a newline.
///
/// A name containing a newline would be ambiguous. The host is required not to produce one, and
/// this rejects the record rather than mis-splitting it, because a silently truncated directory
/// listing is how a segment goes missing.
fn parse_listing(bytes: &[u8], path: &DbPath) -> Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    for record in bytes.split(|b| *b == b'\n') {
        if record.is_empty() {
            continue;
        }
        let (kind, name) = record.split_at(1);
        let kind = match kind.first() {
            Some(b'f') => EntryKind::File,
            Some(b'd') => EntryKind::Directory,
            _ => {
                return Err(StorageError::Io {
                    path: path.clone(),
                    operation: StorageOp::ListDir,
                    detail: "the host returned a listing record with no kind byte".to_owned(),
                }
                .into())
            }
        };
        let name = core::str::from_utf8(name).map_err(|_| {
            DbError::from(StorageError::Io {
                path: path.clone(),
                operation: StorageOp::ListDir,
                detail: "the host returned a name that is not valid UTF-8".to_owned(),
            })
        })?;
        out.push(DirEntry {
            name: name.to_owned(),
            kind,
        });
    }
    Ok(out)
}

/// One open file.
#[derive(Debug)]
struct WebFile {
    handle: i32,
    path: DbPath,
    writable: bool,
}

// Same reasoning as `WebStorage`: the handle is only ever used from the thread that owns the
// instance.
unsafe impl Send for WebFile {}
unsafe impl Sync for WebFile {}

impl WebFile {
    fn refuse_write(&self) -> Result<()> {
        if self.writable {
            return Ok(());
        }
        Err(StorageError::PermissionDenied {
            path: self.path.clone(),
            operation: StorageOp::Write,
        }
        .into())
    }
}

impl File for WebFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let off = offset_to_f64(offset, &self.path)?;
        let rc = unsafe { host::vdb_host_read(self.handle, buf.as_mut_ptr(), buf.len(), off) };
        if rc < 0 {
            return Err(host::to_error(rc, &self.path, StorageOp::Read));
        }
        Ok(rc as usize)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        self.refuse_write()?;
        if buf.is_empty() {
            return Ok(());
        }
        let off = offset_to_f64(offset, &self.path)?;
        let rc = unsafe { host::vdb_host_write(self.handle, buf.as_ptr(), buf.len(), off) };
        if rc < 0 {
            return Err(host::to_error(rc, &self.path, StorageOp::Write));
        }
        Ok(())
    }

    fn append(&mut self, buf: &[u8]) -> Result<u64> {
        let end = self.len()?;
        self.write_at(buf, end)?;
        Ok(end)
    }

    fn truncate(&mut self, len: u64) -> Result<()> {
        self.refuse_write()?;
        let n = offset_to_f64(len, &self.path)?;
        let rc = unsafe { host::vdb_host_truncate(self.handle, n) };
        if rc < 0 {
            return Err(host::to_error(rc, &self.path, StorageOp::Truncate));
        }
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        let n = unsafe { host::vdb_host_size(self.handle) };
        if n < 0.0 {
            return Err(host::to_error(n as i32, &self.path, StorageOp::Metadata));
        }
        Ok(n as u64)
    }

    fn sync_data(&mut self) -> Result<()> {
        let rc = unsafe { host::vdb_host_sync(self.handle) };
        if rc < 0 {
            return Err(host::to_error(rc, &self.path, StorageOp::Sync));
        }
        Ok(())
    }
}

impl Drop for WebFile {
    fn drop(&mut self) {
        // A leaked OPFS sync access handle keeps an exclusive claim on the file until the page
        // goes away, so closing is not optional. There is nowhere to report a failure from a
        // destructor, and retrying would risk closing a handle the host has already reused.
        let _ = unsafe { host::vdb_host_close(self.handle) };
    }
}

/// An advisory lock held on the host's behalf.
#[derive(Debug)]
pub struct WebLock {
    path: String,
    holder: String,
}

unsafe impl Send for WebLock {}
unsafe impl Sync for WebLock {}

impl FileLock for WebLock {
    fn holder(&self) -> &str {
        &self.holder
    }
}

impl Drop for WebLock {
    fn drop(&mut self) {
        let _ = unsafe { host::vdb_host_unlock(self.path.as_ptr(), self.path.len()) };
    }
}
