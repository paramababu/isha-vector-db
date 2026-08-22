//! The single-writer advisory lock.
//!
//! # Why `flock` and not a lock file
//!
//! The obvious implementation is `create_new` on a `LOCK` file, deleted on close. It is also
//! wrong for this project: the file outlives a crash, so an application killed by the operating
//! system — routine on mobile — can never reopen its own database. Users would meet a permanent
//! "database already open" error with no way out but deleting a file they know nothing about.
//!
//! `flock` is released by the kernel when the process goes away, whether it exited cleanly or
//! was killed. That is the behaviour actually wanted, and it is why this crate takes a `libc`
//! dependency.
//!
//! # What it does and does not protect
//!
//! Advisory, and honestly so. It prevents accidents — two instances of an application, a debug
//! tool left open — and it is not a security boundary. Some network filesystems implement
//! `flock` as a no-op, so a database on an NFS mount may not be protected at all; that is a
//! property of the mount, and is documented rather than papered over.
//!
//! # Forking while the database is open
//!
//! An `flock` lock belongs to the *open file description*, and `fork` gives the child a copy of
//! every one of them. So a child forked while a database is open inherits the lock, and the
//! lock is not released until the parent has dropped it **and** every inherited copy is closed.
//!
//! Rust opens files `O_CLOEXEC`, so the descriptor closes as soon as the child `exec`s — which
//! covers the ordinary `spawn a program` case. What it does not cover is a child that forks and
//! never execs, or the brief window between the two. An application that closes its database
//! and immediately reopens it while a subprocess is starting can therefore see a spurious
//! "already open".
//!
//! This is a property of `flock` rather than something to work around: the alternatives — a lock
//! file, or `fcntl` locks, which are released by closing *any* descriptor for the file and so
//! break far more surprisingly — are worse. It is recorded here because SDK authors will meet
//! it: Node spawns child processes, and so does anything that shells out.

use std::fs::OpenOptions;
use std::path::Path;

use vdb_core::error::{Result, StorageError};
use vdb_core::path::DbPath;
use vdb_core::storage::FileLock;

/// Take the lock, or report who has it.
pub(crate) fn acquire(host: &Path, path: &DbPath) -> Result<Box<dyn FileLock>> {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(host)
            .map_err(|e| StorageError::Io {
                path: path.clone(),
                operation: vdb_core::error::StorageOp::Lock,
                detail: e.to_string(),
            })?;

        // SAFETY: `fd` is a valid descriptor owned by `file`, which outlives the call and the
        // returned guard. LOCK_NB makes this non-blocking, so a contended lock returns rather
        // than hanging the caller.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            return Err(StorageError::LockUnavailable { path: path.clone() }.into());
        }
        Ok(Box::new(OsLock {
            _file: file,
            holder: describe_holder(),
        }))
    }
    #[cfg(not(unix))]
    {
        // Without `flock` the only portable option is an exclusive-create marker, which does
        // *not* survive a crash gracefully. Declared honestly: `capabilities().file_locking` is
        // false off unix, and the conformance suite therefore requires this to say so rather
        // than pretend.
        let _ = (host, OpenOptions::new());
        Err(StorageError::Unsupported {
            operation: vdb_core::error::StorageOp::Lock,
            backend: "os",
        }
        .into())
    }
}

#[cfg(unix)]
fn describe_holder() -> String {
    // SAFETY: `getpid` takes no arguments and cannot fail.
    let pid = unsafe { libc::getpid() };
    format!("pid {pid}")
}

/// Released when dropped — and by the kernel if the process dies first.
#[cfg(unix)]
#[derive(Debug)]
struct OsLock {
    _file: std::fs::File,
    holder: String,
}

#[cfg(unix)]
impl FileLock for OsLock {
    fn holder(&self) -> &str {
        &self.holder
    }
}
