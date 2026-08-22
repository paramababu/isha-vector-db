//! `OsStorage` against the shared conformance suite, plus the behaviours only a real filesystem
//! can exhibit.
//!
//! This is the payoff for the storage abstraction. The suite is the same one `MemoryStorage`
//! passes, unchanged — if the two backends needed different tests, the abstraction would be
//! leaking and the engine would eventually grow per-backend special cases.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use vdb_core::path::DbPath;
use vdb_core::storage::{OpenMode, Storage};
use vdb_storage_os::OsStorage;
use vdb_testkit::storage_conformance;

/// A temporary directory that cleans up after itself.
///
/// Hand-rolled rather than pulling in `tempfile`: it is fifteen lines, and a dev-dependency in
/// a crate whose whole job is filesystem access is worth avoiding when the alternative is this
/// small.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("vdb-test-{label}-{pid}-{unique}"));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("create temp dir");
        Self(path)
    }

    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn p(s: &str) -> DbPath {
    DbPath::parse(s).unwrap()
}

#[test]
fn os_storage_is_conformant() {
    // A fresh directory per check, since the suite requires each to start clean.
    let holder = TempDir::new("conformance");
    let counter = AtomicU64::new(0);
    let report = storage_conformance(&|| {
        let n = counter.fetch_add(1, Ordering::SeqCst);
        let dir = holder.path().join(format!("case-{n}"));
        Box::new(OsStorage::open(dir).unwrap()) as Box<dyn Storage>
    });
    report.assert_ok();
    assert!(
        report.passed.len() >= 25,
        "suite shrank: {}",
        report.passed.len()
    );
}

#[test]
fn declared_capabilities_match_the_platform() {
    let dir = TempDir::new("caps");
    let s = OsStorage::open(dir.path()).unwrap();
    let caps = s.capabilities();
    assert!(caps.atomic_rename);
    assert!(caps.durable_sync);
    assert!(caps.sparse_files);
    assert_eq!(caps.file_locking, cfg!(unix));
    assert!(
        !caps.mmap,
        "mmap lands as its own change, measured against this baseline"
    );
}

/// The reason this backend uses `flock` rather than a lock file: a process that dies must not
/// leave its database permanently unopenable.
#[cfg(unix)]
#[test]
fn a_lock_is_released_when_its_guard_is_dropped() {
    let dir = TempDir::new("lock");
    let s = OsStorage::open(dir.path()).unwrap();

    let lock = s.try_lock(&p("LOCK")).unwrap();
    assert!(lock.holder().contains("pid"));
    assert!(
        s.try_lock(&p("LOCK")).is_err(),
        "the lock should be exclusive"
    );
    drop(lock);
    s.try_lock(&p("LOCK"))
        .expect("the lock should be available again");
}

/// A lock taken by a *different process* must be visible to this one — the case a lock file
/// gets right by accident and `flock` gets right on purpose.
#[cfg(unix)]
#[test]
fn a_lock_is_exclusive_across_processes() {
    use std::process::Command;

    let dir = TempDir::new("lock-cross");
    let s = OsStorage::open(dir.path()).unwrap();
    let lock_path = dir.path().join("LOCK");
    std::fs::write(&lock_path, b"").unwrap();

    // A short-lived child that takes the lock and holds it while we try.
    let script = format!(
        "exec 9<>{}; flock -n 9 || exit 3; sleep 1",
        shell_quote(&lock_path.to_string_lossy())
    );
    let mut child = match Command::new("sh").arg("-c").arg(&script).spawn() {
        Ok(c) => c,
        // `flock(1)` is not on macOS by default; skip rather than fail on a missing tool.
        Err(_) => return,
    };
    std::thread::sleep(std::time::Duration::from_millis(150));

    let ours = s.try_lock(&p("LOCK"));
    let status = child.wait().unwrap();
    if status.code() == Some(3) || status.code() == Some(127) {
        return; // the child could not take the lock, or flock(1) is absent: nothing proven
    }
    assert!(
        ours.is_err(),
        "a lock held by another process should be visible"
    );
}

#[cfg(unix)]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Data written and synced must be readable by a completely separate handle — the closest a
/// test can get to "it really reached the disk" without pulling the power.
#[test]
fn data_survives_reopening_the_backend() {
    let dir = TempDir::new("durable");
    {
        let s = OsStorage::open(dir.path()).unwrap();
        s.create_dir_all(&p("nested/deep")).unwrap();
        let mut f = s
            .open_file(&p("nested/deep/data.bin"), OpenMode::Create)
            .unwrap();
        f.write_at(&(0..=255u8).collect::<Vec<_>>(), 0).unwrap();
        f.sync_data().unwrap();
        s.sync_dir(&p("nested/deep")).unwrap();
    }
    let s = OsStorage::open(dir.path()).unwrap();
    let f = s
        .open_file(&p("nested/deep/data.bin"), OpenMode::Read)
        .unwrap();
    let mut buf = vec![0u8; 256];
    assert_eq!(f.read_at(&mut buf, 0).unwrap(), 256);
    assert_eq!(buf, (0..=255u8).collect::<Vec<_>>());
}

/// The engine builds paths from validated `DbPath`s, so traversal is already impossible. The
/// backend refuses it anyway — two independent checks, because the cost of being wrong is
/// writing outside the database directory.
#[test]
fn nothing_can_be_written_outside_the_database_directory() {
    let holder = TempDir::new("escape");
    let root = holder.path().join("db");
    let s = OsStorage::open(&root).unwrap();

    // `DbPath` cannot even represent these.
    assert!(DbPath::parse("../escaped").is_err());
    assert!(DbPath::root().join("..").is_err());

    s.create_dir_all(&p("inside")).unwrap();
    let mut f = s.open_file(&p("inside/file"), OpenMode::Create).unwrap();
    f.write_at(b"contained", 0).unwrap();
    f.sync_data().unwrap();

    assert!(root.join("inside/file").exists());
    assert!(!holder.path().join("escaped").exists());
}

#[test]
fn a_large_file_round_trips_at_arbitrary_offsets() {
    let dir = TempDir::new("large");
    let s = OsStorage::open(dir.path()).unwrap();
    let mut f = s.open_file(&p("big.bin"), OpenMode::Create).unwrap();

    let block: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    for i in 0..64u64 {
        f.write_at(&block, i * 4096).unwrap();
    }
    f.sync_data().unwrap();
    assert_eq!(f.len().unwrap(), 64 * 4096);

    let mut buf = vec![0u8; 4096];
    for i in [0u64, 1, 31, 63] {
        f.read_at(&mut buf, i * 4096).unwrap();
        assert_eq!(buf, block, "block {i}");
    }
    // A read spanning two blocks, at an offset that is not a multiple of anything.
    let mut span = vec![0u8; 100];
    f.read_at(&mut span, 4096 - 50).unwrap();
    assert_eq!(&span[..50], &block[4046..]);
    assert_eq!(&span[50..], &block[..50]);
}

#[test]
fn missing_files_and_directories_report_not_found() {
    let dir = TempDir::new("missing");
    let s = OsStorage::open(dir.path()).unwrap();
    assert!(s.metadata(&p("nope")).unwrap().is_none());
    assert!(!s.exists(&p("nope")).unwrap());
    assert!(s.open_file(&p("nope"), OpenMode::Read).is_err());
    assert!(s.list_dir(&p("nope")).is_err());
    assert!(s.remove_file(&p("nope")).is_err());
}
