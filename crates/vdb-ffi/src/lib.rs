//! The stable C ABI.
//!
//! One contract, consumed by React Native, Flutter, Android, iOS and anything else that can
//! call C. Node and the browser reach the engine differently — N-API and wasm-bindgen bind Rust
//! directly — but everything native comes through here.
//!
//! **This boundary is frozen at 0.2 and additive-only after that.** Once four SDKs depend on it,
//! a change here is four changes plus a compatibility matrix. [`vdb_abi_version`] exists so a
//! binding that finds itself loaded against a different library refuses rather than crashes.
//!
//! # The rules this file follows
//!
//! **No panic crosses the boundary.** Unwinding into C is undefined behaviour and would take the
//! host application down. Every entry point runs inside [`catch_unwind`](std::panic::catch_unwind)
//! and converts a panic into `VDB_INTERNAL`. That is a bug when it happens, but a reportable one
//! rather than a crash in someone's app.
//!
//! **Every pointer is checked.** A null where one is not expected returns `VDB_NULL_POINTER`
//! rather than dereferencing. Callers are other people's binding code, and eventually one of
//! them will pass null.
//!
//! **Vectors are never copied at the boundary.** `const float*` plus a length; the engine copies
//! once, into the log. That is what lets a JavaScript `Float32Array`, a Dart `Float32List` and a
//! Kotlin `ByteBuffer` pass straight through.
//!
//! **Everything allocated here has an explicit free.** Ownership is stated per function, and the
//! header repeats it.
//!
//! **Calls are synchronous.** The engine spawns no threads; each binding wraps these in its own
//! platform's concurrency primitive.

// A C ABI is unsafe by construction: it receives raw pointers from callers this crate cannot
// see. Every block carries a SAFETY comment stating what the caller must have guaranteed, and
// the header states the same thing in the caller's own language.
#![deny(unsafe_op_in_unsafe_fn)]
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

mod error;
mod handles;
mod strings;

pub use error::{vdb_error_code, vdb_error_free, vdb_error_message, VdbError};

use std::sync::Arc;

use vdb_core::api::{CollectionSpec, Database, DatabaseConfig, SearchRequest, UpsertOutcome};
use vdb_core::clock::Clock;
use vdb_core::document::{DocId, DocumentInput, Include};
use vdb_core::metadata::{Metadata, Value};
use vdb_core::persistence::Durability;
use vdb_core::vector::VectorView;
use vdb_core::{DbError, Metric};
use vdb_storage_os::OsStorage;

use error::{guard, set_error};
use strings::{borrow_bytes, borrow_str};

// Public because they appear in the exported signatures. Opaque to C — the header declares them
// as incomplete types, so nothing about their layout is part of the contract.
pub use handles::{VdbCollection, VdbDb, VdbMetadata, VdbResults};

/// Success.
pub const VDB_OK: i32 = 0;
/// A required pointer was null.
pub const VDB_NULL_POINTER: i32 = -1;
/// A panic was caught at the boundary. Always a bug in the engine.
pub const VDB_INTERNAL: i32 = -2;
/// A string argument was not valid UTF-8.
pub const VDB_INVALID_UTF8: i32 = -3;
/// A discriminant argument was outside its defined range.
pub const VDB_INVALID_ARGUMENT: i32 = -4;

/// The ABI revision.
///
/// Bumped on any change to this header. A binding checks it at load and refuses a mismatch —
/// which is the difference between a clear error and a crash when an application ships a
/// prebuilt library and an SDK that were built at different times.
pub const VDB_ABI_VERSION: u32 = 1;

/// Similarity metric discriminants, matching `vdb_metric_t` in the header.
const METRIC_COSINE: i32 = 1;
const METRIC_L2: i32 = 2;
const METRIC_DOT: i32 = 3;

/// Durability discriminants, matching `vdb_durability_t`.
const DURABILITY_FULL: i32 = 1;
const DURABILITY_BATCH: i32 = 2;
const DURABILITY_RELAXED: i32 = 3;

/// Wall-clock time. The engine reads no clock of its own.
#[derive(Debug)]
struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// version
// ---------------------------------------------------------------------------

