//! Finding text in the document.
//!
//! The algorithm is [`quieditor_engine::search::find`]; this module is the wire
//! format around it.

use crate::api::document::DOCUMENTS;
use crate::generated::find_generated::re_editor::ffi as fb;
use crate::{guard, memory, respond, write_out_len};
use quieditor_engine::buffer::Document;
use quieditor_engine::search::{FindError, FindOptions};
use std::sync::Arc;

/// Answers a `FindResponse` for the document behind `doc`.
///
/// Returns null when the pattern is not a valid regular expression. That is not
/// an error to report: the Dart caller treats it as "no result", exactly as its
/// own implementation does when `RegExp` refuses the pattern, and falls back to
/// that implementation to say so. An invalid pattern is rare and a whole
/// isolate round trip for one costs nothing worth avoiding.
///
/// A document with no matches answers with an empty list, which is a result
/// rather than a failure — the caller has to be able to tell the two apart.
///
/// # Safety
///
/// `req` must be null or point to `req_len` readable bytes, and `out_len` must
/// be null or point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn re_editor_doc_find(
    doc: usize,
    req: *const u8,
    req_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    // SAFETY: the caller guarantees `out_len` is null or writable.
    unsafe { write_out_len(out_len, 0) };
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { crate::api::request_bytes(req, req_len) };
    match guard(|| DOCUMENTS.and_then(doc, |document| find(document, request))) {
        // SAFETY: `out_len` is still null or writable.
        Some(Some(bytes)) => unsafe { respond(bytes, out_len) },
        _ => std::ptr::null_mut(),
    }
}

/// Called on a worker thread when an asynchronous search finishes.
///
/// `ptr` and `len` describe a response buffer the callback now owns, and must
/// release with [`crate::re_editor_free`]. A null `ptr` means the search could
/// not be answered — an invalid pattern, the same case the synchronous call
/// reports the same way.
///
/// `search` is the value the call that started this search passed as its
/// `search` argument, handed back unchanged. Several searches can be in flight
/// at once and the worker has no idea which one it is running, so this is the
/// only thing that connects an answer to the call waiting for it.
pub type FindCallback = extern "C" fn(*mut u8, usize, usize);

/// Runs a search on a worker thread and calls `callback` with the result.
///
/// The search cannot run where it is asked for. On a document of any size it
/// takes milliseconds, and that is time the thread drawing the editor cannot
/// spend — measured at around 7 ms against 3 ms for the implementation this
/// replaced, which ran on an isolate instead.
///
/// Everything the worker needs is taken as an owned snapshot before the thread
/// starts: the flattened text (an `Arc` clone, so this costs nothing however
/// large the document is), the revision, and a copy of the request bytes. The
/// document itself is never touched by the worker, so an edit on this thread
/// cannot race with a search on that one.
///
/// `search` is echoed back to `callback` unchanged. It is the caller's value,
/// not ours: the caller picks it, so it can put whatever it wants to find the
/// answer with — an index into a table, a pointer it owns, a counter — in place
/// before the call, which is the only moment that works. An identifier issued
/// here and returned instead would arrive too late to be filed under anything:
/// the caller does not have it until this function returns, and nothing on this
/// side stops the worker from finishing first.
///
/// Returns 0 when the work was handed off. A negative return means it was not,
/// and the callback will not be called — the caller should fall back to its own
/// implementation rather than wait. The number says only which of the reasons it
/// was: no callback, no document, no thread, or the snapshot could not be taken.
///
/// # Safety
///
/// `req` must be null or point to `req_len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn re_editor_doc_find_async(
    doc: usize,
    req: *const u8,
    req_len: usize,
    callback: Option<FindCallback>,
    search: usize,
) -> i32 {
    let Some(callback) = callback else {
        return -1;
    };
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { crate::api::request_bytes(req, req_len) }.to_vec();
    // Taken under the document's lock and owned from here on. The lock is
    // released before the thread starts, which is what keeps the worker from
    // holding it — the snapshot is an `Arc` clone, so the lock is held for no
    // longer than a pointer copy, however large the document is.
    let snapshot = guard(|| {
        DOCUMENTS.with(doc, |document| {
            (document.flattened_snapshot(), document.revision())
        })
    });
    let Some(snapshot) = snapshot else {
        // The call panicked while taking the snapshot.
        return -4;
    };
    let Some((text, revision)) = snapshot else {
        return -2;
    };

    // Nothing here needs a `Send` wrapper the way a caller's pointer would:
    // what crosses to the thread is a `usize`, and it is the caller's to keep
    // alive, not ours.
    let spawned = std::thread::Builder::new()
        .name("re_editor-find".to_string())
        .spawn(move || {
            // Guarded for the same reason the entry points are, though the
            // failure lands differently: a panic here cannot unwind into C, but
            // it kills this thread, and the callback is the only thing the
            // caller is waiting on. A null answer says what a pattern that
            // cannot be compiled says, and the caller already reads that.
            let (ptr, len) = match guard(|| find_in_snapshot(text, revision, request)) {
                Some(Some(bytes)) => memory::leak(bytes),
                _ => (std::ptr::null_mut(), 0),
            };
            callback(ptr, len, search);
        });
    match spawned {
        Ok(_) => 0,
        Err(_) => -3,
    }
}

