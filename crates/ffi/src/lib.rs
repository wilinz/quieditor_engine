//! Hand-written C ABI shim over [`quieditor_engine`].
//!
//! This crate owns the crossing of the FFI boundary and nothing else — no
//! algorithms live here. The companion crate `quieditor_engine` holds the actual
//! computation, is free of `unsafe`, and can be tested on its own; anything in
//! this crate is bookkeeping.
//!
//! # Calling convention
//!
//! One `#[no_mangle] extern "C"` function per operation, so the signature stays
//! checkable on both sides instead of routing everything through a tagged
//! union. Every operation has the same shape:
//!
//! ```text
//! *mut u8 f(const u8 *request, usize request_len, usize *out_len)
//! ```
//!
//! * The request is a FlatBuffer the caller built. It is read zero-copy and
//!   never mutated; the caller frees it.
//! * The response is a FlatBuffer this crate builds, returned as `(ptr, len)`
//!   with the length written through `out_len`. It is null on failure. The
//!   caller must hand the pair back to [`re_editor_free`].
//!
//! [`memory`] holds both halves of that ownership rule in one place, because
//! getting them out of step is a use-after-free. The operations themselves live
//! in [`api`], one module each.
//!
//! # Handles
//!
//! Three things — a document, a compiled grammar, a highlighter — outlive a
//! call, so the caller holds one across many of them. What it holds is a
//! `usize`, not a pointer: [`registry`] keeps those values and the handle names
//! an entry in it. Nothing on this side dereferences a handle, so a handle that
//! has been released, released twice, fabricated, or passed to the wrong
//! operation reaches nothing and gets the same refusal a missing one gets. That
//! is the difference between a contract and a guarantee, and it is the reason
//! the entry points that take only a handle need no `unsafe`.
//!
//! Zero is not a valid handle; it is what "no handle" is spelled as.
//!
//! # Failure
//!
//! Null means "this did not happen", never "here is an empty answer" — the
//! distinction matters, because the caller responds to the two differently: an
//! empty answer is a real result it must render, while a failure sends it back
//! to its own implementation. A response is returned for every case the
//! operation can express, including an empty list of results.
//!
//! A panic must not unwind across `extern "C"` — that is undefined behaviour —
//! so every entry point that can panic goes through [`guard`], which turns one
//! into a null return. When the panic happened inside an operation on a handle,
//! it also poisons that value's lock and the handle is retired on its next use:
//! a value that was being edited when something went wrong is not one to keep
//! reading. See [`registry::Registry::with`].
#![deny(unsafe_op_in_unsafe_fn)]

use std::panic::{catch_unwind, AssertUnwindSafe};

pub mod api;
mod generated;
mod memory;
mod registry;

/// The ABI this shim was compiled against.
///
/// Dart compares the value it reads through `re_editor_abi_version` against the
/// one its code was generated for. A mismatch means the bundled library is
/// older than the Dart calling it, and the safe move is to use the Dart
/// implementation rather than interpret the bytes wrongly.
pub const ABI_VERSION: u32 = quieditor_engine::ABI_VERSION;

/// Reserves a buffer for a request the caller is about to write into.
///
/// For the one caller that has nowhere else to put it. Everywhere Dart reaches
/// this crate through `dart:ffi` it allocates the request itself and hands the
/// pointer in — but in the browser it reaches it through `WebAssembly`, where
/// there is no `malloc` on that side and no way to reserve room in this
/// module's linear memory except by asking. This is the asking.
///
/// What comes back is a boxed slice, the same shape the operations hand out and
/// [`re_editor_free`] reclaims, so the ownership rule is the one already
/// written down: this crate gives, this crate takes back. A request written
/// into this buffer is freed with the same call, and with the same length.
///
/// Answers null for a length of zero, which doubles as the "no request" pointer
/// the operations already read as empty.
#[no_mangle]
pub extern "C" fn re_editor_alloc(len: usize) -> *mut u8 {
    if len == 0 {
        return std::ptr::null_mut();
    }
    memory::leak(vec![0u8; len]).0
}

/// Releases a buffer handed out by one of the operations in this crate, or by
/// [`re_editor_alloc`].
///
/// # Safety
///
/// `ptr` and `len` must be exactly a pair this crate returned, and must not
/// have been freed already. Passing anything else is undefined behaviour.
#[no_mangle]
pub unsafe extern "C" fn re_editor_free(ptr: *mut u8, len: usize) {
    // SAFETY: the caller guarantees the pair came from this crate and is not
    // already freed, which is exactly `reclaim`'s precondition.
    unsafe { memory::reclaim(ptr, len) }
}

/// Runs `f`, converting a panic into `None` rather than letting it unwind into
/// C.
pub(crate) fn guard<T>(f: impl FnOnce() -> T) -> Option<T> {
    catch_unwind(AssertUnwindSafe(f)).ok()
}

/// # Safety
///
/// `out_len` must be null or point to a writable `usize`.
pub(crate) unsafe fn write_out_len(out_len: *mut usize, value: usize) {
    if !out_len.is_null() {
        // SAFETY: the caller guarantees a non-null `out_len` is writable.
        unsafe { *out_len = value };
    }
}

/// Hands `bytes` to the caller and reports its length.
///
/// # Safety
///
/// `out_len` must be null or point to a writable `usize`.
pub(crate) unsafe fn respond(bytes: Vec<u8>, out_len: *mut usize) -> *mut u8 {
    let (ptr, len) = memory::leak(bytes);
    // SAFETY: as above.
    unsafe { write_out_len(out_len, len) };
    ptr
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_allocated_buffer_is_writable_and_reclaimable() {
        let len = 16;
        let buffer = re_editor_alloc(len);
        assert!(!buffer.is_null());

        // SAFETY: `re_editor_alloc` just handed over this many writable bytes,
        // and this is the only reference to them.
        let bytes = unsafe { std::slice::from_raw_parts_mut(buffer, len) };
        assert_eq!(bytes, &[0u8; 16], "the room comes zeroed");
        bytes[0] = 0xff;
        bytes[len - 1] = 0x01;

        // SAFETY: the pair is exactly what `re_editor_alloc` returned, and this
        // is its first free.
        unsafe { re_editor_free(buffer, len) };
    }

    #[test]
    fn allocating_nothing_answers_null() {
        assert!(re_editor_alloc(0).is_null());
    }

    /// The whole cycle the browser runs, against the ownership rule it has to
    /// obey: allocate here, write here, hand it back here. A `reclaim` that
    /// could not rebuild a buffer that `re_editor_alloc` produced would be a
    /// different shape of allocation, and the mismatch is a use-after-free
    /// rather than an error.
    #[test]
    fn a_buffer_the_allocator_handed_out_survives_a_round_trip() {
        let written = b"the caller's bytes, written across the boundary";
        let buffer = re_editor_alloc(written.len());
        assert!(!buffer.is_null());
        // SAFETY: the buffer covers this many writable bytes.
        unsafe { std::slice::from_raw_parts_mut(buffer, written.len()) }.copy_from_slice(written);

        // Read back through a pair, as the operations do.
        // SAFETY: as above, and only read here.
        let read_back = unsafe { std::slice::from_raw_parts(buffer, written.len()) };
        assert_eq!(read_back, written);

        // SAFETY: exactly the pair `re_editor_alloc` returned, and its first
        // free.
        unsafe { re_editor_free(buffer, written.len()) };
    }
}
