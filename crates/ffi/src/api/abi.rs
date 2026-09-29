//! Reporting the identity of the loaded library.
//!
//! The Dart side calls this once at start-up. Without it a stale bundled
//! library is nearly impossible to diagnose: the symbols resolve, the calls
//! succeed, and only the table layout is wrong.

use crate::generated::abi_generated::re_editor::ffi as fb;
use crate::{guard, respond, write_out_len};

/// The ABI this shim was compiled against.
///
/// Deliberately a bare scalar with no allocation: it is the symbol the Dart
/// side probes for, inside a `try`/`catch`, so it must be cheap and must not be
/// able to fail for any reason other than the symbol being missing.
#[no_mangle]
pub extern "C" fn re_editor_abi_version() -> u32 {
    crate::ABI_VERSION
}

/// Returns an `AbiInfo` FlatBuffer describing the loaded library.
///
/// # Safety
///
/// `out_len` must be null or point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn re_editor_abi_info(out_len: *mut usize) -> *mut u8 {
    // SAFETY: the caller guarantees `out_len` is null or writable.
    unsafe { write_out_len(out_len, 0) };
    match guard(build) {
        // SAFETY: as above; still valid and still writable.
        Some(bytes) => unsafe { respond(bytes, out_len) },
        None => std::ptr::null_mut(),
    }
}

fn build() -> Vec<u8> {
    let info = quieditor_engine::AbiInfo::current();
    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let core_version = builder.create_string(&info.core_version);
    let root = fb::AbiInfo::create(
        &mut builder,
        &fb::AbiInfoArgs {
            abi_version: info.abi_version,
            core_version: Some(core_version),
        },
    );
    builder.finish(root, None);
    builder.finished_data().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trips through the same encode/decode pair Dart uses, so a schema
    /// change that breaks the wire format fails here rather than in the app.
    #[test]
    fn abi_info_round_trips_through_the_wire_format() {
        let bytes = build();
        let decoded = flatbuffers::root::<fb::AbiInfo>(&bytes).expect("valid flatbuffer");
        assert_eq!(decoded.abi_version(), crate::ABI_VERSION);
        assert_eq!(decoded.core_version(), Some(quieditor_engine::VERSION));
    }

    #[test]
    fn entry_point_reports_the_abi_version() {
        assert_eq!(re_editor_abi_version(), crate::ABI_VERSION);
    }

    #[test]
    fn abi_info_entry_point_returns_a_usable_buffer() {
        let mut len = 0usize;
        // SAFETY: `len` is a live local, which is what the signature requires.
        let ptr = unsafe { re_editor_abi_info(&mut len) };
        assert!(!ptr.is_null());
        assert!(len > 0);
        // SAFETY: the call above returned a valid (ptr, len) pair.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let decoded = flatbuffers::root::<fb::AbiInfo>(bytes).expect("valid flatbuffer");
        assert_eq!(decoded.abi_version(), crate::ABI_VERSION);
        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr, len) };
    }

    /// The probe must tolerate a caller that does not want the length.
    #[test]
    fn abi_info_entry_point_tolerates_a_null_out_len() {
        // SAFETY: null is explicitly allowed by the signature.
        let ptr = unsafe { re_editor_abi_info(std::ptr::null_mut()) };
        assert!(!ptr.is_null());
        // We did not learn the length, so the buffer is deliberately leaked
        // rather than guessed at — the test is about the entry point surviving.
    }
}