fn find(document: &Document, request: &[u8]) -> Option<Vec<u8>> {
    let text = document.flattened();
    let starts = document.flattened_starts();
    encode(text, &starts, document.revision(), request)
}

/// Runs on a worker thread, with the text it needs already in hand.
///
/// Everything the search touches is owned by this call — the text it was given,
/// the offsets it derives, the request bytes — so nothing here can race with
/// the editor editing the document it came from.
fn find_in_snapshot(text: Arc<String>, revision: u64, request: Vec<u8>) -> Option<Vec<u8>> {
    let starts = quieditor_engine::buffer::line_starts(&text);
    encode(&text, &starts, revision, &request)
}

fn encode(text: &str, starts: &[usize], revision: u64, request: &[u8]) -> Option<Vec<u8>> {
    let request = flatbuffers::root::<fb::FindRequest>(request).ok()?;
    let options = FindOptions {
        pattern: request.pattern().unwrap_or("").to_string(),
        case_sensitive: request.case_sensitive(),
        regex: request.regex(),
    };

    let matches = match quieditor_engine::search::find_in(text, starts, &options) {
        Ok(matches) => matches,
        Err(FindError::InvalidPattern(_)) => return None,
    };

    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let items = matches
        .iter()
        .map(|found| {
            Some(fb::FindMatch::new(
                u32::try_from(found.start_line).ok()?,
                u32::try_from(found.start_offset).ok()?,
                u32::try_from(found.end_line).ok()?,
                u32::try_from(found.end_offset).ok()?,
            ))
        })
        .collect::<Option<Vec<fb::FindMatch>>>()?;
    let items = builder.create_vector(&items);
    let root = fb::FindResponse::create(
        &mut builder,
        &fb::FindResponseArgs {
            matches: Some(items),
            revision,
        },
    );
    builder.finish(root, None);
    Some(builder.finished_data().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::document::re_editor_doc_free;

    fn create(text: &str, lines: usize) -> usize {
        crate::api::test_support::create_declaring(text, lines)
    }

    fn request(pattern: &str, case_sensitive: bool, regex: bool) -> Vec<u8> {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let pattern = builder.create_string(pattern);
        let root = fb::FindRequest::create(
            &mut builder,
            &fb::FindRequestArgs {
                pattern: Some(pattern),
                case_sensitive,
                regex,
            },
        );
        builder.finish(root, None);
        builder.finished_data().to_vec()
    }

    /// Matches as `(start_line, start_offset, end_line, end_offset)`, or `None`
    /// when the native side declined.
    fn find(
        doc: usize,
        pattern: &str,
        case_sensitive: bool,
        regex: bool,
    ) -> Option<Vec<(u32, u32, u32, u32)>> {
        let request = request(pattern, case_sensitive, regex);
        let mut len = 0usize;
        // SAFETY: `request` is a live buffer.
        let ptr = unsafe { re_editor_doc_find(doc, request.as_ptr(), request.len(), &mut len) };
        if ptr.is_null() {
            return None;
        }
        // SAFETY: the call above returned a valid (ptr, len) pair.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
        let response = flatbuffers::root::<fb::FindResponse>(bytes).expect("valid flatbuffer");
        let matches = response
            .matches()
            .map(|matches| {
                matches
                    .iter()
                    .map(|m| {
                        (
                            m.start_line(),
                            m.start_offset(),
                            m.end_line(),
                            m.end_offset(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr, len) };
        Some(matches)
    }

    #[test]
    fn a_literal_is_found_across_the_wire() {
        let doc = create("hello\nworld", 2);
        assert_eq!(find(doc, "world", true, false), Some(vec![(1, 0, 1, 5)]));
        re_editor_doc_free(doc);
    }

    #[test]
    fn nothing_found_is_an_empty_answer_not_a_refusal() {
        let doc = create("hello", 1);
        assert_eq!(find(doc, "zzz", true, false), Some(Vec::new()));
        re_editor_doc_free(doc);
    }

    #[test]
    fn an_invalid_pattern_is_refused_so_the_caller_falls_back() {
        let doc = create("hello", 1);
        assert_eq!(find(doc, "(", true, true), None);
        re_editor_doc_free(doc);
    }

    #[test]
    fn a_handle_of_zero_is_refused() {
        let request = request("a", true, false);
        let mut len = 0usize;
        // SAFETY: `request` is a live buffer, and zero is a handle like any
        // other to the signature.
        let ptr = unsafe { re_editor_doc_find(0, request.as_ptr(), request.len(), &mut len) };
        assert!(ptr.is_null());
        assert_eq!(len, 0);
    }

    /// A released document is as absent as a handle that was never issued, and
    /// a search on one answers rather than reading freed memory.
    #[test]
    fn a_released_document_is_refused() {
        let released = create("hello", 1);
        let live = create("hello", 1);
        re_editor_doc_free(released);

        assert_eq!(find(released, "hello", true, false), None);
        assert_eq!(find(live, "hello", true, false), Some(vec![(0, 0, 0, 5)]));
        re_editor_doc_free(live);
    }

    #[test]
    fn folded_content_is_found_over_the_wire() {
        // The end-to-end version of the core test: what the user folded away is
        // still findable, which is the only reason the document keeps a
        // flattened view at all.
        use crate::api::document::re_editor_doc_splice;
        use crate::generated::document_generated::re_editor::ffi as doc_fb;

        let doc = create("{\n}", 2);
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let text = builder.create_string("{");
        let hidden = builder.create_string("secret inside");
        let counts = builder.create_vector(&[1u32]);
        let root = doc_fb::SpliceRequest::create(
            &mut builder,
            &doc_fb::SpliceRequestArgs {
                start: 0,
                removed: 1,
                added: 1,
                text: Some(text),
                hidden: Some(hidden),
                hidden_counts: Some(counts),
            },
        );
        builder.finish(root, None);
        let splice = builder.finished_data().to_vec();

        let mut len = 0usize;
        // SAFETY: `splice` is a live request buffer.
        let ptr = unsafe { re_editor_doc_splice(doc, splice.as_ptr(), splice.len(), &mut len) };
        assert!(!ptr.is_null(), "the splice was refused");
        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr, len) };

        assert_eq!(find(doc, "secret", true, false), Some(vec![(1, 0, 1, 6)]));
        re_editor_doc_free(doc);
    }

    /// What a finished search is reported as: the response pair, and the
    /// `search` value the call that asked for it passed.
    type Answer = (usize, usize, usize);

    /// The callback of an asynchronous search, which runs on the worker thread.
    ///
    /// It hands the pair back over a channel: the test is on the other thread,
    /// and the callback running at all is half of what is being checked. The
    /// `search` value is the address of that channel's sender, which is what the
    /// caller would use the parameter for — the callback has no other way to
    /// know which search it is answering.
    extern "C" fn answer(response: *mut u8, length: usize, search: usize) {
        // SAFETY: the test keeps the sender alive until this has run — it is
        // waiting on the receive — and handed its address over as `search`.
        let sender = unsafe { &*(search as *const std::sync::mpsc::Sender<Answer>) };
        let _ = sender.send((response as usize, length, search));
    }

    #[test]
    fn a_search_answers_from_the_worker_thread() {
        let doc = create("alpha\nbeta\ngamma", 3);
        let request = request("beta", true, false);
        let (sender, receiver) = std::sync::mpsc::channel::<Answer>();
        let search = &sender as *const _ as usize;

        // SAFETY: `request` is a live buffer, and the sender outlives the
        // callback — the receive below is what waits for it.
        let handed_off = unsafe {
            re_editor_doc_find_async(doc, request.as_ptr(), request.len(), Some(answer), search)
        };
        assert_eq!(handed_off, 0, "the search went to a worker");

        let (ptr, len, echoed) = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the worker called back");
        assert_eq!(echoed, search, "the caller's own value comes back");
        assert_ne!(
            ptr, 0,
            "the search was answered with a buffer, not a refusal"
        );

        // The answer is the one the synchronous call gives for the same request.
        // SAFETY: the callback handed over a live pair of this length.
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
        let response = flatbuffers::root::<fb::FindResponse>(bytes).expect("valid flatbuffer");
        let matches: Vec<(u32, u32, u32, u32)> = response
            .matches()
            .map(|matches| {
                matches
                    .iter()
                    .map(|m| {
                        (
                            m.start_line(),
                            m.start_offset(),
                            m.end_line(),
                            m.end_offset(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(matches, vec![(1, 0, 1, 4)]);

        // SAFETY: the pair is intact and this is its first free.
        unsafe { crate::re_editor_free(ptr as *mut u8, len) };
        re_editor_doc_free(doc);
    }

    /// The document is released while the search is still in flight. Nothing
    /// goes wrong, and that is the property the handle table must not break:
    /// the worker holds a snapshot of the text, not the document, so the
    /// release below and the read on the worker thread do not meet.
    #[test]
    fn a_search_survives_the_document_being_released_mid_flight() {
        let doc = create("alpha\nbeta\ngamma", 3);
        let request = request("beta", true, false);
        let (sender, receiver) = std::sync::mpsc::channel::<Answer>();

        // SAFETY: `request` is a live buffer, and the sender outlives the
        // callback — the receive below is what waits for it.
        let handed_off = unsafe {
            re_editor_doc_find_async(
                doc,
                request.as_ptr(),
                request.len(),
                Some(answer),
                &sender as *const _ as usize,
            )
        };
        assert_eq!(handed_off, 0, "the search went to a worker");

        // Released here, while the worker is still running.
        re_editor_doc_free(doc);

        let (ptr, len, _) = receiver
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the worker called back");
        assert_ne!(ptr, 0, "the search still answered");
        // SAFETY: the callback handed over a live pair of this length.
        unsafe { crate::re_editor_free(ptr as *mut u8, len) };
    }

    #[test]
    fn an_async_search_that_cannot_run_says_so_rather_than_waiting() {
        let request = request("beta", true, false);

        // A handle with no document behind it.
        // SAFETY: zero is a handle like any other to the signature, and
        // `request` is live.
        let refused = unsafe {
            re_editor_doc_find_async(0, request.as_ptr(), request.len(), Some(answer), 0)
        };
        assert_eq!(refused, -2);

        // No callback, so there is nowhere to answer and nothing is started.
        let doc = create("alpha", 1);
        // SAFETY: `request` is live, and there is no callback.
        let refused =
            unsafe { re_editor_doc_find_async(doc, request.as_ptr(), request.len(), None, 0) };
        assert_eq!(refused, -1);
        re_editor_doc_free(doc);
    }
}