/// The library version, as a NUL-terminated string owned by the library.
#[no_mangle]
pub extern "C" fn vdb_version() -> *const std::os::raw::c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// The ABI revision this library implements. See [`VDB_ABI_VERSION`].
#[no_mangle]
pub extern "C" fn vdb_abi_version() -> u32 {
    VDB_ABI_VERSION
}

/// The on-disk format version this library writes.
#[no_mangle]
pub extern "C" fn vdb_format_version() -> u32 {
    u32::from(vdb_format::FORMAT_VERSION)
}

// ---------------------------------------------------------------------------
// database
// ---------------------------------------------------------------------------

/// Open or create a database.
///
/// # Safety
/// `path` must point to `path_len` readable bytes of UTF-8. `out_db` and `err` must be valid
/// writable pointers, or null in the case of `err`. The returned handle must be released with
/// [`vdb_close`].
#[no_mangle]
pub unsafe extern "C" fn vdb_open(
    path: *const u8,
    path_len: usize,
    create_if_missing: bool,
    read_only: bool,
    durability: i32,
    out_db: *mut *mut VdbDb,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        if out_db.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: the caller guarantees `path` points to `path_len` readable bytes.
        let path = unsafe { borrow_str(path, path_len) }?;
        let durability = match durability {
            DURABILITY_FULL => Durability::Full,
            DURABILITY_BATCH => Durability::Batch,
            DURABILITY_RELAXED => Durability::Relaxed,
            _ => return Err(Boundary::InvalidArgument),
        };

        let config = DatabaseConfig::default()
            .create_if_missing(create_if_missing && !read_only)
            .read_only(read_only)
            .durability(durability);
        let storage = Arc::new(OsStorage::open(path).map_err(Boundary::Db)?);
        let db = Database::open_with_index(
            storage,
            config,
            Arc::new(SystemClock),
            Arc::new(vdb_index_flat::FlatIndex::new()),
        )
        .map_err(Boundary::Db)?;

        // SAFETY: `out_db` was checked non-null above and the caller guarantees it is writable.
        unsafe { *out_db = VdbDb::into_raw(db) };
        Ok(())
    })
}

/// Flush and close a database, releasing its lock.
///
/// The handle is invalid afterwards, whether or not this succeeded.
///
/// # Safety
/// `db` must come from [`vdb_open`] and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn vdb_close(db: *mut VdbDb, err: *mut *mut VdbError) -> i32 {
    guard(err, || {
        if db.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: the caller guarantees `db` came from `vdb_open` and is not used again.
        let db = unsafe { VdbDb::from_raw(db) };
        db.close().map_err(Boundary::Db)
    })
}

/// Flush every collection's buffered writes.
///
/// # Safety
/// `db` must be a live handle from [`vdb_open`].
#[no_mangle]
pub unsafe extern "C" fn vdb_flush(db: *const VdbDb, err: *mut *mut VdbError) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees `db` is live.
        let db = unsafe { VdbDb::borrow(db) }?;
        db.flush().map_err(Boundary::Db)
    })
}

// ---------------------------------------------------------------------------
// collections
// ---------------------------------------------------------------------------

/// Create a collection, or open it if the specification matches.
///
/// # Safety
/// `db` must be live; `name` must point to `name_len` readable UTF-8 bytes; `out` must be
/// writable. The returned handle must be released with [`vdb_collection_free`].
#[no_mangle]
pub unsafe extern "C" fn vdb_collection_create(
    db: *const VdbDb,
    name: *const u8,
    name_len: usize,
    dimension: u32,
    metric: i32,
    u64_ids: bool,
    out: *mut *mut VdbCollection,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        if out.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: the caller guarantees `db` is live and `name` is readable.
        let database = unsafe { VdbDb::borrow(db) }?;
        let name = unsafe { borrow_str(name, name_len) }?;
        let metric = match metric {
            METRIC_COSINE => Metric::Cosine,
            METRIC_L2 => Metric::L2,
            METRIC_DOT => Metric::Dot,
            _ => return Err(Boundary::InvalidArgument),
        };

        let mut spec = CollectionSpec::new(name, dimension, metric);
        if u64_ids {
            spec = spec.with_u64_ids();
        }
        let collection = database
            .get_or_create_collection(spec)
            .map_err(Boundary::Db)?;
        // SAFETY: `out` was checked non-null above.
        unsafe { *out = VdbCollection::into_raw(collection) };
        Ok(())
    })
}

