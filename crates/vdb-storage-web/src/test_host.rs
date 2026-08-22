//! A Rust implementation of the host imports, backed by an in-memory filesystem.
//!
//! # Why this exists
//!
//! The real host is JavaScript driving OPFS, which needs a browser. Without something standing
//! in for it, everything in this crate — path resolution, error-code translation, the listing
//! format, the short-read contract, the buffer-growth loop — would be untested until it ran in
//! a browser, which is the worst place to find out that a directory listing splits on the wrong
//! byte.
//!
//! With these symbols linked in, `vdb-testkit`'s storage conformance suite runs against
//! [`WebStorage`](crate::WebStorage) natively, and the browser is left to prove only the one
//! thing it uniquely can: that OPFS behaves as this interface says it must.
//!
//! This is a test double, not a second storage backend. It is behind a feature flag and is
//! never compiled into a shipped artefact.

// Every function here is `unsafe extern "C"` because it implements the import table declared in
// `host.rs`. The safety contract is stated once there — pointers describe live, initialised
// slices for the duration of the call and are not retained — and repeating it fifteen times
// would add words without adding meaning.
#![allow(clippy::missing_safety_doc)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::host::code;

/// A file or directory in the fake filesystem.
#[derive(Debug, Clone)]
enum Node {
    File(Vec<u8>),
    Dir,
}

#[derive(Debug, Default)]
struct Fs {
    /// Absolute path to node. A `BTreeMap` so listings come out in a deterministic order, which
    /// matters because a test that depends on host iteration order would be flaky in a browser.
    nodes: BTreeMap<String, Node>,
    /// Open handles: handle to the path it refers to.
    open: BTreeMap<i32, (String, bool)>,
    /// Paths currently locked.
    locks: BTreeMap<String, ()>,
}

fn fs() -> &'static Mutex<Fs> {
    static FS: OnceLock<Mutex<Fs>> = OnceLock::new();
    FS.get_or_init(|| Mutex::new(Fs::default()))
}

static NEXT_HANDLE: AtomicI32 = AtomicI32::new(1);

/// A distinct root per caller, so tests that share the process do not share a namespace.
///
/// There is deliberately no `reset()`. The filesystem is a process-wide static and `cargo test`
/// runs a crate's tests concurrently, so anything that emptied it would be a landmine: one test
/// could wipe another's database mid-run, and the resulting failure would look like a bug in the
/// engine. Isolation comes from every caller getting its own root instead.
pub fn unique_root(label: &str) -> String {
    static N: AtomicI32 = AtomicI32::new(0);
    format!("/{label}-{}", N.fetch_add(1, Ordering::Relaxed))
}

/// Read a string the caller passed as pointer and length.
///
/// # Safety
/// `ptr` must point to `len` initialised bytes.
unsafe fn str_arg<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    core::str::from_utf8(bytes).ok()
}

/// Every ancestor directory of `path`, nearest last.
fn parent_of(path: &str) -> Option<&str> {
    path.rfind('/').map(|i| &path[..i])
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_open(path: *const u8, path_len: usize, mode: u32) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());

    let exists = matches!(fs.nodes.get(path), Some(Node::File(_)));
    if matches!(fs.nodes.get(path), Some(Node::Dir)) {
        // Opening a directory as a file must fail. A real OPFS `getFileHandle` on a directory
        // rejects; `vdb-storage-os` had a bug here once and the conformance suite caught it.
        return code::PERMISSION_DENIED;
    }
    match mode {
        crate::host::mode::READ | crate::host::mode::READ_WRITE if !exists => {
            return code::NOT_FOUND
        }
        crate::host::mode::CREATE_NEW if exists => return code::ALREADY_EXISTS,
        _ => {}
    }
    if !exists {
        // The parent must exist, as it must in OPFS.
        if let Some(parent) = parent_of(path) {
            if !parent.is_empty() && !matches!(fs.nodes.get(parent), Some(Node::Dir)) {
                return code::NOT_FOUND;
            }
        }
        fs.nodes.insert(path.to_owned(), Node::File(Vec::new()));
    }
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    let writable = mode != crate::host::mode::READ;
    fs.open.insert(handle, (path.to_owned(), writable));
    handle
}

