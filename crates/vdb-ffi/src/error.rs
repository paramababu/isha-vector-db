//! Errors across the boundary.
//!
//! An out-parameter rather than a thread-local "last error". A thread-local breaks the moment a
//! binding dispatches work to a pool — which every one of them does, because the engine is
//! synchronous and none of them want to block their UI thread. The error would then belong to
//! whichever worker happened to run the call.

use std::ffi::CString;
use std::os::raw::c_char;
use std::panic::{catch_unwind, AssertUnwindSafe};

use vdb_core::DbError;

use crate::{status, Boundary, VDB_INTERNAL, VDB_OK};

/// A failure, owned by the caller until it calls [`vdb_error_free`].
#[derive(Debug)]
pub struct VdbError {
    code: u32,
    message: CString,
}

impl VdbError {
    fn new(e: &DbError) -> Self {
        // The message is built once, here, so `vdb_error_message` is a pointer read rather than
        // a formatting call the caller might make repeatedly.
        let text = e.to_string();
        let message = CString::new(text).unwrap_or_else(|_| CString::default());
        Self {
            code: e.code().0,
            message,
        }
    }
}

/// Record a failure in the caller's out-parameter, if it wanted one.
pub(crate) fn set_error(slot: *mut *mut VdbError, e: DbError) {
    if slot.is_null() {
        return;
    }
    let boxed = Box::new(VdbError::new(&e));
    // SAFETY: `slot` was checked non-null, and the caller guarantees it is writable.
    unsafe { *slot = Box::into_raw(boxed) };
}

/// Run a boundary function, converting failures and catching panics.
///
/// Unwinding into C is undefined behaviour, so nothing may escape. A panic here is a bug in the
/// engine — it becomes `VDB_INTERNAL`, which is reportable rather than a crash in someone's app.
pub(crate) fn guard(err: *mut *mut VdbError, body: impl FnOnce() -> Result<(), Boundary>) -> i32 {
    // `AssertUnwindSafe` because the closure borrows caller-supplied pointers. If it panics
    // part-way through, the engine's own state is protected by lock poisoning, which surfaces as
    // an `Internal` error on the next call rather than as silent corruption.
    match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => VDB_OK,
        Ok(Err(e)) => status(e, err),
        Err(_) => {
            set_error(
                err,
                vdb_core::internal_error!("a panic was caught at the C boundary"),
            );
            VDB_INTERNAL
        }
    }
}

/// The stable numeric code for a failure. See `docs/api/error-codes.md`.
///
/// # Safety
/// `error` must come from this library, or be null — in which case this returns 0.
#[no_mangle]
pub unsafe extern "C" fn vdb_error_code(error: *const VdbError) -> u32 {
    if error.is_null() {
        return 0;
    }
    // SAFETY: the caller guarantees a non-null pointer came from this library.
    unsafe { (*error).code }
}

/// A human-readable description, NUL-terminated and owned by the error.
///
/// Valid until [`vdb_error_free`].
///
/// # Safety
/// `error` must come from this library, or be null — in which case this returns null.
#[no_mangle]
pub unsafe extern "C" fn vdb_error_message(error: *const VdbError) -> *const c_char {
    if error.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the caller guarantees a non-null pointer came from this library.
    unsafe { (*error).message.as_ptr() }
}

/// Release an error. Null is a no-op.
///
/// # Safety
/// `error` must come from this library and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn vdb_error_free(error: *mut VdbError) {
    if error.is_null() {
        return;
    }
    // SAFETY: the caller guarantees the pointer came from this library and is not reused.
    drop(unsafe { Box::from_raw(error) });
}