/// Open an existing collection.
///
/// # Safety
/// As [`vdb_collection_create`].
#[no_mangle]
pub unsafe extern "C" fn vdb_collection_open(
    db: *const VdbDb,
    name: *const u8,
    name_len: usize,
    out: *mut *mut VdbCollection,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        if out.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: the caller guarantees `db` is live and `name` is readable.
        let database = unsafe { VdbDb::borrow(db) }?;
        let name = unsafe { borrow_str(name, name_len) }?;
        let collection = database.open_collection(name).map_err(Boundary::Db)?;
        // SAFETY: `out` was checked non-null above.
        unsafe { *out = VdbCollection::into_raw(collection) };
        Ok(())
    })
}

/// Delete a collection and everything in it. Irreversible.
///
/// # Safety
/// `db` must be live and `name` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_collection_drop(
    db: *const VdbDb,
    name: *const u8,
    name_len: usize,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees `db` is live and `name` is readable.
        let database = unsafe { VdbDb::borrow(db) }?;
        let name = unsafe { borrow_str(name, name_len) }?;
        database.drop_collection(name).map_err(Boundary::Db)
    })
}

/// Release a collection handle. The collection itself is unaffected.
///
/// # Safety
/// `collection` must come from this library and must not be used again. Null is a no-op, so
/// double-free is harmless if the caller nulls its pointer.
#[no_mangle]
pub unsafe extern "C" fn vdb_collection_free(collection: *mut VdbCollection) {
    if collection.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the handle came from this library and is not reused.
    unsafe { VdbCollection::destroy(collection) };
}

/// Live documents in a collection.
///
/// # Safety
/// `collection` must be live and `out` writable.
#[no_mangle]
pub unsafe extern "C" fn vdb_collection_count(
    collection: *const VdbCollection,
    out: *mut u64,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        if out.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: the caller guarantees `collection` is live.
        let c = unsafe { VdbCollection::borrow(collection) }?;
        let count = c.count().map_err(Boundary::Db)?;
        // SAFETY: `out` was checked non-null above.
        unsafe { *out = count };
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// documents
// ---------------------------------------------------------------------------

/// Insert or replace a document.
///
/// `metadata` may be null. `out_inserted`, if not null, receives whether the document was new.
///
/// # Safety
/// `collection` must be live; `id` must point to `id_len` readable bytes; `vector` must point to
/// `dimension` readable floats. Nothing is retained after the call returns.
#[no_mangle]
pub unsafe extern "C" fn vdb_upsert(
    collection: *const VdbCollection,
    id: *const u8,
    id_len: usize,
    vector: *const f32,
    dimension: u32,
    metadata: *const VdbMetadata,
    out_inserted: *mut bool,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees `collection` is live and `id` is readable.
        let c = unsafe { VdbCollection::borrow(collection) }?;
        let id_bytes = unsafe { borrow_bytes(id, id_len) }?;
        // SAFETY: the caller guarantees `vector` points to `dimension` readable floats.
        let values = unsafe { borrow_floats(vector, dimension) }?;

        let doc_id = decode_id(c, id_bytes)?;
        let mut input = DocumentInput::new(doc_id, VectorView::f32(values));
        if !metadata.is_null() {
            // SAFETY: non-null metadata must be a live handle from `vdb_metadata_new`.
            input = input.with_metadata(unsafe { VdbMetadata::borrow(metadata) }?.clone());
        }

        let outcome = c.upsert(input).map_err(Boundary::Db)?;
        if !out_inserted.is_null() {
            // SAFETY: the caller guarantees a non-null `out_inserted` is writable.
            unsafe { *out_inserted = matches!(outcome, UpsertOutcome::Inserted) };
        }
        Ok(())
    })
}

