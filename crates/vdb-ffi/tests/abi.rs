//! The ABI exercised as a C caller would exercise it: raw pointers, out-parameters, status
//! codes, and the misuse a real binding will eventually commit.
//!
//! Written in Rust rather than C so it runs in the normal test suite on every platform, without
//! a C toolchain in the loop. What it deliberately does not do is take shortcuts a C caller
//! could not: every call goes through the exported symbols with raw pointers.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::ffi::CStr;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use vdb_ffi::*;

#[derive(Debug)]
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "vdb-abi-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        Self(path)
    }
    fn bytes(&self) -> Vec<u8> {
        self.0.to_string_lossy().into_owned().into_bytes()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const COSINE: i32 = 1;
const BATCH: i32 = 2;

/// Open a database the way a binding would.
fn open(dir: &TempDir) -> *mut vdb_ffi::VdbDb {
    let path = dir.bytes();
    let mut db = ptr::null_mut();
    let mut err = ptr::null_mut();
    let rc = unsafe {
        vdb_open(
            path.as_ptr(),
            path.len(),
            true,
            false,
            BATCH,
            &mut db,
            &mut err,
        )
    };
    assert_eq!(rc, VDB_OK, "open failed: {}", message(err));
    assert!(!db.is_null());
    db
}

fn collection(db: *mut vdb_ffi::VdbDb, dim: u32) -> *mut vdb_ffi::VdbCollection {
    let name = b"docs";
    let mut c = ptr::null_mut();
    let mut err = ptr::null_mut();
    let rc = unsafe {
        vdb_collection_create(
            db,
            name.as_ptr(),
            name.len(),
            dim,
            COSINE,
            false,
            &mut c,
            &mut err,
        )
    };
    assert_eq!(rc, VDB_OK, "create failed: {}", message(err));
    c
}

fn message(err: *mut vdb_ffi::VdbError) -> String {
    if err.is_null() {
        return "<no error>".to_owned();
    }
    let text = unsafe { CStr::from_ptr(vdb_error_message(err)) }
        .to_string_lossy()
        .into_owned();
    unsafe { vdb_error_free(err) };
    text
}

fn upsert(c: *mut vdb_ffi::VdbCollection, id: &str, v: &[f32]) {
    let mut err = ptr::null_mut();
    let rc = unsafe {
        vdb_upsert(
            c,
            id.as_ptr(),
            id.len(),
            v.as_ptr(),
            v.len() as u32,
            ptr::null(),
            ptr::null_mut(),
            &mut err,
        )
    };
    assert_eq!(rc, VDB_OK, "upsert failed: {}", message(err));
}

// ---------------------------------------------------------------------------

#[test]
fn version_information_is_available_without_opening_anything() {
    let v = unsafe { CStr::from_ptr(vdb_version()) }.to_str().unwrap();
    assert!(!v.is_empty());
    assert_eq!(vdb_abi_version(), 1);
    assert_eq!(vdb_format_version(), 1);
}

#[test]
fn the_full_lifecycle_works_through_the_abi() {
    let dir = TempDir::new("lifecycle");
    let db = open(&dir);
    let c = collection(db, 3);

    upsert(c, "east", &[1.0, 0.0, 0.0]);
    upsert(c, "north", &[0.0, 1.0, 0.0]);
    upsert(c, "up", &[0.0, 0.0, 1.0]);

    let mut count = 0u64;
    let mut err = ptr::null_mut();
    assert_eq!(
        unsafe { vdb_collection_count(c, &mut count, &mut err) },
        VDB_OK
    );
    assert_eq!(count, 3);

    // Search, and read the results back through the accessors.
    let query = [0.9f32, 0.1, 0.0];
    let mut results = ptr::null_mut();
    let rc = unsafe { vdb_search(c, query.as_ptr(), 3, 2, &mut results, &mut err) };
    assert_eq!(rc, VDB_OK, "search failed: {}", message(err));
    assert_eq!(unsafe { vdb_results_len(results) }, 2);

    let mut len = 0usize;
    let id_ptr = unsafe { vdb_results_id(results, 0, &mut len) };
    let id = unsafe { std::slice::from_raw_parts(id_ptr, len) };
    assert_eq!(id, b"east");
    assert!(unsafe { vdb_results_score(results, 0) } > 0.9);
    // Out of range yields null and a zero score rather than failing.
    assert!(unsafe { vdb_results_id(results, 99, &mut len) }.is_null());
    assert_eq!(unsafe { vdb_results_score(results, 99) }, 0.0);
    unsafe { vdb_results_free(results) };

    let mut existed = false;
    assert_eq!(
        unsafe { vdb_delete(c, b"east".as_ptr(), 4, &mut existed, &mut err) },
        VDB_OK
    );
    assert!(existed);

    unsafe { vdb_collection_free(c) };
    assert_eq!(
        unsafe { vdb_close(db, &mut err) },
        VDB_OK,
        "{}",
        message(err)
    );
}

#[test]
fn metadata_round_trips_through_the_builder() {
    let dir = TempDir::new("metadata");
    let db = open(&dir);
    let c = collection(db, 2);

    let m = vdb_metadata_new();
    let mut err = ptr::null_mut();
    unsafe {
        assert_eq!(
            vdb_metadata_set_string(m, b"kind".as_ptr(), 4, b"tool".as_ptr(), 4, &mut err),
            VDB_OK
        );
        assert_eq!(
            vdb_metadata_set_i64(m, b"count".as_ptr(), 5, 7, &mut err),
            VDB_OK
        );
        assert_eq!(
            vdb_metadata_set_f64(m, b"price".as_ptr(), 5, 1.5, &mut err),
            VDB_OK
        );
        assert_eq!(
            vdb_metadata_set_bool(m, b"live".as_ptr(), 4, true, &mut err),
            VDB_OK
        );
    }

    let v = [1.0f32, 0.0];
    let mut inserted = false;
    let rc = unsafe {
        vdb_upsert(
            c,
            b"a".as_ptr(),
            1,
            v.as_ptr(),
            2,
            m,
            &mut inserted,
            &mut err,
        )
    };
    assert_eq!(rc, VDB_OK, "{}", message(err));
    assert!(inserted, "a new document should report itself inserted");

    // The builder is the caller's to reuse or free; the document took a copy.
    unsafe { vdb_metadata_free(m) };

    let mut inserted = true;
    unsafe {
        vdb_upsert(
            c,
            b"a".as_ptr(),
            1,
            v.as_ptr(),
            2,
            ptr::null(),
            &mut inserted,
            &mut err,
        )
    };
    assert!(!inserted, "replacing should not report an insert");

    unsafe { vdb_collection_free(c) };
    unsafe { vdb_close(db, &mut err) };
}

#[test]
fn data_survives_close_and_reopen() {
    let dir = TempDir::new("reopen");
    {
        let db = open(&dir);
        let c = collection(db, 2);
        upsert(c, "kept", &[1.0, 0.0]);
        unsafe { vdb_collection_free(c) };
        let mut err = ptr::null_mut();
        assert_eq!(unsafe { vdb_close(db, &mut err) }, VDB_OK);
    }
    let db = open(&dir);
    let mut err = ptr::null_mut();
    let mut c = ptr::null_mut();
    assert_eq!(
        unsafe { vdb_collection_open(db, b"docs".as_ptr(), 4, &mut c, &mut err) },
        VDB_OK
    );
    let mut found = false;
    unsafe { vdb_contains(c, b"kept".as_ptr(), 4, &mut found, &mut err) };
    assert!(found);
    unsafe { vdb_collection_free(c) };
    unsafe { vdb_close(db, &mut err) };
}

// ---- the misuse a real binding will eventually commit ----------------------

/// Every entry point must survive a null where it expects a pointer. Bindings are other people's
/// code, and one of them will pass null.
#[test]
fn null_pointers_are_refused_rather_than_dereferenced() {
    let mut err = ptr::null_mut();
    let mut out = ptr::null_mut();

    unsafe {
        assert_eq!(
            vdb_open(ptr::null(), 10, true, false, BATCH, &mut out, &mut err),
            VDB_NULL_POINTER
        );
        assert_eq!(vdb_close(ptr::null_mut(), &mut err), VDB_NULL_POINTER);
        assert_eq!(vdb_flush(ptr::null(), &mut err), VDB_NULL_POINTER);
        assert_eq!(
            vdb_collection_count(ptr::null(), ptr::null_mut(), &mut err),
            VDB_NULL_POINTER
        );
        assert_eq!(
            vdb_upsert(
                ptr::null(),
                ptr::null(),
                0,
                ptr::null(),
                0,
                ptr::null(),
                ptr::null_mut(),
                &mut err
            ),
            VDB_NULL_POINTER
        );
        assert_eq!(
            vdb_search(ptr::null(), ptr::null(), 0, 1, &mut out.cast(), &mut err),
            VDB_NULL_POINTER
        );
        assert_eq!(
            vdb_metadata_set_i64(ptr::null_mut(), b"k".as_ptr(), 1, 0, &mut err),
            VDB_NULL_POINTER
        );

        // Frees must accept null, so a binding that nulls its pointer cannot double-free.
        vdb_collection_free(ptr::null_mut());
        vdb_results_free(ptr::null_mut());
        vdb_metadata_free(ptr::null_mut());
        vdb_error_free(ptr::null_mut());

        // Accessors on null return a value rather than faulting.
        assert_eq!(vdb_results_len(ptr::null()), 0);
        assert_eq!(vdb_results_score(ptr::null(), 0), 0.0);
        assert_eq!(vdb_error_code(ptr::null()), 0);
        assert!(vdb_error_message(ptr::null()).is_null());
    }
}

/// A binding that does not want error detail passes null, and must not be punished for it —
/// including on the engine-error path, where there is something it is declining to receive.
#[test]
fn a_null_error_slot_is_allowed() {
    // A boundary failure: the out-parameter itself is null.
    let rc = unsafe {
        vdb_open(
            b"/tmp".as_ptr(),
            4,
            true,
            false,
            BATCH,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert_eq!(rc, VDB_NULL_POINTER);

    // And an engine failure, where an error would have been produced had anyone asked.
    let dir = TempDir::new("null-err");
    let db = open(&dir);
    let c = collection(db, 4);
    let short = [1.0f32];
    let rc = unsafe {
        vdb_upsert(
            c,
            b"a".as_ptr(),
            1,
            short.as_ptr(),
            1,
            ptr::null(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    assert!(rc > 0, "the status code must still be returned: {rc}");
    unsafe { vdb_collection_free(c) };
    let mut err = ptr::null_mut();
    unsafe { vdb_close(db, &mut err) };
}

/// An empty path is an *engine* error, not a boundary one. Worth pinning: the boundary hands
/// over what it was given and lets validation judge it, so the caller gets a message about the
/// path rather than an opaque "null pointer".
#[test]
fn an_empty_path_is_reported_by_the_engine_not_the_boundary() {
    let mut db = ptr::null_mut();
    let mut err = ptr::null_mut();
    let rc = unsafe { vdb_open(ptr::null(), 0, true, false, BATCH, &mut db, &mut err) };
    assert!(rc > 0, "expected an engine code, got {rc}");
    assert!(!message(err).is_empty());
}

#[test]
fn invalid_utf8_and_bad_discriminants_are_distinguished() {
    let dir = TempDir::new("invalid");
    let path = dir.bytes();
    let mut db = ptr::null_mut();
    let mut err = ptr::null_mut();

    // An undefined durability value.
    assert_eq!(
        unsafe {
            vdb_open(
                path.as_ptr(),
                path.len(),
                true,
                false,
                99,
                &mut db,
                &mut err,
            )
        },
        VDB_INVALID_ARGUMENT
    );

    // A path that is not UTF-8.
    let bad = [0xFFu8, 0xFE];
    assert_eq!(
        unsafe { vdb_open(bad.as_ptr(), 2, true, false, BATCH, &mut db, &mut err) },
        VDB_INVALID_UTF8
    );

    let db = open(&dir);
    let mut c = ptr::null_mut();
    assert_eq!(
        unsafe { vdb_collection_create(db, b"docs".as_ptr(), 4, 2, 99, false, &mut c, &mut err) },
        VDB_INVALID_ARGUMENT,
        "an undefined metric must be rejected"
    );
    unsafe { vdb_close(db, &mut err) };
}

/// Engine failures arrive as their stable code, with a message the caller can show.
#[test]
fn engine_errors_carry_their_code_and_message() {
    let dir = TempDir::new("errors");
    let db = open(&dir);
    let c = collection(db, 4);

    // The wrong dimension: an engine-level validation failure, not a boundary one.
    let short = [1.0f32, 2.0];
    let mut err = ptr::null_mut();
    let rc = unsafe {
        vdb_upsert(
            c,
            b"a".as_ptr(),
            1,
            short.as_ptr(),
            2,
            ptr::null(),
            ptr::null_mut(),
            &mut err,
        )
    };
    assert!(
        rc > 0,
        "an engine error should be a positive code, got {rc}"
    );
    assert!(!err.is_null(), "the error slot should have been filled");
    assert_eq!(
        unsafe { vdb_error_code(err) } as i32,
        rc,
        "code should match the return value"
    );
    let text = message(err);
    assert!(text.contains("dimension"), "{text}");
    assert!(
        text.contains("docs"),
        "the message should name the collection: {text}"
    );

    unsafe { vdb_collection_free(c) };
    let mut err = ptr::null_mut();
    unsafe { vdb_close(db, &mut err) };
}

#[test]
fn opening_the_same_database_twice_is_refused_with_a_usable_error() {
    let dir = TempDir::new("lock");
    let first = open(&dir);

    let path = dir.bytes();
    let mut second = ptr::null_mut();
    let mut err = ptr::null_mut();
    let rc = unsafe {
        vdb_open(
            path.as_ptr(),
            path.len(),
            true,
            false,
            BATCH,
            &mut second,
            &mut err,
        )
    };
    assert!(rc > 0, "expected an engine error, got {rc}");
    let text = message(err);
    assert!(text.to_lowercase().contains("already open"), "{text}");

    let mut err = ptr::null_mut();
    unsafe { vdb_close(first, &mut err) };
}
