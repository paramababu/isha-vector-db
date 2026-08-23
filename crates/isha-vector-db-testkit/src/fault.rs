//! Fault-injecting storage: the centrepiece of the durability test suite.
//!
//! Wraps any [`Storage`] and makes it fail at a chosen operation, in a chosen way. The driver
//! (see `crash_sweep`) runs a workload once per I/O operation in it, crashing at each index in
//! turn, and asserts after every one that reopening the database yields a consistent state.
//!
//! This is worth more than any amount of hand-written "does it save?" testing, because the bugs
//! it finds are the ones that would otherwise be found by a user losing their data on a subway
//! platform — a crash at operation 47 of 300, in a sequence nobody would think to write by hand.
//!
//! Because the in-memory storage backend makes the whole sweep run without touching a
//! disk, it finishes in seconds and executes identically on every CI runner, so it can run on
//! every push rather than nightly. A durability suite that is too slow to run is not a
//! durability suite.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use isha_vector_db_core::error::{DbError, Result, StorageError, StorageOp};
use isha_vector_db_core::path::DbPath;
use isha_vector_db_core::storage::{
    DirEntry, File, FileLock, FileMeta, OpenMode, Storage, StorageCapabilities,
};

/// What goes wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Fault {
    /// The process dies. The operation does not happen, and nothing afterwards does either.
    ///
    /// Models an OS kill: the most common failure on mobile by a wide margin.
    Crash,
    /// Only the first `prefix` bytes of the write land, then the process dies.
    ///
    /// Models a write interrupted part-way through — the case that leaves a half-written frame
    /// at the end of a log.
    TornWrite {
        /// Bytes that reach storage before the failure.
        prefix: usize,
    },
    /// The volume is full.
    NoSpace,
    /// A generic I/O failure that does not stop subsequent operations.
    ///
    /// Models a transient error, so the engine's error propagation is exercised without the
    /// whole run ending.
    Transient,
    /// `sync_data` returns success without making anything durable.
    ///
    /// Models a lying filesystem, and the reason durability must be tested with
    /// `simulate_power_loss` rather than assumed from a successful sync.
    DropSync,
}

/// Which operations the injector counts and can fail at.
///
/// Only state-changing operations. Reads are not counted, because a crash during a read leaves
/// nothing to recover and would only dilute the sweep with uninteresting indices.
fn is_mutating(op: StorageOp) -> bool {
    matches!(
        op,
        StorageOp::Write
            | StorageOp::Append
            | StorageOp::Sync
            | StorageOp::Truncate
            | StorageOp::Remove
            | StorageOp::Rename
            | StorageOp::CreateDir
            | StorageOp::Open
    )
}

#[derive(Debug)]
struct Injector {
    /// Mutating operations seen so far.
    counter: AtomicU64,
    /// Which operation index to fail at, if any.
    fault_at: Option<u64>,
    /// What to do at that index.
    fault: Fault,
    /// Set once a crash-class fault has fired.
    crashed: Mutex<bool>,
}

impl Injector {
    /// Decide what to do about the next mutating operation.
    fn check(&self, op: StorageOp, path: &DbPath) -> Injection {
        if !is_mutating(op) {
            return Injection::Proceed;
        }
        if *self.crashed.lock().unwrap_or_else(PoisonError::into_inner) {
            return Injection::Fail(crashed_error(path, op));
        }
        let index = self.counter.fetch_add(1, Ordering::SeqCst);
        if Some(index) != self.fault_at {
            return Injection::Proceed;
        }
        match self.fault {
            Fault::Crash => {
                self.mark_crashed();
                Injection::Fail(crashed_error(path, op))
            }
            Fault::TornWrite { prefix } => {
                self.mark_crashed();
                Injection::Tear(prefix)
            }
            Fault::NoSpace => Injection::Fail(
                StorageError::InsufficientStorage {
                    required: 0,
                    available: Some(0),
                }
                .into(),
            ),
            Fault::Transient => Injection::Fail(
                StorageError::Io {
                    path: path.clone(),
                    operation: op,
                    detail: "injected transient failure".to_owned(),
                }
                .into(),
            ),
            Fault::DropSync => {
                if op == StorageOp::Sync {
                    Injection::Swallow
                } else {
                    Injection::Proceed
                }
            }
        }
    }

    fn mark_crashed(&self) {
        *self.crashed.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }
}

enum Injection {
    /// Perform the operation normally.
    Proceed,
    /// Fail with this error.
    Fail(DbError),
    /// Write only this many bytes, then behave as crashed.
    Tear(usize),
    /// Report success without doing anything.
    Swallow,
}

fn crashed_error(path: &DbPath, op: StorageOp) -> DbError {
    StorageError::Io {
        path: path.clone(),
        operation: op,
        detail: "injected crash: the process is gone".to_owned(),
    }
    .into()
}

/// A [`Storage`] that fails on demand.
#[derive(Clone)]
pub struct FaultyStorage {
    inner: Arc<dyn Storage>,
    injector: Arc<Injector>,
}

impl fmt::Debug for FaultyStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultyStorage")
            .field("inner", &self.inner.name())
            .field("fault_at", &self.injector.fault_at)
            .field("fault", &self.injector.fault)
            .field("ops", &self.op_count())
            .finish()
    }
}

