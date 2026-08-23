//! Borrowing caller-supplied buffers.
//!
//! Pointer-plus-length rather than NUL-terminated strings. Two reasons: it avoids a `strlen`
//! scan on every call, and it means an id containing a NUL is rejected by the engine's own
//! validation rather than being silently truncated at the boundary — which is the kind of
//! difference that turns into a data-loss bug report.

use crate::Boundary;

/// Borrow a caller-supplied byte buffer.
///
/// # Safety
/// `ptr` must point to `len` readable bytes, or be null when `len` is zero. The buffer must stay
/// valid for the duration of the call.
pub(crate) unsafe fn borrow_bytes<'a>(ptr: *const u8, len: usize) -> Result<&'a [u8], Boundary> {
    if len == 0 {
        // An empty id is invalid, but that is the engine's judgement to make and its error to
        // report — the boundary's job is only to hand over what it was given.
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(Boundary::Null);
    }
    // SAFETY: the caller guarantees `len` readable bytes at `ptr`, valid for the call.
    Ok(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// Borrow a caller-supplied UTF-8 string.
///
/// # Safety
/// As [`borrow_bytes`].
pub(crate) unsafe fn borrow_str<'a>(ptr: *const u8, len: usize) -> Result<&'a str, Boundary> {
    // SAFETY: the caller upholds `borrow_bytes`'s contract.
    let bytes = unsafe { borrow_bytes(ptr, len) }?;
    core::str::from_utf8(bytes).map_err(|_| Boundary::Utf8)
}