/// Resolve a handle to its path, or return a code.
fn path_for(fs: &Fs, handle: i32) -> Result<String, i32> {
    fs.open
        .get(&handle)
        .map(|(p, _)| p.clone())
        .ok_or(code::BAD_HANDLE)
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_read(handle: i32, buf: *mut u8, len: usize, offset: f64) -> i32 {
    let fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    let path = match path_for(&fs, handle) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(Node::File(data)) = fs.nodes.get(&path) else {
        return code::NOT_FOUND;
    };
    let start = offset as usize;
    if start >= data.len() {
        return 0;
    }
    let n = len.min(data.len() - start);
    unsafe { core::ptr::copy_nonoverlapping(data.as_ptr().add(start), buf, n) };
    n as i32
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_write(
    handle: i32,
    buf: *const u8,
    len: usize,
    offset: f64,
) -> i32 {
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    let path = match path_for(&fs, handle) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if !fs.open.get(&handle).map(|(_, w)| *w).unwrap_or(false) {
        return code::PERMISSION_DENIED;
    }
    let Some(Node::File(data)) = fs.nodes.get_mut(&path) else {
        return code::NOT_FOUND;
    };
    let start = offset as usize;
    // Writing past the end zero-fills, which is what both POSIX and OPFS do.
    if data.len() < start + len {
        data.resize(start + len, 0);
    }
    let src = unsafe { core::slice::from_raw_parts(buf, len) };
    let Some(dest) = data.get_mut(start..start + len) else {
        return code::IO;
    };
    dest.copy_from_slice(src);
    0
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_truncate(handle: i32, len: f64) -> i32 {
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    let path = match path_for(&fs, handle) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(Node::File(data)) = fs.nodes.get_mut(&path) else {
        return code::NOT_FOUND;
    };
    data.resize(len as usize, 0);
    0
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_size(handle: i32) -> f64 {
    let fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    let path = match path_for(&fs, handle) {
        Ok(p) => p,
        Err(e) => return f64::from(e),
    };
    match fs.nodes.get(&path) {
        Some(Node::File(data)) => data.len() as f64,
        _ => f64::from(code::NOT_FOUND),
    }
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_sync(handle: i32) -> i32 {
    let fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    match path_for(&fs, handle) {
        Ok(_) => 0,
        Err(e) => e,
    }
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_close(handle: i32) -> i32 {
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    if fs.open.remove(&handle).is_none() {
        return code::BAD_HANDLE;
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_remove_file(path: *const u8, path_len: usize) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    match fs.nodes.get(path) {
        Some(Node::File(_)) => {
            fs.nodes.remove(path);
            0
        }
        Some(Node::Dir) => code::PERMISSION_DENIED,
        None => code::NOT_FOUND,
    }
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_create_dir_all(path: *const u8, path_len: usize) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    if matches!(fs.nodes.get(path), Some(Node::File(_))) {
        return code::ALREADY_EXISTS;
    }
    // Create every ancestor, as `mkdir -p` does.
    let mut acc = String::new();
    for part in path.split('/') {
        if part.is_empty() {
            continue;
        }
        acc.push('/');
        acc.push_str(part);
        if matches!(fs.nodes.get(&acc), Some(Node::File(_))) {
            return code::ALREADY_EXISTS;
        }
        fs.nodes.entry(acc.clone()).or_insert(Node::Dir);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_remove_dir_all(path: *const u8, path_len: usize) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    if !fs.nodes.contains_key(path) {
        return code::NOT_FOUND;
    }
    let prefix = format!("{path}/");
    fs.nodes.retain(|k, _| k != path && !k.starts_with(&prefix));
    0
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_sync_dir(path: *const u8, path_len: usize) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    match fs.nodes.get(path) {
        Some(Node::Dir) => 0,
        Some(Node::File(_)) => code::PERMISSION_DENIED,
        None => code::NOT_FOUND,
    }
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_metadata(
    path: *const u8,
    path_len: usize,
    out_len: *mut f64,
    out_kind: *mut u32,
) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    match fs.nodes.get(path) {
        Some(Node::File(data)) => {
            unsafe {
                *out_len = data.len() as f64;
                *out_kind = 0;
            }
            0
        }
        Some(Node::Dir) => {
            unsafe {
                *out_len = 0.0;
                *out_kind = 1;
            }
            0
        }
        None => code::NOT_FOUND,
    }
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_list_dir(
    path: *const u8,
    path_len: usize,
    buf: *mut u8,
    cap: usize,
) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    if !matches!(fs.nodes.get(path), Some(Node::Dir)) {
        return code::NOT_FOUND;
    }
    let prefix = format!("{path}/");
    let mut out = Vec::new();
    for (k, node) in &fs.nodes {
        let Some(rest) = k.strip_prefix(&prefix) else {
            continue;
        };
        // Immediate children only.
        if rest.contains('/') {
            continue;
        }
        out.push(match node {
            Node::File(_) => b'f',
            Node::Dir => b'd',
        });
        out.extend_from_slice(rest.as_bytes());
        out.push(b'\n');
    }
    if out.len() > cap {
        return code::BUFFER_TOO_SMALL;
    }
    unsafe { core::ptr::copy_nonoverlapping(out.as_ptr(), buf, out.len()) };
    out.len() as i32
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_lock(path: *const u8, path_len: usize) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    if fs.locks.contains_key(path) {
        return code::LOCKED;
    }
    fs.locks.insert(path.to_owned(), ());
    0
}

#[no_mangle]
pub unsafe extern "C" fn vdb_host_unlock(path: *const u8, path_len: usize) -> i32 {
    let Some(path) = (unsafe { str_arg(path, path_len) }) else {
        return code::INVALID_PATH;
    };
    let mut fs = fs().lock().unwrap_or_else(|e| e.into_inner());
    fs.locks.remove(path);
    0
}