impl FaultyStorage {
    /// Wrap a backend without injecting anything, to count the operations a workload performs.
    pub fn counting(inner: Arc<dyn Storage>) -> Self {
        Self {
            inner,
            injector: Arc::new(Injector {
                counter: AtomicU64::new(0),
                fault_at: None,
                fault: Fault::Crash,
                crashed: Mutex::new(false),
            }),
        }
    }

    /// Wrap a backend, failing at mutating operation number `at`.
    pub fn failing_at(inner: Arc<dyn Storage>, at: u64, fault: Fault) -> Self {
        Self {
            inner,
            injector: Arc::new(Injector {
                counter: AtomicU64::new(0),
                fault_at: Some(at),
                fault,
                crashed: Mutex::new(false),
            }),
        }
    }

    /// Mutating operations performed so far.
    pub fn op_count(&self) -> u64 {
        self.injector.counter.load(Ordering::SeqCst)
    }

    /// Whether a crash-class fault has fired.
    pub fn crashed(&self) -> bool {
        *self
            .injector
            .crashed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Storage for FaultyStorage {
    fn name(&self) -> &'static str {
        "faulty"
    }

    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }

    fn open_file(&self, path: &DbPath, mode: OpenMode) -> Result<Box<dyn File>> {
        // Only creating counts as mutating; opening for reading changes nothing.
        if mode.is_writable() {
            match self.injector.check(StorageOp::Open, path) {
                Injection::Fail(e) => return Err(e),
                Injection::Tear(_) | Injection::Swallow | Injection::Proceed => {}
            }
        }
        let file = self.inner.open_file(path, mode)?;
        Ok(Box::new(FaultyFile {
            inner: file,
            path: path.clone(),
            injector: Arc::clone(&self.injector),
        }))
    }

    fn remove_file(&self, path: &DbPath) -> Result<()> {
        self.guard(StorageOp::Remove, path)?;
        self.inner.remove_file(path)
    }

    fn rename(&self, from: &DbPath, to: &DbPath) -> Result<()> {
        self.guard(StorageOp::Rename, from)?;
        self.inner.rename(from, to)
    }

    fn create_dir_all(&self, path: &DbPath) -> Result<()> {
        self.guard(StorageOp::CreateDir, path)?;
        self.inner.create_dir_all(path)
    }

    fn remove_dir_all(&self, path: &DbPath) -> Result<()> {
        self.guard(StorageOp::Remove, path)?;
        self.inner.remove_dir_all(path)
    }

    fn list_dir(&self, path: &DbPath) -> Result<Vec<DirEntry>> {
        self.inner.list_dir(path)
    }

    fn metadata(&self, path: &DbPath) -> Result<Option<FileMeta>> {
        self.inner.metadata(path)
    }

    fn sync_dir(&self, path: &DbPath) -> Result<()> {
        self.guard(StorageOp::Sync, path)?;
        self.inner.sync_dir(path)
    }

    fn try_lock(&self, path: &DbPath) -> Result<Box<dyn FileLock>> {
        self.inner.try_lock(path)
    }
}

impl FaultyStorage {
    fn guard(&self, op: StorageOp, path: &DbPath) -> Result<()> {
        match self.injector.check(op, path) {
            Injection::Fail(e) => Err(e),
            Injection::Proceed | Injection::Tear(_) | Injection::Swallow => Ok(()),
        }
    }
}

struct FaultyFile {
    inner: Box<dyn File>,
    path: DbPath,
    injector: Arc<Injector>,
}

impl fmt::Debug for FaultyFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FaultyFile")
            .field("path", &self.path)
            .finish()
    }
}

impl File for FaultyFile {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> Result<usize> {
        self.inner.read_at(buf, offset)
    }

    fn write_at(&mut self, buf: &[u8], offset: u64) -> Result<()> {
        match self.injector.check(StorageOp::Write, &self.path) {
            Injection::Proceed => self.inner.write_at(buf, offset),
            Injection::Fail(e) => Err(e),
            Injection::Swallow => Ok(()),
            Injection::Tear(prefix) => {
                let n = prefix.min(buf.len());
                if let Some(partial) = buf.get(..n) {
                    self.inner.write_at(partial, offset)?;
                }
                Err(crashed_error(&self.path, StorageOp::Write))
            }
        }
    }

    fn append(&mut self, buf: &[u8]) -> Result<u64> {
        match self.injector.check(StorageOp::Append, &self.path) {
            Injection::Proceed => self.inner.append(buf),
            Injection::Fail(e) => Err(e),
            Injection::Swallow => self.inner.len(),
            Injection::Tear(prefix) => {
                let n = prefix.min(buf.len());
                if let Some(partial) = buf.get(..n) {
                    self.inner.append(partial)?;
                }
                Err(crashed_error(&self.path, StorageOp::Append))
            }
        }
    }

    fn truncate(&mut self, len: u64) -> Result<()> {
        match self.injector.check(StorageOp::Truncate, &self.path) {
            Injection::Proceed => self.inner.truncate(len),
            Injection::Fail(e) => Err(e),
            Injection::Swallow | Injection::Tear(_) => Ok(()),
        }
    }

    fn len(&self) -> Result<u64> {
        self.inner.len()
    }

    fn sync_data(&mut self) -> Result<()> {
        match self.injector.check(StorageOp::Sync, &self.path) {
            Injection::Proceed => self.inner.sync_data(),
            Injection::Fail(e) => Err(e),
            // A filesystem that reports a successful sync without making anything durable.
            Injection::Swallow | Injection::Tear(_) => Ok(()),
        }
    }
}
