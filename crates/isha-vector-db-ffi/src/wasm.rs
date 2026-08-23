//! Linear-memory allocation for WebAssembly embedders.
//!
//! **These are not part of the C ABI.** `include/vdb.h` does not declare them and
//! `vdb_abi_version()` does not cover them. A C caller passing a pointer to `vdb_upsert` already
//! has an allocator; a JavaScript caller does not — it can only see this module's linear memory
//! as an `ArrayBuffer`, and needs the module itself to hand it a region to write into. That is a
//! property of how WebAssembly is called, not of the database's interface, so it lives here and
//! is compiled only for wasm targets.
//!
//! The contract is deliberately narrow: every block returned by [`vdb_wasm_alloc`] must be
//! returned to [`vdb_wasm_free`] with the *same* length. Rust's allocator needs the layout back
//! to free it, and storing a header before each block to avoid that would be a second allocator
//! for no benefit.

use std::alloc::{alloc, dealloc, Layout};

/// The alignment every block is given.
///
/// Eight bytes covers `f64` and `u64`, the widest types that cross this boundary, so a caller
/// can write a vector of doubles straight into a returned block.
const ALIGN: usize = 8;

/// Allocate `len` bytes, returning a pointer into linear memory or null.
///
/// Returns null for a zero length and for any length that cannot form a valid layout, rather
/// than aborting: a caller that mishandles a length should get a null it must check, not a
/// module that traps and takes the database with it.
///
/// # Safety
/// The returned block is uninitialised. The caller must write it before reading it, and must
/// eventually pass it to [`vdb_wasm_free`] with the same `len`.
#[no_mangle]
pub unsafe extern "C" fn vdb_wasm_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return core::ptr::null_mut();
    }
    let Ok(layout) = Layout::from_size_align(len, ALIGN) else {
        return core::ptr::null_mut();
    };
    // SAFETY: the layout is non-zero-sized, which is `alloc`'s requirement.
    unsafe { alloc(layout) }
}

/// Release a block from [`vdb_wasm_alloc`].
///
/// A null pointer or zero length is ignored, so the JavaScript side can free unconditionally in
/// a `finally` without first checking whether the allocation happened.
///
/// # Safety
/// `ptr` must have come from [`vdb_wasm_alloc`] with the same `len`, and must not be used again.
#[no_mangle]
pub unsafe extern "C" fn vdb_wasm_free(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let Ok(layout) = Layout::from_size_align(len, ALIGN) else {
        return;
    };
    // SAFETY: the caller guarantees the pointer and length match a live allocation.
    unsafe { dealloc(ptr, layout) };
}

/// Report a panic message to the embedder before the module dies.
///
/// `panic = "abort"` is the only option on `wasm32-unknown-unknown`, so a panic becomes an
/// `unreachable` instruction and JavaScript sees `RuntimeError: unreachable` with no message and
/// a stack of raw function indices. That is close to undebuggable, and it is what a user would
/// report to us. The hook below sends the message and location across the boundary first, so the
/// failure arrives as text.
///
/// This is a diagnostic, not error handling. Everything the engine *expects* to go wrong comes
/// back as a structured error through the ABI; a panic reaching here is a bug in the engine.
#[cfg_attr(target_arch = "wasm32", link(wasm_import_module = "vdb_host"))]
extern "C" {
    fn vdb_host_panic(ptr: *const u8, len: usize);
    fn vdb_host_now_ms() -> f64;
}

/// Milliseconds since the Unix epoch, from the embedder.
///
/// A double because that is what `Date.now()` returns and what an `f64` represents exactly well
/// past any date this software will see. A host that returns something nonsensical — negative,
/// infinite, NaN — gets zero rather than a wrapped or trapping conversion, because a clock is
/// never worth taking the database down for.
pub(crate) fn host_now_ms() -> u64 {
    // SAFETY: the import takes no arguments and returns a plain double.
    let ms = unsafe { vdb_host_now_ms() };
    if ms.is_finite() && ms >= 0.0 {
        ms as u64
    } else {
        0
    }
}

/// Install the panic hook. Idempotent, and called from [`crate::vdb_open`] so an embedder cannot
/// forget it.
pub(crate) fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let message = info.to_string();
            // SAFETY: the pointer and length describe a live slice for the duration of the call,
            // and the host is required not to retain it.
            unsafe { vdb_host_panic(message.as_ptr(), message.len()) };
        }));
    });
}
