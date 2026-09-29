//! The document handle: creating it, editing it, releasing it.
//!
//! The document is the one thing here with a lifetime, so it is the one thing
//! that crosses as a handle rather than as a FlatBuffer. Dart holds the handle
//! and is responsible for handing it back to [`re_editor_doc_free`]; what
//! happens in between is [`Registry`]'s business.
//!
//! # Threading
//!
//! A handle is created, edited and read from the thread that owns the editor,
//! which is the main isolate. That is no longer a requirement on the caller:
//! the registry serialises operations on one document, so a second thread
//! reaching the same handle blocks rather than racing. What it still cannot do
//! is be *useful* from another thread — the document it edits is the one the
//! editor is showing.

use crate::api::request_bytes;
use crate::generated::document_generated::re_editor::ffi as fb;
use crate::registry::Registry;
use crate::{guard, respond, write_out_len};
use quieditor_engine::buffer::{Document, Line};

/// The live documents, keyed by the handle Dart holds.
///
/// Shared with the sibling modules rather than reached through a function: a
/// document handle is a document handle whichever entry point is given it, and
/// the operations that take one are spread over `chunk` and `find` as well.
pub(crate) static DOCUMENTS: Registry<Document> = Registry::new();

/// Creates a document from a `CreateRequest`.
///
/// Returns 0 when the request will not decode, or when the lines do not add up
/// — a line holding a newline of its own, say, which would put every line index
/// out of step with the caller's. A zero return is the caller's signal to keep
/// its own implementation; it is not an empty document.
///
/// # Safety
///
/// `req` must be null, or point to `req_len` readable bytes that stay valid for
/// the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn re_editor_doc_create(req: *const u8, req_len: usize) -> usize {
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let bytes = unsafe { request_bytes(req, req_len) };
    match guard(|| create(bytes)) {
        Some(Some(document)) => DOCUMENTS.insert(document),
        _ => 0,
    }
}

fn create(request: &[u8]) -> Option<Document> {
    let request = flatbuffers::root::<fb::CreateRequest>(request).ok()?;
    let lines = decode_lines(
        request.text().unwrap_or(""),
        request.lines() as usize,
        request.hidden().unwrap_or(""),
        &request
            .hidden_counts()
            .map(|counts| counts.iter().collect::<Vec<u32>>())
            .unwrap_or_default(),
    )?;
    Some(Document::from_owned_lines(lines))
}

/// Releases a document handle.
///
/// A handle that is not live — already released, never issued, or zero — does
/// nothing, so a double release costs a lookup rather than a second free. That
/// is why this is safe to call with anything at all.
#[no_mangle]
pub extern "C" fn re_editor_doc_free(doc: usize) {
    // Dropped rather than returned: this is where the document itself goes when
    // the caller held the last reference to it.
    drop(DOCUMENTS.remove(doc));
}

/// The document's revision, or 0 when there is no such document.
///
/// A scalar rather than a FlatBuffer because it is a scalar: callers poll it to
/// decide whether work they have in flight is still worth applying.
///
/// Safe to call with any handle, for the same reason [`re_editor_doc_free`] is.
#[no_mangle]
pub extern "C" fn re_editor_doc_revision(doc: usize) -> u64 {
    DOCUMENTS
        .with(doc, |document| document.revision())
        .unwrap_or(0)
}

/// How many lines the document holds, or 0 when there is no such document.
///
/// A scalar for the same reason `revision` is one. It exists so that a refused
/// splice can say how far apart the two sides were: the refusal itself only
/// carries what the caller asked for, which is the side it already knows.
///
/// Safe to call with any handle, for the same reason [`re_editor_doc_free`] is.
#[no_mangle]
pub extern "C" fn re_editor_doc_line_count(doc: usize) -> u32 {
    DOCUMENTS
        .with(doc, |document| document.line_count() as u32)
        .unwrap_or(0)
}

/// Applies a `SpliceRequest` to the document and answers with a
/// `SpliceResponse`.
///
/// Returns null for a handle that is not live, an undecodable request, or a
/// splice that does not fit the document — in which case the document is left
/// untouched.
///
/// # Safety
///
/// `req` must be null or point to `req_len` readable bytes. The first parameter
/// is a handle rather than a pointer, so no value of it is undefined — but a
/// handle that is not live answers null like any other, and the caller reads
/// that the same way.
#[no_mangle]
pub unsafe extern "C" fn re_editor_doc_splice(
    doc: usize,
    req: *const u8,
    req_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    // SAFETY: the caller guarantees `out_len` is null or writable.
    unsafe { write_out_len(out_len, 0) };
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { request_bytes(req, req_len) };
    // A panic here happens inside the document's lock, so it leaves the handle
    // poisoned: this call answers null and the next one retires the handle. The
    // document is deliberately not left usable — a panic part-way through an
    // edit is a bug, and continuing to read a half-edited document would turn
    // one bug into two. See `Registry::with`.
    match guard(|| DOCUMENTS.and_then(doc, |document| splice(document, request))) {
        // SAFETY: `out_len` is still null or writable.
        Some(Some(bytes)) => unsafe { respond(bytes, out_len) },
        _ => std::ptr::null_mut(),
    }
}

