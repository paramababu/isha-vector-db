//! The storage conformance suite.
//!
//! Every [`Storage`] implementation must pass this, including ones written by third parties.
//! It exists because an abstraction is only real if something checks that implementations obey
//! it — otherwise each backend quietly develops its own dialect and the engine ends up with
//! per-backend special cases, which is exactly the coupling the abstraction was meant to prevent.
//!
//! Two kinds of check:
//!
//! - **Semantics.** Positional reads and writes, append offsets, truncation, directory handling,
//!   error classification. These are the same for every backend.
//! - **Declared capabilities.** If a backend claims
//!   [`atomic_rename`](vdb_core::storage::StorageCapabilities::atomic_rename), the
//!   suite exercises rename and requires it to work; if it does *not* claim it, the suite
//!   requires rename to fail with `Unsupported` rather than silently doing something non-atomic.
//!   Claiming a capability you do not have is the failure mode that loses data in the field.

use vdb_core::error::{DbError, StorageError};
use vdb_core::path::DbPath;
use vdb_core::storage::{EntryKind, OpenMode, Storage};

/// One named check. `Err(reason)` is a conformance failure, not a test-harness error.
type Check = (&'static str, fn(&dyn Storage) -> Result<(), String>);

/// What the suite found.
#[derive(Debug, Default, Clone)]
pub struct ConformanceReport {
    /// Names of checks that passed.
    pub passed: Vec<String>,
    /// Failures, as `(check name, what went wrong)`.
    pub failures: Vec<(String, String)>,
}

impl ConformanceReport {
    /// Whether every check passed.
    pub fn is_ok(&self) -> bool {
        self.failures.is_empty()
    }

    /// Panic with every failure listed, for use as the body of a `#[test]`.
    ///
    /// Reports all failures at once rather than stopping at the first, because when a new
    /// backend is being written the whole list is more useful than one line of it.
    ///
    /// # Panics
    /// If any check failed.
    pub fn assert_ok(&self) {
        if self.is_ok() {
            return;
        }
        let mut msg = format!(
            "{} of {} storage conformance checks failed:\n",
            self.failures.len(),
            self.failures.len() + self.passed.len()
        );
        for (name, detail) in &self.failures {
            msg.push_str(&format!("  - {name}: {detail}\n"));
        }
        panic!("{msg}");
    }
}

/// Run the suite against a backend.
///
/// `factory` is called once per check so each starts from a clean filesystem; a backend that
/// leaks state between checks would otherwise pass or fail depending on ordering.
pub fn storage_conformance(factory: &dyn Fn() -> Box<dyn Storage>) -> ConformanceReport {
    let mut report = ConformanceReport::default();
    let checks: &[Check] = &[
        ("root directory exists", check_root_exists),
        (
            "create_dir_all is recursive and idempotent",
            check_create_dir_all,
        ),
        (
            "open Read on a missing file reports NotFound",
            check_open_missing,
        ),
        (
            "open CreateNew twice reports AlreadyExists",
            check_create_new_twice,
        ),
        (
            "write_at then read_at round-trips",
            check_write_read_roundtrip,
        ),
        (
            "write_at past the end zero-fills the gap",
            check_sparse_write,
        ),
        (
            "append returns the offset it wrote at",
            check_append_offsets,
        ),
        (
            "read_at past the end returns zero bytes",
            check_read_past_end,
        ),
        ("read_at returns a short count at the end", check_short_read),
        ("truncate shortens and zero-extends", check_truncate),
        ("read-only handles refuse writes", check_read_only_handle),
        ("two handles see the same file", check_shared_handles),
        ("metadata reports length and kind", check_metadata),
        (
            "metadata on a missing path is Ok(None)",
            check_metadata_missing,
        ),
        (
            "remove_file deletes, and twice is NotFound",
            check_remove_file,
        ),
        ("list_dir lists only immediate children", check_list_dir),
        (
            "list_dir on a missing directory is NotFound",
            check_list_dir_missing,
        ),
        ("remove_dir_all is recursive", check_remove_dir_all),
        (
            "files cannot be created in a missing directory",
            check_missing_parent,
        ),
        (
            "a directory cannot be opened as a file",
            check_open_directory,
        ),
        ("sync_data succeeds and preserves content", check_sync),
        ("locks are exclusive and released on drop", check_locking),
        (
            "rename behaves as the backend declares",
            check_rename_matches_capability,
        ),
        (
            "mmap behaves as the backend declares",
            check_mmap_matches_capability,
        ),
        (
            "read_exact_at reports truncation as corruption",
            check_read_exact_at,
        ),
    ];

    for (name, check) in checks {
        let storage = factory();
        match check(storage.as_ref()) {
            Ok(()) => report.passed.push((*name).to_owned()),
            Err(detail) => report.failures.push(((*name).to_owned(), detail)),
        }
    }
    report
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn p(s: &str) -> DbPath {
    DbPath::parse(s).unwrap_or_else(|_| DbPath::root())
}

fn err(context: &str, e: impl core::fmt::Display) -> String {
    format!("{context}: {e}")
}

fn expect_storage_err<T>(
    result: Result<T, DbError>,
    want: fn(&StorageError) -> bool,
    what: &str,
) -> Result<(), String> {
    match result {
        Ok(_) => Err(format!("expected {what}, got success")),
        Err(DbError::Storage(e)) if want(&e) => Ok(()),
        Err(other) => Err(format!("expected {what}, got {other}")),
    }
}

/// A file with `contents`, in a directory that already exists.
fn seed_file(s: &dyn Storage, path: &DbPath, contents: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        s.create_dir_all(&parent)
            .map_err(|e| err("create_dir_all", e))?;
    }
    let mut f = s
        .open_file(path, OpenMode::Create)
        .map_err(|e| err("open Create", e))?;
    f.write_at(contents, 0).map_err(|e| err("write_at", e))?;
    f.sync_data().map_err(|e| err("sync_data", e))?;
    Ok(())
}

fn read_all(s: &dyn Storage, path: &DbPath) -> Result<Vec<u8>, String> {
    let f = s
        .open_file(path, OpenMode::Read)
        .map_err(|e| err("open Read", e))?;
    let len = f.len().map_err(|e| err("len", e))? as usize;
    let mut buf = vec![0u8; len];
    let n = f.read_at(&mut buf, 0).map_err(|e| err("read_at", e))?;
    buf.truncate(n);
    Ok(buf)
}

// ---------------------------------------------------------------------------
// checks
// ---------------------------------------------------------------------------

fn check_root_exists(s: &dyn Storage) -> Result<(), String> {
    let meta = s
        .metadata(&DbPath::root())
        .map_err(|e| err("metadata(root)", e))?;
    match meta {
        Some(m) if m.kind == EntryKind::Directory => Ok(()),
        Some(m) => Err(format!("root reported as {:?}", m.kind)),
        None => Err("root directory does not exist".into()),
    }
}

fn check_create_dir_all(s: &dyn Storage) -> Result<(), String> {
    let deep = p("a/b/c");
    s.create_dir_all(&deep).map_err(|e| err("first call", e))?;
    s.create_dir_all(&deep)
        .map_err(|e| err("second call should be idempotent", e))?;
    for dir in ["a", "a/b", "a/b/c"] {
        match s.metadata(&p(dir)).map_err(|e| err("metadata", e))? {
            Some(m) if m.kind == EntryKind::Directory => {}
            other => return Err(format!("{dir} is {other:?}, expected a directory")),
        }
    }
    Ok(())
}

fn check_open_missing(s: &dyn Storage) -> Result<(), String> {
    expect_storage_err(
        s.open_file(&p("nope"), OpenMode::Read),
        |e| matches!(e, StorageError::NotFound { .. }),
        "StorageError::NotFound",
    )?;
    expect_storage_err(
        s.open_file(&p("nope"), OpenMode::ReadWrite),
        |e| matches!(e, StorageError::NotFound { .. }),
        "StorageError::NotFound for ReadWrite",
    )
}

fn check_create_new_twice(s: &dyn Storage) -> Result<(), String> {
    let path = p("once");
    s.open_file(&path, OpenMode::CreateNew)
        .map_err(|e| err("first CreateNew", e))?;
    expect_storage_err(
        s.open_file(&path, OpenMode::CreateNew),
        |e| matches!(e, StorageError::AlreadyExists { .. }),
        "StorageError::AlreadyExists",
    )
}

fn check_write_read_roundtrip(s: &dyn Storage) -> Result<(), String> {
    let path = p("data/file.bin");
    let payload: Vec<u8> = (0..=255u8).collect();
    seed_file(s, &path, &payload)?;
    let got = read_all(s, &path)?;
    if got != payload {
        return Err(format!(
            "read back {} bytes, expected {}",
            got.len(),
            payload.len()
        ));
    }
    // Positional read from the middle.
    let f = s
        .open_file(&path, OpenMode::Read)
        .map_err(|e| err("reopen", e))?;
    let mut buf = [0u8; 4];
    f.read_at(&mut buf, 10).map_err(|e| err("read_at(10)", e))?;
    if buf != [10, 11, 12, 13] {
        return Err(format!(
            "read_at(offset 10) gave {buf:?}, expected [10, 11, 12, 13]"
        ));
    }
    Ok(())
}

fn check_sparse_write(s: &dyn Storage) -> Result<(), String> {
    let path = p("sparse");
    seed_file(s, &path, b"ab")?;
    let mut f = s
        .open_file(&path, OpenMode::ReadWrite)
        .map_err(|e| err("open", e))?;
    f.write_at(b"z", 9).map_err(|e| err("write_at(9)", e))?;
    let got = read_all(s, &path)?;
    let want = b"ab\0\0\0\0\0\0\0z";
    if got != want {
        return Err(format!("got {got:?}, expected {want:?}"));
    }
    Ok(())
}

fn check_append_offsets(s: &dyn Storage) -> Result<(), String> {
    let path = p("log");
    seed_file(s, &path, b"")?;
    let mut f = s
        .open_file(&path, OpenMode::ReadWrite)
        .map_err(|e| err("open", e))?;
    let first = f.append(b"hello").map_err(|e| err("append", e))?;
    let second = f.append(b" world").map_err(|e| err("append", e))?;
    if first != 0 || second != 5 {
        return Err(format!(
            "append offsets were {first} and {second}, expected 0 and 5"
        ));
    }
    let got = read_all(s, &path)?;
    if got != b"hello world" {
        return Err(format!("file contains {got:?}"));
    }
    Ok(())
}

fn check_read_past_end(s: &dyn Storage) -> Result<(), String> {
    let path = p("short");
    seed_file(s, &path, b"abc")?;
    let f = s
        .open_file(&path, OpenMode::Read)
        .map_err(|e| err("open", e))?;
    let mut buf = [0u8; 8];
    let n = f
        .read_at(&mut buf, 100)
        .map_err(|e| err("read_at(100)", e))?;
    if n != 0 {
        return Err(format!("read {n} bytes past the end, expected 0"));
    }
    let n = f.read_at(&mut buf, 3).map_err(|e| err("read_at(3)", e))?;
    if n != 0 {
        return Err(format!("read {n} bytes at exactly the end, expected 0"));
    }
    Ok(())
}

fn check_short_read(s: &dyn Storage) -> Result<(), String> {
    let path = p("short2");
    seed_file(s, &path, b"abcde")?;
    let f = s
        .open_file(&path, OpenMode::Read)
        .map_err(|e| err("open", e))?;
    let mut buf = [0u8; 100];
    let n = f.read_at(&mut buf, 2).map_err(|e| err("read_at", e))?;
    if n != 3 {
        return Err(format!(
            "read {n} bytes from offset 2 of a 5-byte file, expected 3"
        ));
    }
    if buf.get(..3) != Some(b"cde".as_slice()) {
        return Err(format!("got {:?}", buf.get(..3)));
    }
    Ok(())
}

fn check_truncate(s: &dyn Storage) -> Result<(), String> {
    let path = p("trunc");
    seed_file(s, &path, b"abcdefgh")?;
    let mut f = s
        .open_file(&path, OpenMode::ReadWrite)
        .map_err(|e| err("open", e))?;
    f.truncate(3).map_err(|e| err("truncate(3)", e))?;
    if f.len().map_err(|e| err("len", e))? != 3 {
        return Err("length was not 3 after truncate".into());
    }
    f.truncate(6).map_err(|e| err("truncate(6)", e))?;
    let got = read_all(s, &path)?;
    if got != b"abc\0\0\0" {
        return Err(format!("zero-extension gave {got:?}"));
    }
    Ok(())
}

fn check_read_only_handle(s: &dyn Storage) -> Result<(), String> {
    let path = p("ro");
    seed_file(s, &path, b"immutable")?;
    let mut f = s
        .open_file(&path, OpenMode::Read)
        .map_err(|e| err("open Read", e))?;
    if f.write_at(b"x", 0).is_ok() {
        return Err("a Read handle accepted a write".into());
    }
    if f.append(b"x").is_ok() {
        return Err("a Read handle accepted an append".into());
    }
    if f.truncate(0).is_ok() {
        return Err("a Read handle accepted a truncate".into());
    }
    if read_all(s, &path)? != b"immutable" {
        return Err("the file changed despite the writes being rejected".into());
    }
    Ok(())
}

fn check_shared_handles(s: &dyn Storage) -> Result<(), String> {
    let path = p("shared");
    seed_file(s, &path, b"one")?;
    let mut writer = s
        .open_file(&path, OpenMode::ReadWrite)
        .map_err(|e| err("open w", e))?;
    let reader = s
        .open_file(&path, OpenMode::Read)
        .map_err(|e| err("open r", e))?;
    writer.write_at(b"two", 0).map_err(|e| err("write", e))?;
    let mut buf = [0u8; 3];
    reader.read_at(&mut buf, 0).map_err(|e| err("read", e))?;
    if &buf != b"two" {
        return Err(format!("second handle saw {buf:?}, expected b\"two\""));
    }
    Ok(())
}

fn check_metadata(s: &dyn Storage) -> Result<(), String> {
    let path = p("dir/meta");
    seed_file(s, &path, b"12345")?;
    match s.metadata(&path).map_err(|e| err("metadata", e))? {
        Some(m) if m.kind == EntryKind::File && m.len == 5 => {}
        other => {
            return Err(format!(
                "file metadata was {other:?}, expected File with len 5"
            ))
        }
    }
    match s.metadata(&p("dir")).map_err(|e| err("metadata(dir)", e))? {
        Some(m) if m.kind == EntryKind::Directory => Ok(()),
        other => Err(format!("directory metadata was {other:?}")),
    }
}

fn check_metadata_missing(s: &dyn Storage) -> Result<(), String> {
    match s.metadata(&p("absent")) {
        Ok(None) => {}
        Ok(Some(m)) => return Err(format!("a missing path reported {m:?}")),
        Err(e) => return Err(format!("a missing path was an error, not Ok(None): {e}")),
    }
    match s.exists(&p("absent")) {
        Ok(false) => Ok(()),
        other => Err(format!("exists() on a missing path gave {other:?}")),
    }
}

fn check_remove_file(s: &dyn Storage) -> Result<(), String> {
    let path = p("doomed");
    seed_file(s, &path, b"x")?;
    s.remove_file(&path).map_err(|e| err("remove_file", e))?;
    if s.exists(&path).map_err(|e| err("exists", e))? {
        return Err("file still exists after removal".into());
    }
    expect_storage_err(
        s.remove_file(&path),
        |e| matches!(e, StorageError::NotFound { .. }),
        "StorageError::NotFound on a second removal",
    )
}

fn check_list_dir(s: &dyn Storage) -> Result<(), String> {
    s.create_dir_all(&p("top/sub"))
        .map_err(|e| err("create_dir_all", e))?;
    seed_file(s, &p("top/a.txt"), b"a")?;
    seed_file(s, &p("top/b.txt"), b"b")?;
    seed_file(s, &p("top/sub/deep.txt"), b"d")?;

    let mut entries = s.list_dir(&p("top")).map_err(|e| err("list_dir", e))?;
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
    if names != ["a.txt", "b.txt", "sub"] {
        return Err(format!(
            "listed {names:?}, expected [\"a.txt\", \"b.txt\", \"sub\"]"
        ));
    }
    let kinds: Vec<EntryKind> = entries.iter().map(|e| e.kind).collect();
    if kinds != [EntryKind::File, EntryKind::File, EntryKind::Directory] {
        return Err(format!("kinds were {kinds:?}"));
    }
    Ok(())
}

fn check_list_dir_missing(s: &dyn Storage) -> Result<(), String> {
    expect_storage_err(
        s.list_dir(&p("nowhere")),
        |e| matches!(e, StorageError::NotFound { .. }),
        "StorageError::NotFound",
    )
}

fn check_remove_dir_all(s: &dyn Storage) -> Result<(), String> {
    seed_file(s, &p("tree/x/y/leaf"), b"leaf")?;
    seed_file(s, &p("keep/other"), b"other")?;
    s.remove_dir_all(&p("tree"))
        .map_err(|e| err("remove_dir_all", e))?;
    for gone in ["tree", "tree/x", "tree/x/y", "tree/x/y/leaf"] {
        if s.exists(&p(gone)).map_err(|e| err("exists", e))? {
            return Err(format!("{gone} survived remove_dir_all"));
        }
    }
    if !s.exists(&p("keep/other")).map_err(|e| err("exists", e))? {
        return Err("remove_dir_all deleted an unrelated tree".into());
    }
    Ok(())
}

fn check_missing_parent(s: &dyn Storage) -> Result<(), String> {
    // A real filesystem refuses this. A backend that allows it hides path-construction bugs.
    match s.open_file(&p("no/such/dir/file"), OpenMode::Create) {
        Ok(_) => Err("created a file inside a directory that does not exist".into()),
        Err(DbError::Storage(StorageError::NotFound { .. })) => Ok(()),
        Err(e) => Err(format!("expected NotFound, got {e}")),
    }
}

fn check_open_directory(s: &dyn Storage) -> Result<(), String> {
    s.create_dir_all(&p("adir"))
        .map_err(|e| err("create_dir_all", e))?;
    if s.open_file(&p("adir"), OpenMode::Read).is_ok() {
        return Err("a directory was opened as a file".into());
    }
    Ok(())
}

fn check_sync(s: &dyn Storage) -> Result<(), String> {
    let path = p("synced");
    seed_file(s, &path, b"durable")?;
    let mut f = s
        .open_file(&path, OpenMode::ReadWrite)
        .map_err(|e| err("open", e))?;
    f.append(b"!").map_err(|e| err("append", e))?;
    f.sync_data().map_err(|e| err("sync_data", e))?;
    if read_all(s, &path)? != b"durable!" {
        return Err("content changed across a sync".into());
    }
    Ok(())
}

fn check_locking(s: &dyn Storage) -> Result<(), String> {
    if !s.capabilities().file_locking {
        return match s.try_lock(&p("LOCK")) {
            Err(DbError::Storage(StorageError::Unsupported { .. })) => Ok(()),
            Err(e) => Err(format!(
                "backend declares no locking; expected Unsupported, got {e}"
            )),
            Ok(_) => Err("backend declares no locking but try_lock succeeded".into()),
        };
    }
    let lock = s
        .try_lock(&p("LOCK"))
        .map_err(|e| err("first try_lock", e))?;
    match s.try_lock(&p("LOCK")) {
        Err(DbError::Storage(StorageError::LockUnavailable { .. })) => {}
        Err(e) => {
            return Err(format!(
                "second try_lock gave {e}, expected LockUnavailable"
            ))
        }
        Ok(_) => return Err("the lock was granted twice".into()),
    }
    drop(lock);
    s.try_lock(&p("LOCK"))
        .map_err(|e| err("try_lock after release", e))?;
    Ok(())
}

fn check_rename_matches_capability(s: &dyn Storage) -> Result<(), String> {
    let from = p("from.bin");
    let to = p("to.bin");
    seed_file(s, &from, b"payload")?;

    if !s.capabilities().atomic_rename {
        // A backend without atomic rename must say so rather than emulate it: the engine picks
        // its commit protocol from this answer.
        return match s.rename(&from, &to) {
            Err(DbError::Storage(StorageError::Unsupported { .. })) => Ok(()),
            Err(e) => Err(format!("expected Unsupported, got {e}")),
            Ok(()) => Err("backend does not declare atomic_rename but rename succeeded".into()),
        };
    }

    s.rename(&from, &to).map_err(|e| err("rename", e))?;
    if s.exists(&from).map_err(|e| err("exists(from)", e))? {
        return Err("source still exists after rename".into());
    }
    if read_all(s, &to)? != b"payload" {
        return Err("content did not survive the rename".into());
    }
    // Renaming over an existing destination must replace it — that is how a manifest is swapped.
    seed_file(s, &from, b"newer")?;
    s.rename(&from, &to)
        .map_err(|e| err("rename over existing", e))?;
    if read_all(s, &to)? != b"newer" {
        return Err("rename did not replace the destination".into());
    }
    expect_storage_err(
        s.rename(&p("ghost"), &to),
        |e| matches!(e, StorageError::NotFound { .. }),
        "StorageError::NotFound renaming a missing file",
    )
}

fn check_mmap_matches_capability(s: &dyn Storage) -> Result<(), String> {
    let path = p("mapped");
    let payload: Vec<u8> = (0..100u8).collect();
    seed_file(s, &path, &payload)?;
    let f = s
        .open_file(&path, OpenMode::Read)
        .map_err(|e| err("open", e))?;
    let mapped = f.map_readonly().map_err(|e| err("map_readonly", e))?;
    match (s.capabilities().mmap, mapped) {
        (true, Some(region)) => {
            if region.as_slice() != payload.as_slice() {
                return Err("the mapping did not match the file".into());
            }
            Ok(())
        }
        (true, None) => Err("backend declares mmap but map_readonly returned None".into()),
        (false, None) => Ok(()),
        (false, Some(_)) => {
            Err("backend does not declare mmap but map_readonly returned a mapping".into())
        }
    }
}

fn check_read_exact_at(s: &dyn Storage) -> Result<(), String> {
    let path = p("exact");
    seed_file(s, &path, b"1234")?;
    let f = s
        .open_file(&path, OpenMode::Read)
        .map_err(|e| err("open", e))?;
    let mut ok = [0u8; 4];
    f.read_exact_at(&mut ok, 0, &path)
        .map_err(|e| err("read_exact_at within bounds", e))?;

    let mut too_much = [0u8; 8];
    match f.read_exact_at(&mut too_much, 0, &path) {
        Err(e) if e.is_corruption() => Ok(()),
        Err(e) => Err(format!(
            "expected a CorruptionError for a short read, got {e}"
        )),
        Ok(()) => Err("read_exact_at succeeded past the end of the file".into()),
    }
}
