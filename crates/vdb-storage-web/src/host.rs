//! The host interface: everything this crate needs the embedder to provide.
//!
//! # Why a hand-written import table
//!
//! [ADR-0009](../../../docs/adr/README.md) commits to one hand-written C ABI as the single
//! interop contract, on the grounds that a generated binding layer is a dependency that can
//! break in ways nobody on the project understands. The same reasoning applies here, so this
//! crate declares its imports by hand and the JavaScript side implements them by hand. There is
//! no `wasm-bindgen`, and therefore no build step beyond `cargo build`.
//!
//! # Numbers
//!
//! Offsets and sizes cross the boundary as `f64`, not `i64`. WebAssembly can pass `i64`, but it
//! surfaces in JavaScript as `BigInt`, and every read and write on the hot path would allocate
//! one. `f64` represents every integer up to 2^53 exactly, which this crate declares as its
//! maximum file size — around 9 petabytes, against an OPFS quota measured in gigabytes.
//!
//! # Errors
//!
//! A function that can fail returns a non-negative value on success and one of the negative
//! [`HostError`] codes on failure. The host never throws across the boundary: a JavaScript
//! exception unwinding into WebAssembly aborts the module, taking the database with it, so the
//! glue catches everything and converts it to a code.

use vdb_core::error::{DbError, StorageError, StorageOp};
use vdb_core::path::DbPath;

/// Error codes the host returns. Negative so they cannot be confused with a handle or a count.
pub mod code {
    /// The path does not exist.
    pub const NOT_FOUND: i32 = -1;
    /// The path exists and the mode required that it not.
    pub const ALREADY_EXISTS: i32 = -2;
    /// The host refused the operation.
    pub const PERMISSION_DENIED: i32 = -3;
    /// Anything else the host could not complete.
    pub const IO: i32 = -4;
    /// The host rejected the path itself.
    pub const INVALID_PATH: i32 = -5;
    /// Another holder has the lock.
    pub const LOCKED: i32 = -6;
    /// The storage quota is exhausted.
    pub const QUOTA_EXCEEDED: i32 = -7;
    /// The handle is not one the host issued, or has been closed.
    pub const BAD_HANDLE: i32 = -8;
    /// The destination buffer was too small; the return value says how much is needed.
    pub const BUFFER_TOO_SMALL: i32 = -9;
}

/// Open modes, matching [`vdb_core::storage::OpenMode`] one for one.
pub mod mode {
    /// Read only; must exist.
    pub const READ: u32 = 0;
    /// Read and write; must exist.
    pub const READ_WRITE: u32 = 1;
    /// Read and write, creating if absent.
    pub const CREATE: u32 = 2;
    /// Create; must not already exist.
    pub const CREATE_NEW: u32 = 3;
}

/// Turn a host error code into the engine's own error, naming the path and operation.
///
/// An unrecognised code becomes an I/O error rather than a panic: the host is the least
/// trustworthy part of this system and must not be able to bring the engine down by returning
/// a number nobody expected.
pub(crate) fn to_error(code: i32, path: &DbPath, op: StorageOp) -> DbError {
    match code {
        self::code::NOT_FOUND => StorageError::NotFound { path: path.clone() }.into(),
        self::code::ALREADY_EXISTS => StorageError::AlreadyExists { path: path.clone() }.into(),
        self::code::PERMISSION_DENIED => StorageError::PermissionDenied {
            path: path.clone(),
            operation: op,
        }
        .into(),
        self::code::QUOTA_EXCEEDED => StorageError::InsufficientStorage {
            required: 0,
            available: None,
        }
        .into(),
        other => StorageError::Io {
            path: path.clone(),
            operation: op,
            detail: describe(other).to_owned(),
        }
        .into(),
    }
}

/// A stable description for each code.
fn describe(code: i32) -> &'static str {
    match code {
        self::code::IO => "the host reported an I/O failure",
        self::code::INVALID_PATH => "the host rejected the path",
        self::code::LOCKED => "another holder has the lock",
        self::code::BAD_HANDLE => "the host does not recognise the file handle",
        self::code::BUFFER_TOO_SMALL => "the destination buffer was too small",
        _ => "the host returned an unrecognised error code",
    }
}

// The imports themselves. Every pointer is an offset into this module's linear memory, and
// every length is a byte count. The host must not retain a pointer after the call returns:
// linear memory can be reallocated and moved by any later allocation.
// On wasm these are imports the embedder supplies, gathered under one module name so the JS
// import object has a single namespace. On other targets the attribute is meaningless and the
// symbols are resolved at link time — by `test_host`, which is how the conformance suite runs.
#[cfg_attr(target_arch = "wasm32", link(wasm_import_module = "vdb_host"))]
#[allow(clippy::missing_safety_doc)]
extern "C" {
    /// Open `path`, returning a non-negative handle or an error code.
    pub(crate) fn vdb_host_open(path: *const u8, path_len: usize, mode: u32) -> i32;
    /// Read into `buf`, returning the number of bytes read — which may be short at end of file.
    pub(crate) fn vdb_host_read(handle: i32, buf: *mut u8, len: usize, offset: f64) -> i32;
    /// Write all of `buf` at `offset`.
    pub(crate) fn vdb_host_write(handle: i32, buf: *const u8, len: usize, offset: f64) -> i32;
    /// Set the file's length, growing with zeroes or discarding the tail.
    pub(crate) fn vdb_host_truncate(handle: i32, len: f64) -> i32;
    /// The file's current length, or a negative error code.
    pub(crate) fn vdb_host_size(handle: i32) -> f64;
    /// Flush this file's contents as far as the host can.
    pub(crate) fn vdb_host_sync(handle: i32) -> i32;
    /// Release a handle. Further use of it is an error, not undefined behaviour.
    pub(crate) fn vdb_host_close(handle: i32) -> i32;

    /// Delete a file.
    pub(crate) fn vdb_host_remove_file(path: *const u8, path_len: usize) -> i32;
    /// Create a directory and every missing parent.
    pub(crate) fn vdb_host_create_dir_all(path: *const u8, path_len: usize) -> i32;
    /// Remove a directory and everything under it.
    pub(crate) fn vdb_host_remove_dir_all(path: *const u8, path_len: usize) -> i32;
    /// Flush a directory's own entries, if the host has such a concept.
    pub(crate) fn vdb_host_sync_dir(path: *const u8, path_len: usize) -> i32;

    /// Stat a path. Writes the length to `out_len` and 0 (file) or 1 (directory) to `out_kind`.
    /// Returns 0 when the path exists, `NOT_FOUND` when it does not.
    pub(crate) fn vdb_host_metadata(
        path: *const u8,
        path_len: usize,
        out_len: *mut f64,
        out_kind: *mut u32,
    ) -> i32;

    /// List a directory into `buf` as `kind_byte name "\n"` records, where `kind_byte` is `f`
    /// or `d`. Returns the number of bytes written, or `BUFFER_TOO_SMALL` — in which case the
    /// caller retries with at least `vdb_host_list_dir_size` bytes.
    pub(crate) fn vdb_host_list_dir(
        path: *const u8,
        path_len: usize,
        buf: *mut u8,
        cap: usize,
    ) -> i32;

    /// Take an advisory lock, returning 0, `LOCKED`, or another error code.
    pub(crate) fn vdb_host_lock(path: *const u8, path_len: usize) -> i32;
    /// Release a lock taken by [`vdb_host_lock`].
    pub(crate) fn vdb_host_unlock(path: *const u8, path_len: usize) -> i32;
}