fn splice(document: &mut Document, request: &[u8]) -> Option<Vec<u8>> {
    let request = flatbuffers::root::<fb::SpliceRequest>(request).ok()?;
    let added = decode_lines(
        request.text().unwrap_or(""),
        request.added() as usize,
        request.hidden().unwrap_or(""),
        &request
            .hidden_counts()
            .map(|counts| counts.iter().collect::<Vec<u32>>())
            .unwrap_or_default(),
    )?;

    // A splice that does not fit is refused rather than clamped: it means the
    // caller's idea of the document has diverged, and guessing would corrupt
    // the rest of the session.
    let changed = document
        .splice(request.start() as usize, request.removed() as usize, &added)
        .ok()?;

    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let root = fb::SpliceResponse::create(
        &mut builder,
        &fb::SpliceResponseArgs {
            changed,
            revision: document.revision(),
        },
    );
    builder.finish(root, None);
    Some(builder.finished_data().to_vec())
}

/// Rebuilds lines from the four fields they travel in.
///
/// Shared by creating a document and editing one, because the two must agree on
/// what a line is: a document that arrived without its folded content and then
/// received an edit that carried it would hold two kinds of line, and the
/// difference would surface as a search that misses.
///
/// Returns `None` if the fields do not add up. That check is the whole reason
/// the counts are on the wire: the lines were joined with newlines to get here,
/// so one containing a newline of its own would quietly become two and put
/// every index after it out of step with the caller's model.
fn decode_lines(text: &str, expected: usize, hidden: &str, counts: &[u32]) -> Option<Vec<Line>> {
    let texts: Vec<&str> = if expected == 0 {
        Vec::new()
    } else {
        text.split('\n').collect()
    };
    if texts.len() != expected {
        return None;
    }

    // How many hidden lines there are is what `counts` says, and it has to be
    // read that way round: `hidden` is those lines joined with newlines, so a
    // total of `n` is `n - 1` separators and `n` parts — except at `n == 0`,
    // whose join is the empty string too. Deciding from the string instead
    // cannot tell that case from a single hidden line that is itself empty,
    // and those are different documents.
    if counts.len() != expected {
        return None;
    }
    let hidden_total: usize = counts.iter().map(|count| *count as usize).sum();
    let hidden_parts: Vec<&str> = if hidden_total == 0 {
        if !hidden.is_empty() {
            return None;
        }
        Vec::new()
    } else {
        hidden.split('\n').collect()
    };
    // Still the check the counts are on the wire for: a line carrying a newline
    // of its own would join into more parts than there are lines.
    if hidden_parts.len() != hidden_total {
        return None;
    }

    let mut hidden_lines = hidden_parts.into_iter();
    let mut lines = Vec::with_capacity(expected);
    for (index, count) in counts.iter().enumerate() {
        let count = *count as usize;
        let hidden = if count == 0 {
            Vec::new()
        } else {
            (0..count)
                .map(|_| hidden_lines.next().unwrap_or("").to_string())
                .collect()
        };
        lines.push(Line::with_hidden(texts[index], hidden));
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::document_generated::re_editor::ffi as doc_fb;

    fn create(text: &str, lines: usize) -> usize {
        crate::api::test_support::create_declaring(text, lines)
    }

    /// A splice of lines that hide nothing, which is the usual case.
    fn splice_request(start: u32, removed: u32, added: &[&str]) -> Vec<u8> {
        let lines: Vec<(&str, &[&str])> = added.iter().map(|line| (*line, &[][..])).collect();
        encode_splice(start, removed, &lines)
    }

    /// Encodes the request the way Dart does, so the tests exercise the real
    /// encode/decode pair rather than a shortcut.
    fn encode_splice(start: u32, removed: u32, added: &[(&str, &[&str])]) -> Vec<u8> {
        let text = added
            .iter()
            .map(|(text, _)| *text)
            .collect::<Vec<_>>()
            .join("\n");
        let hidden = added
            .iter()
            .flat_map(|(_, hidden)| hidden.iter().copied())
            .collect::<Vec<_>>()
            .join("\n");
        let counts: Vec<u32> = added
            .iter()
            .map(|(_, hidden)| hidden.len() as u32)
            .collect();

        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let text = builder.create_string(&text);
        let hidden = builder.create_string(&hidden);
        let counts = builder.create_vector(&counts);
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
        builder.finished_data().to_vec()
    }

    fn splice(doc: usize, start: u32, removed: u32, added: &[&str]) -> (bool, u64) {
        let request = splice_request(start, removed, added);
        let mut len = 0usize;
        // SAFETY: `request` is a live buffer.
        let ptr = unsafe { re_editor_doc_splice(doc, request.as_ptr(), request.len(), &mut len) };
        assert!(!ptr.is_null(), "splice was refused");
        // SAFETY: the call above returned a valid (ptr, len) pair.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let response =
            flatbuffers::root::<doc_fb::SpliceResponse>(bytes).expect("valid flatbuffer");
        let result = (response.changed(), response.revision());
        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr, len) };
        result
    }

    /// Whether a splice is accepted.
    fn accepts(doc: usize, request: &[u8]) -> bool {
        let mut len = 0usize;
        // SAFETY: `request` is a live buffer.
        let ptr = unsafe { re_editor_doc_splice(doc, request.as_ptr(), request.len(), &mut len) };
        if ptr.is_null() {
            return false;
        }
        // SAFETY: the call above returned a valid (ptr, len) pair.
        unsafe { crate::re_editor_free(ptr, len) };
        true
    }

    /// A splice request whose four fields are given independently, for the
    /// combinations `encode_splice` cannot produce — it derives the counts from
    /// the hidden lines, so it never emits a request that disagrees with
    /// itself.
    fn encode_raw_splice(
        start: u32,
        removed: u32,
        text: &str,
        added: u32,
        hidden: &str,
        counts: &[u32],
    ) -> Vec<u8> {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let text = builder.create_string(text);
        let hidden = builder.create_string(hidden);
        let counts = builder.create_vector(counts);
        let root = doc_fb::SpliceRequest::create(
            &mut builder,
            &doc_fb::SpliceRequestArgs {
                start,
                removed,
                added,
                text: Some(text),
                hidden: Some(hidden),
                hidden_counts: Some(counts),
            },
        );
        builder.finish(root, None);
        builder.finished_data().to_vec()
    }

    #[test]
    fn creating_and_freeing_a_document() {
        let doc = create("a\nb", 2);
        assert_ne!(doc, 0);
        re_editor_doc_free(doc);
        // Zero is the handle a failed create leaves behind, and releasing it is
        // as allowed as releasing one that worked.
        re_editor_doc_free(0);
    }

    /// The sequence the browser has to use. It has no allocator on its side of
    /// the boundary, so the room for the request comes from this crate, is
    /// written into from over there, and is handed back with it — and the
    /// document has to come out the same as when the caller allocated it
    /// itself. Everywhere else this is the path nobody takes; in the browser it
    /// is the only one there is.
    #[test]
    fn a_request_written_into_an_allocated_buffer_is_accepted() {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let text = builder.create_string("a\nb");
        let hidden = builder.create_string("");
        let counts = builder.create_vector(&[0u32, 0u32]);
        let root = doc_fb::CreateRequest::create(
            &mut builder,
            &doc_fb::CreateRequestArgs {
                text: Some(text),
                lines: 2,
                hidden: Some(hidden),
                hidden_counts: Some(counts),
            },
        );
        builder.finish(root, None);
        let encoded = builder.finished_data();

        let buffer = crate::re_editor_alloc(encoded.len());
        assert!(!buffer.is_null());
        // SAFETY: the buffer covers this many writable bytes, and this is the
        // only reference to them.
        unsafe { std::slice::from_raw_parts_mut(buffer, encoded.len()) }.copy_from_slice(encoded);
        // SAFETY: the buffer is live and now holds a request of that length.
        let doc = unsafe { re_editor_doc_create(buffer, encoded.len()) };
        // SAFETY: exactly the pair `re_editor_alloc` returned, and its first
        // free. Freeing the request does not touch the document, which took a
        // copy of what it read.
        unsafe { crate::re_editor_free(buffer, encoded.len()) };

        assert_ne!(doc, 0, "the document was created from the buffer");
        assert_eq!(re_editor_doc_line_count(doc), 2);
        re_editor_doc_free(doc);
    }

    /// Releasing a handle twice must not release the document twice.
    #[test]
    fn releasing_a_handle_twice_is_harmless() {
        let doc = create("a\nb", 2);
        re_editor_doc_free(doc);
        re_editor_doc_free(doc);
    }

    /// The failure a pointer could not survive: reaching a document after it
    /// was released. The handle is not in the table, so every entry point has
    /// an answer to give rather than freed memory to read.
    #[test]
    fn a_released_handle_answers_rather_than_dereferencing() {
        let doc = create("a\nb", 2);
        re_editor_doc_free(doc);

        assert_eq!(re_editor_doc_revision(doc), 0);
        assert_eq!(re_editor_doc_line_count(doc), 0);
        assert!(!accepts(doc, &splice_request(0, 0, &["X"])));
    }

    /// Two documents are two entries: releasing one leaves the other alone.
    #[test]
    fn two_documents_do_not_share_a_handle() {
        let first = create("a\nb", 2);
        let second = create("x\ny", 2);
        assert_ne!(first, second);

        re_editor_doc_free(first);
        assert_eq!(re_editor_doc_line_count(first), 0);
        assert_eq!(re_editor_doc_line_count(second), 2);

        re_editor_doc_free(second);
    }

    #[test]
    fn a_line_count_that_does_not_add_up_is_refused() {
        // Two newlines is three lines, whatever the caller says.
        assert_eq!(create("a\nb\nc", 2), 0);
    }

    #[test]
    fn an_empty_document_is_one_line() {
        assert_ne!(create("", 1), 0);
    }

    #[test]
    fn a_splice_changes_the_document_and_bumps_the_revision() {
        let doc = create("a\nb\nc", 3);
        assert_eq!(re_editor_doc_revision(doc), 0);
        assert_eq!(splice(doc, 1, 1, &["X"]), (true, 1));
        assert_eq!(re_editor_doc_revision(doc), 1);
        re_editor_doc_free(doc);
    }

    #[test]
    fn a_splice_that_changes_nothing_reports_so() {
        let doc = create("a\nb\nc", 3);
        assert_eq!(splice(doc, 1, 1, &["b"]), (false, 0));
        re_editor_doc_free(doc);
    }

    #[test]
    fn a_splice_that_does_not_fit_leaves_the_document_alone() {
        let doc = create("a\nb", 2);
        let request = splice_request(1, 5, &["X"]);
        let mut len = 0usize;
        // SAFETY: a live handle and a live request buffer.
        let ptr = unsafe { re_editor_doc_splice(doc, request.as_ptr(), request.len(), &mut len) };
        assert!(ptr.is_null());
        assert_eq!(len, 0);
        // The revision is untouched, which is how a caller tells that the
        // refused splice did not half-apply.
        assert_eq!(re_editor_doc_revision(doc), 0);
        re_editor_doc_free(doc);
    }

    #[test]
    fn a_hidden_line_that_is_empty_is_still_a_line() {
        // A folded region whose whole content is one empty line — the shape a
        // `{`, a blank line and a `}` produce, and one the editor really sends.
        // That line joins to the empty string, which is also what *no* hidden
        // lines join to, so a decoder that reads the count off the string
        // refuses an edit that is perfectly well formed.
        let doc = create("}{", 1);
        assert!(accepts(doc, &encode_splice(0, 1, &[("}{", &[""][..])])));
        re_editor_doc_free(doc);
    }

    #[test]
    fn hidden_lines_that_do_not_add_up_are_refused() {
        // One part where the counts say two.
        let doc = create("a\nb", 2);
        assert!(!accepts(doc, &encode_raw_splice(0, 1, "a", 1, "x", &[2])));
        re_editor_doc_free(doc);

        // Hidden content where the counts say there is none.
        let doc = create("a\nb", 2);
        assert!(!accepts(doc, &encode_raw_splice(0, 1, "a", 1, "x", &[0])));
        re_editor_doc_free(doc);
    }

    #[test]
    fn no_hidden_lines_is_still_read_as_none() {
        // The other side of the same ambiguity: an empty field with counts
        // saying zero is no hidden lines, and must stay that way.
        let doc = create("a\nb", 2);
        assert!(accepts(doc, &encode_raw_splice(0, 1, "a", 1, "", &[0])));
        assert!(accepts(doc, &splice_request(0, 1, &["a"])));
        re_editor_doc_free(doc);
    }

    #[test]
    fn the_line_count_is_the_documents_own() {
        let doc = create("a\nb\nc", 3);
        assert_eq!(re_editor_doc_line_count(doc), 3);
        // Survives an edit, which is the only time it is read.
        assert_eq!(splice(doc, 1, 1, &["x"]), (true, 1));
        assert_eq!(re_editor_doc_line_count(doc), 3);
        re_editor_doc_free(doc);
    }

    #[test]
    fn a_handle_of_zero_is_refused_rather_than_looked_up() {
        let request = splice_request(0, 0, &["X"]);
        let mut len = 0usize;
        // SAFETY: `request` is a live buffer, and zero is a handle like any
        // other to the signature.
        let ptr = unsafe { re_editor_doc_splice(0, request.as_ptr(), request.len(), &mut len) };
        assert!(ptr.is_null());
        assert_eq!(len, 0);
        assert_eq!(re_editor_doc_revision(0), 0);
        assert_eq!(re_editor_doc_line_count(0), 0);
    }
}