/// Remove a document. `out_existed`, if not null, receives whether it was there.
///
/// # Safety
/// `collection` must be live and `id` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_delete(
    collection: *const VdbCollection,
    id: *const u8,
    id_len: usize,
    out_existed: *mut bool,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees `collection` is live and `id` is readable.
        let c = unsafe { VdbCollection::borrow(collection) }?;
        let id_bytes = unsafe { borrow_bytes(id, id_len) }?;
        let doc_id = decode_id(c, id_bytes)?;
        let existed = c.delete(doc_id).map_err(Boundary::Db)?;
        if !out_existed.is_null() {
            // SAFETY: the caller guarantees a non-null `out_existed` is writable.
            unsafe { *out_existed = existed };
        }
        Ok(())
    })
}

/// Whether a document exists.
///
/// # Safety
/// `collection` must be live, `id` readable, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn vdb_contains(
    collection: *const VdbCollection,
    id: *const u8,
    id_len: usize,
    out: *mut bool,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        if out.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: the caller guarantees `collection` is live and `id` is readable.
        let c = unsafe { VdbCollection::borrow(collection) }?;
        let id_bytes = unsafe { borrow_bytes(id, id_len) }?;
        let doc_id = decode_id(c, id_bytes)?;
        let found = c.contains(&doc_id).map_err(Boundary::Db)?;
        // SAFETY: `out` was checked non-null above.
        unsafe { *out = found };
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// Search for the nearest documents.
///
/// # Safety
/// `collection` must be live; `query` must point to `dimension` readable floats; `out` must be
/// writable. The result must be released with [`vdb_results_free`].
#[no_mangle]
pub unsafe extern "C" fn vdb_search(
    collection: *const VdbCollection,
    query: *const f32,
    dimension: u32,
    top_k: usize,
    out: *mut *mut VdbResults,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        if out.is_null() {
            return Err(Boundary::Null);
        }
        // SAFETY: the caller guarantees `collection` is live and `query` is readable.
        let c = unsafe { VdbCollection::borrow(collection) }?;
        let values = unsafe { borrow_floats(query, dimension) }?;

        let request =
            SearchRequest::new(VectorView::f32(values), top_k).with_include(Include::NONE);
        let response = c.search(&request).map_err(Boundary::Db)?;
        // SAFETY: `out` was checked non-null above.
        unsafe { *out = VdbResults::into_raw(response) };
        Ok(())
    })
}

/// How many hits a result holds.
///
/// # Safety
/// `results` must be live, or null — in which case this returns 0.
#[no_mangle]
pub unsafe extern "C" fn vdb_results_len(results: *const VdbResults) -> usize {
    if results.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees a non-null handle is live.
    unsafe { (*results).len() }
}

/// A hit's score. Always higher-is-better, whatever the metric.
///
/// # Safety
/// `results` must be live. An out-of-range index yields 0.
#[no_mangle]
pub unsafe extern "C" fn vdb_results_score(results: *const VdbResults, index: usize) -> f32 {
    if results.is_null() {
        return 0.0;
    }
    // SAFETY: the caller guarantees a non-null handle is live.
    unsafe { (*results).score(index) }
}

/// A hit's id, as bytes borrowed from the result.
///
/// The pointer is valid until [`vdb_results_free`]. Interpretation follows the collection's id
/// kind: UTF-8 for string ids, eight little-endian bytes for integer ids.
///
/// # Safety
/// `results` must be live and `out_len` writable. An out-of-range index yields null.
#[no_mangle]
pub unsafe extern "C" fn vdb_results_id(
    results: *const VdbResults,
    index: usize,
    out_len: *mut usize,
) -> *const u8 {
    if results.is_null() || out_len.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the caller guarantees a non-null handle is live and `out_len` is writable.
    unsafe {
        let (ptr, len) = (*results).id(index);
        *out_len = len;
        ptr
    }
}

/// Release a search result.
///
/// # Safety
/// `results` must come from [`vdb_search`] and must not be used again. Null is a no-op.
#[no_mangle]
pub unsafe extern "C" fn vdb_results_free(results: *mut VdbResults) {
    if results.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the handle came from this library and is not reused.
    unsafe { VdbResults::destroy(results) };
}

// ---------------------------------------------------------------------------
// metadata
// ---------------------------------------------------------------------------

/// Create an empty metadata builder. Release with [`vdb_metadata_free`].
#[no_mangle]
pub extern "C" fn vdb_metadata_new() -> *mut VdbMetadata {
    VdbMetadata::into_raw(Metadata::new())
}

