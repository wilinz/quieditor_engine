//! The computations a code editor needs in order to stay responsive on large
//! documents, in pure Rust.
//!
//! Nothing in this crate knows that Dart exists. There is no FFI, no
//! `flatbuffers`, and no `unsafe` — see the lint below. Everything is a plain
//! function over plain Rust types, so it can be exercised with `cargo test`.
//!
//! The companion crate `re_editor_ffi` is the shim that carries these results
//! across the C ABI.
//!
//! # Why this exists
//!
//! The Dart implementation of the editor recomputes whole-document work on
//! every keystroke (syntax highlighting, bracket analysis, search). Measured on
//! the 108k-line sample that ships with the package, a single highlight pass
//! takes tens of seconds and bracket analysis takes ~230 ms — both re-run for
//! every character typed. Moving the algorithms here makes them cheap enough
//! that the per-keystroke cost stops scaling with the size of the document.
#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod buffer;
pub mod chunk;
pub mod highlight;
pub mod search;

/// The version of the C ABI the `re_editor_ffi` shim exposes.
///
/// Dart compares this against the value it was compiled against; a mismatch
/// means the bundled native library is stale and the Dart side falls back to
/// its own implementation rather than calling through with the wrong layout.
///
/// 2: added `re_editor_doc_line_count`, which a Dart build that calls it cannot
/// do without.
/// 3: `re_editor_highlighter_spans` takes a request and answers with a range,
/// and `HighlighterUpdateResponse` carries `scanned_to`.
/// 4: handles are opaque `usize`s rather than pointers, so every entry point
/// that took one has changed what its first parameter means — and the value
/// `re_editor_doc_find_async` echoes back to its callback is one of those
/// rather than a pointer the caller had to keep alive.
pub const ABI_VERSION: u32 = 4;

/// The crate version, as recorded in `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Identity of this build, reported once at start-up so a mismatch between the
/// Dart side and the bundled library is visible in logs rather than silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbiInfo {
    pub abi_version: u32,
    pub core_version: String,
}

impl AbiInfo {
    pub fn current() -> Self {
        Self {
            abi_version: ABI_VERSION,
            core_version: VERSION.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_info_reports_the_compiled_in_version() {
        let info = AbiInfo::current();
        assert_eq!(info.abi_version, ABI_VERSION);
        assert_eq!(info.core_version, VERSION);
    }
}
