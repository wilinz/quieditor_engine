//! The quieditor C ABI, compiled to WebAssembly for the browser.
//!
//! Everything callable is in [`quieditor_ffi`] and nothing is reimplemented
//! here. What this crate is for is the `cdylib`: a wasm module is one, and the
//! crate that holds the ABI cannot be one, because a `cdylib`-only crate
//! provides no linkable target and nothing could be built on top of it.
//!
//! The module is unmodified for the browser — it builds for
//! `wasm32-unknown-unknown` as it stands. The one thing that differs there is a
//! worker thread, which wasm has none of, and that already has an answer the
//! caller handles.
//!
//! # Why the re-export is here
//!
//! It is load-bearing rather than tidy. `#[no_mangle]` functions are for
//! callers outside Rust, so nothing inside this crate names them, and a symbol
//! nothing refers to is a symbol the linker is free to drop — which would leave
//! a module that instantiates and exports nothing. Naming them is what keeps
//! them, and they are the whole of what this module is for.

pub use quieditor_ffi::*;