/// Release a metadata builder. Null is a no-op.
///
/// # Safety
/// `metadata` must come from [`vdb_metadata_new`] and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn vdb_metadata_free(metadata: *mut VdbMetadata) {
    if metadata.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the handle came from this library and is not reused.
    unsafe { VdbMetadata::destroy(metadata) };
}

/// Set a string field.
///
/// # Safety
/// `metadata` must be live; `key` and `value` must point to readable UTF-8 of the given lengths.
#[no_mangle]
pub unsafe extern "C" fn vdb_metadata_set_string(
    metadata: *mut VdbMetadata,
    key: *const u8,
    key_len: usize,
    value: *const u8,
    value_len: usize,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and both strings are readable.
        let m = unsafe { VdbMetadata::borrow_mut(metadata) }?;
        let key = unsafe { borrow_str(key, key_len) }?.to_owned();
        let value = unsafe { borrow_str(value, value_len) }?.to_owned();
        m.insert(key, Value::Str(value));
        Ok(())
    })
}

/// Set an integer field.
///
/// # Safety
/// `metadata` must be live and `key` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_metadata_set_i64(
    metadata: *mut VdbMetadata,
    key: *const u8,
    key_len: usize,
    value: i64,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and `key` is readable.
        let m = unsafe { VdbMetadata::borrow_mut(metadata) }?;
        let key = unsafe { borrow_str(key, key_len) }?.to_owned();
        m.insert(key, Value::I64(value));
        Ok(())
    })
}

/// Set a floating-point field.
///
/// # Safety
/// `metadata` must be live and `key` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_metadata_set_f64(
    metadata: *mut VdbMetadata,
    key: *const u8,
    key_len: usize,
    value: f64,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and `key` is readable.
        let m = unsafe { VdbMetadata::borrow_mut(metadata) }?;
        let key = unsafe { borrow_str(key, key_len) }?.to_owned();
        m.insert(key, Value::F64(value));
        Ok(())
    })
}

/// Set a boolean field.
///
/// # Safety
/// `metadata` must be live and `key` readable.
#[no_mangle]
pub unsafe extern "C" fn vdb_metadata_set_bool(
    metadata: *mut VdbMetadata,
    key: *const u8,
    key_len: usize,
    value: bool,
    err: *mut *mut VdbError,
) -> i32 {
    guard(err, || {
        // SAFETY: the caller guarantees the handle is live and `key` is readable.
        let m = unsafe { VdbMetadata::borrow_mut(metadata) }?;
        let key = unsafe { borrow_str(key, key_len) }?.to_owned();
        m.insert(key, Value::Bool(value));
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// A failure at the boundary, before or after the engine was involved.
pub(crate) enum Boundary {
    /// A required pointer was null.
    Null,
    /// A string argument was not UTF-8.
    Utf8,
    /// A discriminant was outside its range.
    InvalidArgument,
    /// The engine itself reported a failure.
    Db(DbError),
}

/// Borrow a caller-supplied float array.
///
/// # Safety
/// `ptr` must point to `len` readable `f32` values, or be null when `len` is zero.
unsafe fn borrow_floats<'a>(ptr: *const f32, len: u32) -> Result<&'a [f32], Boundary> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(Boundary::Null);
    }
    // SAFETY: the caller guarantees `len` readable floats at `ptr`, valid for the call.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len as usize) })
}

/// Interpret caller-supplied id bytes according to the collection's id kind.
fn decode_id(collection: &vdb_core::api::Collection, bytes: &[u8]) -> Result<DocId, Boundary> {
    DocId::from_bytes(collection.catalog().id_kind, bytes).map_err(Boundary::Db)
}

/// Turn a boundary failure into a status code, recording the detail for the caller.
pub(crate) fn status(e: Boundary, err: *mut *mut VdbError) -> i32 {
    match e {
        Boundary::Null => VDB_NULL_POINTER,
        Boundary::Utf8 => VDB_INVALID_UTF8,
        Boundary::InvalidArgument => VDB_INVALID_ARGUMENT,
        Boundary::Db(e) => {
            let code = e.code().0 as i32;
            set_error(err, e);
            code
        }
    }
}
