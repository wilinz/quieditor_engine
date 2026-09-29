//! One module per operation exposed over the C ABI.
//!
//! Each module owns a pair of FlatBuffers tables — the request it reads and the
//! response it writes — and nothing else. The algorithms are in
//! `quieditor_engine`; what happens here is decoding, calling, and encoding.
//!
//! Adding an operation means adding a module and one `#[no_mangle]` function,
//! and nothing else: there is no registry to keep in step and no dispatch table
//! to extend.

pub mod abi;
pub mod chunk;
pub mod document;
pub mod find;
pub mod highlight;
pub mod highlighter;

/// Borrows a request buffer that the caller owns.
///
/// Mirrors [`std::slice::from_raw_parts`], including its lifetime arrangement:
/// the returned slice is tied to whichever lifetime the caller binds, and it is
/// the caller's job to make sure that is no longer than the buffer's.
///
/// # Safety
///
/// `ptr` must be null, or point to `len` readable bytes that stay alive and
/// unmutated for the duration of the call.
pub(crate) unsafe fn request_bytes<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        return &[];
    }
    // SAFETY: the caller guarantees `ptr` covers `len` readable bytes.
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// Helpers shared by the operation modules' tests.
#[cfg(test)]
pub(crate) mod test_support {
    use crate::generated::document_generated::re_editor::ffi as doc_fb;

    /// Opens a document whose text and declared line count are whatever the
    /// caller says, even when they disagree.
    ///
    /// Goes through the real entry point rather than constructing a `Document`
    /// directly, so a test that fails is failing in the code that ships.
    ///
    /// The count is passed through rather than checked, which is the only way
    /// to test that a document claiming the wrong number of lines is refused.
    ///
    /// Answers a handle, like the entry point it stands in for: zero when the
    /// document was refused.
    pub(crate) fn create_declaring(text: &str, lines: usize) -> usize {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let text = builder.create_string(text);
        let hidden = builder.create_string("");
        let counts = builder.create_vector(&vec![0u32; lines]);
        let root = doc_fb::CreateRequest::create(
            &mut builder,
            &doc_fb::CreateRequestArgs {
                text: Some(text),
                lines: lines as u32,
                hidden: Some(hidden),
                hidden_counts: Some(counts),
            },
        );
        builder.finish(root, None);
        let request = builder.finished_data().to_vec();
        // SAFETY: `request` is a live buffer of the length given.
        unsafe { crate::api::document::re_editor_doc_create(request.as_ptr(), request.len()) }
    }
}
