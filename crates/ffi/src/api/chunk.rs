//! Finding the regions the editor can fold.
//!
//! The algorithm is [`quieditor_engine::chunk::analyze`]; this module is the wire
//! format around it.
//!
//! There is no request to decode. The document is already here — see
//! [`crate::api::document`] — which is the whole point of the handle: the
//! analysis used to be handed a whole copy of the document per keystroke, and
//! now it reads the one the native side owns.

use crate::api::document::DOCUMENTS;
use crate::generated::chunk_generated::re_editor::ffi as fb;
use crate::{guard, respond, write_out_len};
use quieditor_engine::buffer::Document;

/// Answers a `ChunkAnalyzeResponse` for the document behind `doc`.
///
/// Returns null when there is no such live document. An empty document, or one
/// with nothing to fold, is an empty response — not null — because the caller
/// has to be able to tell "nothing to fold" from "could not answer".
///
/// # Safety
///
/// `out_len` must be null or point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn re_editor_doc_chunk_analyze(doc: usize, out_len: *mut usize) -> *mut u8 {
    // SAFETY: the caller guarantees `out_len` is null or writable.
    unsafe { write_out_len(out_len, 0) };
    match guard(|| DOCUMENTS.and_then(doc, |document| analyze(document))) {
        // SAFETY: `out_len` is still null or writable.
        Some(Some(bytes)) => unsafe { respond(bytes, out_len) },
        _ => std::ptr::null_mut(),
    }
}

fn analyze(document: &Document) -> Option<Vec<u8>> {
    let chunks = quieditor_engine::chunk::analyze(document.iter());

    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let items = chunks
        .iter()
        .map(|chunk| {
            Some(fb::Chunk::new(
                u32::try_from(chunk.index).ok()?,
                u32::try_from(chunk.end).ok()?,
            ))
        })
        .collect::<Option<Vec<fb::Chunk>>>()?;
    let items = builder.create_vector(&items);
    let root = fb::ChunkAnalyzeResponse::create(
        &mut builder,
        &fb::ChunkAnalyzeResponseArgs {
            chunks: Some(items),
            revision: document.revision(),
        },
    );
    builder.finish(root, None);
    Some(builder.finished_data().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::document::{re_editor_doc_free, re_editor_doc_revision, re_editor_doc_splice};
    use crate::generated::document_generated::re_editor::ffi as doc_fb;

    fn create(text: &str, lines: usize) -> usize {
        crate::api::test_support::create_declaring(text, lines)
    }

    fn analyze_handle(doc: usize) -> Vec<(u32, u32)> {
        let mut len = 0usize;
        // SAFETY: `len` is a live local.
        let ptr = unsafe { re_editor_doc_chunk_analyze(doc, &mut len) };
        assert!(!ptr.is_null(), "analysis was refused");
        // SAFETY: the call above returned a valid (ptr, len) pair.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let response =
            flatbuffers::root::<fb::ChunkAnalyzeResponse>(bytes).expect("valid flatbuffer");
        let chunks = response
            .chunks()
            .map(|chunks| {
                chunks
                    .iter()
                    .map(|chunk| (chunk.index(), chunk.end()))
                    .collect()
            })
            .unwrap_or_default();
        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr, len) };
        chunks
    }

    fn revision_of(doc: usize) -> u64 {
        re_editor_doc_revision(doc)
    }

    /// Splices in lines that hide nothing, which is what these tests need: they
    /// are about the analysis following edits, not about folded content.
    fn splice(doc: usize, start: u32, removed: u32, added: &[&str]) {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let text = builder.create_string(&added.join("\n"));
        // Nothing hidden, but the counts field still has to line up with the
        // lines, or the request is refused.
        let hidden = builder.create_string("");
        let counts = builder.create_vector(&vec![0u32; added.len()]);
        let root = doc_fb::SpliceRequest::create(
            &mut builder,
            &doc_fb::SpliceRequestArgs {
                start,
                removed,
                added: added.len() as u32,
                text: Some(text),
                hidden: Some(hidden),
                hidden_counts: Some(counts),
            },
        );
        builder.finish(root, None);
        let request = builder.finished_data().to_vec();

        let mut len = 0usize;
        // SAFETY: `request` is a live buffer.
        let ptr = unsafe { re_editor_doc_splice(doc, request.as_ptr(), request.len(), &mut len) };
        assert!(!ptr.is_null(), "splice was refused");
        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr, len) };
    }

    #[test]
    fn a_document_is_analyzed_in_place() {
        let doc = create("abc(\nabc\nabc\nabc\nabc)", 5);
        assert_eq!(analyze_handle(doc), vec![(0, 4)]);
        re_editor_doc_free(doc);
    }

    #[test]
    fn nothing_to_fold_is_an_empty_answer_not_a_failure() {
        let doc = create("abc\nabc", 2);
        assert_eq!(analyze_handle(doc), Vec::<(u32, u32)>::new());
        re_editor_doc_free(doc);
    }

    #[test]
    fn an_empty_document_answers() {
        let doc = create("", 1);
        assert_eq!(analyze_handle(doc), Vec::<(u32, u32)>::new());
        re_editor_doc_free(doc);
    }

    #[test]
    fn a_handle_of_zero_is_refused() {
        let mut len = 0usize;
        // SAFETY: `len` is a live local, and zero is a handle like any other to
        // the signature.
        let ptr = unsafe { re_editor_doc_chunk_analyze(0, &mut len) };
        assert!(ptr.is_null());
        assert_eq!(len, 0);
    }

    /// The shape of a real editing session: the document is edited in place and
    /// re-analyzed without ever being sent again.
    #[test]
    fn analysis_follows_edits_to_the_document() {
        let doc = create("abc(\nabc\nabc)", 3);
        assert_eq!(analyze_handle(doc), vec![(0, 2)]);
        assert_eq!(revision_of(doc), 0);

        // Push the closing bracket one line further down.
        splice(doc, 2, 1, &["abc", "abc)"]);
        assert_eq!(revision_of(doc), 1);
        assert_eq!(analyze_handle(doc), vec![(0, 3)]);

        // Drop the lines in between: the bracket has nothing left to close and
        // the region is gone.
        splice(doc, 1, 3, &[]);
        assert_eq!(revision_of(doc), 2);
        assert_eq!(analyze_handle(doc), Vec::<(u32, u32)>::new());

        re_editor_doc_free(doc);
    }

    #[test]
    fn the_response_carries_the_revision_it_describes() {
        let doc = create("abc(\nabc\nabc)", 3);
        // A splice that replaces a line with an identical one is not a change,
        // so the revision must stay put — otherwise a caller polling it would
        // redo work for nothing.
        splice(doc, 0, 1, &["abc("]);
        assert_eq!(revision_of(doc), 0);

        splice(doc, 0, 1, &["xyz("]);
        assert_eq!(revision_of(doc), 1);

        let mut len = 0usize;
        // SAFETY: a live handle.
        let ptr = unsafe { re_editor_doc_chunk_analyze(doc, &mut len) };
        // SAFETY: the call above returned a valid (ptr, len) pair.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let response = flatbuffers::root::<fb::ChunkAnalyzeResponse>(bytes).unwrap();
        assert_eq!(response.revision(), 1);
        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr, len) };
        re_editor_doc_free(doc);
    }
}
