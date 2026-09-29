//! Ownership rules for the buffers that cross the C ABI.
//!
//! Every response follows one contract, so the Dart side needs to know exactly
//! one thing: *who frees what*.
//!
//! * Rust allocates the response, hands back a `(ptr, len)` pair, and forgets
//!   about it. The buffer is a boxed slice, so its length equals its capacity
//!   and [`reclaim`] can rebuild it from the length alone.
//! * Dart reads it (zero-copy) and hands the same pair back to
//!   `re_editor_free`.
//!
//! Getting the pair wrong is a use-after-free, so both halves are kept in this
//! one module and neither is exported as a `#[no_mangle]` symbol.
#![deny(unsafe_op_in_unsafe_fn)]

/// Moves `bytes` into an allocation the caller now owns.
///
/// Pairs with [`reclaim`]. Returns an empty (but non-null-dereferenceable)
/// pointer for an empty buffer, which callers treat as "no data".
pub(crate) fn leak(bytes: Vec<u8>) -> (*mut u8, usize) {
    // A boxed slice is exactly (ptr, len) with no spare capacity, which is what
    // makes rebuilding it from a length alone sound.
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let ptr = Box::into_raw(boxed).cast::<u8>();
    (ptr, len)
}

/// Reclaims a buffer previously handed out by [`leak`].
///
/// # Safety
///
/// `ptr` and `len` must be exactly the pair [`leak`] returned, and must not
/// have been reclaimed before.
pub(crate) unsafe fn reclaim(ptr: *mut u8, len: usize) {
    if ptr.is_null() {
        return;
    }
    // Rebuilding the slice, not a Vec, matters: `Vec::from_raw_parts` would
    // assume a capacity of at least `len` and could free with the wrong layout.
    let slice = std::ptr::slice_from_raw_parts_mut(ptr, len);
    // SAFETY: the caller guarantees this pointer came from `leak`, which built
    // it from `Box<[u8]>` with exactly this length.
    unsafe {
        drop(Box::from_raw(slice));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leak_then_reclaim_round_trips_the_contents() {
        let bytes = vec![1u8, 2, 3, 4, 5];
        let (ptr, len) = leak(bytes.clone());
        assert_eq!(len, 5);
        // SAFETY: `leak` just handed us this pair.
        let read_back = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        assert_eq!(read_back, bytes);
        // SAFETY: the pair is intact and this is its first reclaim.
        unsafe { reclaim(ptr, len) };
    }

    #[test]
    fn reclaim_tolerates_a_null_pointer() {
        // SAFETY: null is explicitly allowed and returns immediately.
        unsafe { reclaim(std::ptr::null_mut(), 0) };
    }

    #[test]
    fn an_empty_buffer_is_reclaimable() {
        let (ptr, len) = leak(Vec::new());
        assert_eq!(len, 0);
        // SAFETY: the pair is intact and this is its first reclaim.
        unsafe { reclaim(ptr, len) };
    }
}
