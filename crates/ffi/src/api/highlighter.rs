//! Highlighting a document as it is edited.
//!
//! The engine is [`quieditor_engine::highlight`], the piecewise driver is
//! [`quieditor_engine::highlight::incremental`], and this module is the handle and
//! the wire format around them. What it adds is the thing the editor needs and
//! the driver cannot know: a document that outlives a call, so that an edit costs
//! the lines it touched instead of a document crossing the boundary and being
//! highlighted from the top.
//!
//! A keystroke near the top of a 181,000-line document costs a full highlight
//! otherwise — 871 ms on the machine this was written on, sixty times faster than
//! the Dart implementation and still a second. Through here it costs the lines
//! between the last recorded state before the edit and the line where the state
//! is what it was, which is tens of lines.

use crate::generated::highlight_generated::re_editor::ffi as fb;
use crate::registry::Registry;
use crate::{guard, respond, write_out_len};
use quieditor_engine::buffer;
use quieditor_engine::highlight::compile_json;
use quieditor_engine::highlight::incremental::{Incremental, Span};
use std::collections::HashMap;

/// A document being highlighted in pieces, with the grammar that highlights it.
pub struct Highlighter {
    incremental: Incremental,
}

/// The live highlighters, keyed by the handle Dart holds.
static HIGHLIGHTERS: Registry<Highlighter> = Registry::new();

/// Builds a highlighter from a `HighlighterRequest`.
///
/// Returns 0 when the request will not decode or the grammar will not compile —
/// the same answer every other operation gives for something it cannot do, and
/// the caller's signal to highlight with the Dart implementation instead.
///
/// # Safety
///
/// `req` must be null, or point to `req_len` readable bytes that stay valid for
/// the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn re_editor_highlighter_create(req: *const u8, req_len: usize) -> usize {
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { crate::api::request_bytes(req, req_len) };
    match guard(|| build(request)) {
        Some(Some(highlighter)) => HIGHLIGHTERS.insert(highlighter),
        _ => 0,
    }
}

/// Releases a highlighter.
///
/// A handle that is not live — already released, never issued, or zero — does
/// nothing, so a double release costs a lookup rather than a second free. That
/// is why this is safe to call with anything at all.
#[no_mangle]
pub extern "C" fn re_editor_highlighter_free(highlighter: usize) {
    drop(HIGHLIGHTERS.remove(highlighter));
}

/// Applies an edit and answers with a `HighlighterUpdateResponse`.
///
/// Returns null when there is no such live highlighter, when the request will
/// not decode, or when the grammar has stopped being able to highlight the text.
/// A splice that does not fit the document is clamped rather than refused: the
/// caller's lines and this side's lines come from the same edits, and refusing
/// would mean the two disagreeing about a document neither can see the other's
/// copy of.
///
/// # Safety
///
/// `req` must be null or point to `req_len` readable bytes, and `out_len` must
/// be null or point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn re_editor_highlighter_update(
    highlighter: usize,
    req: *const u8,
    req_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    // SAFETY: the caller guarantees `out_len` is null or writable.
    unsafe { write_out_len(out_len, 0) };
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { crate::api::request_bytes(req, req_len) };
    match guard(|| HIGHLIGHTERS.and_then(highlighter, |highlighter| update(highlighter, request))) {
        // SAFETY: `out_len` is still null or writable.
        Some(Some(bytes)) => unsafe { respond(bytes, out_len) },
        _ => std::ptr::null_mut(),
    }
}

/// Highlights up to the line a `HighlighterSpansRequest` asks for, and answers
/// with a `HighlighterSpansResponse` covering the lines that added.
///
/// How a caller highlights a document in pieces: it asks for the lines it is
/// about to draw, and gets back the spans for everything between where it had
/// got to and there. Asking for the whole document is how a caller asks for
/// everything, which is what one whose cache has been thrown away — a change of
/// theme, say — does.
///
/// This is not a read: walking the document records the states it passes, which
/// is what a later edit resumes from. That is also why the two entry points
/// cannot be merged — each takes the highlighter as an exclusive borrow.
///
/// # Safety
///
/// As [`re_editor_highlighter_update`].
#[no_mangle]
pub unsafe extern "C" fn re_editor_highlighter_spans(
    highlighter: usize,
    req: *const u8,
    req_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    // SAFETY: the caller guarantees `out_len` is null or writable.
    unsafe { write_out_len(out_len, 0) };
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { crate::api::request_bytes(req, req_len) };
    match guard(|| {
        HIGHLIGHTERS.and_then(highlighter, |highlighter| {
            scan(&mut highlighter.incremental, request)
        })
    }) {
        // SAFETY: `out_len` is still null or writable.
        Some(Some(bytes)) => unsafe { respond(bytes, out_len) },
        _ => std::ptr::null_mut(),
    }
}

fn build(request: &[u8]) -> Option<Highlighter> {
    let request = flatbuffers::root::<fb::HighlighterRequest>(request).ok()?;
    let grammar = compile_json(request.json()?).ok()?;

    let mut sub_grammars = HashMap::new();
    for sub in request.sub_languages().iter().flatten() {
        let (Some(name), Some(json)) = (sub.name(), sub.json()) else {
            return None;
        };
        sub_grammars.insert(name.to_string(), compile_json(json).ok()?);
    }

    // Nothing is highlighted here. The caller asks for the lines it is about to
    // draw, and the rest of the document waits until something asks for it —
    // which is the difference between opening a large file and scanning all of
    // it before the first frame.
    let incremental = Incremental::new(grammar, sub_grammars, request.text()?.to_string())?;
    Some(Highlighter { incremental })
}

fn update(highlighter: &mut Highlighter, request: &[u8]) -> Option<Vec<u8>> {
    let request = flatbuffers::root::<fb::HighlighterSpliceRequest>(request).ok()?;
    let added: Vec<&str> = request.added().iter().flatten().collect();
    let update = highlighter.incremental.update(
        request.start() as usize,
        request.removed() as usize,
        &added,
    );

    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let nodes = nodes(&mut builder, &update.spans, &highlighter.incremental)?;
    let root = fb::HighlighterUpdateResponse::create(
        &mut builder,
        &fb::HighlighterUpdateResponseArgs {
            from: update.from as u32,
            to: update.to as u32,
            replaced: update.replaced as u32,
            nodes: Some(nodes),
            scanned_to: update.scanned_to as u32,
        },
    );
    builder.finish(root, None);
    Some(builder.finished_data().to_vec())
}

/// Highlights up to the line the request asks for, and answers with the lines
/// that added.
fn scan(incremental: &mut Incremental, request: &[u8]) -> Option<Vec<u8>> {
    let request = flatbuffers::root::<fb::HighlighterSpansRequest>(request).ok()?;
    let scan = incremental.scan_to(request.to() as usize);

    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let nodes = nodes(&mut builder, &scan.spans, incremental)?;
    let root = fb::HighlighterSpansResponse::create(
        &mut builder,
        &fb::HighlighterSpansResponseArgs {
            from: u32::try_from(scan.from).ok()?,
            to: u32::try_from(scan.to).ok()?,
            nodes: Some(nodes),
        },
    );
    builder.finish(root, None);
    Some(builder.finished_data().to_vec())
}

/// Writes spans as `HighlightNode`s, in the lines and offsets the editor counts
/// in.
///
/// The driver works in byte offsets into the document, which is what the engine
/// gives it; the editor works in lines and UTF-16 offsets, which is what it
/// draws. The conversion is here, in one place, because a span that disagreed
/// with a match about where a line ends would be a bug in whichever of the two
/// was read second.
fn nodes<'a>(
    builder: &mut flatbuffers::FlatBufferBuilder<'a>,
    spans: &[Span],
    incremental: &Incremental,
) -> Option<
    flatbuffers::WIPOffset<
        flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<fb::HighlightNode<'a>>>,
    >,
> {
    let text = incremental.text();
    let starts = incremental.line_starts();
    let items = spans
        .iter()
        .map(|span| {
            let (start_line, start_offset) = locate(text, starts, span.start);
            let (end_line, end_offset) = locate(text, starts, span.end);
            let scope = builder.create_string(&span.scope);
            Some(fb::HighlightNode::create(
                builder,
                &fb::HighlightNodeArgs {
                    scope: Some(scope),
                    start_line: u32::try_from(start_line).ok()?,
                    start_offset: u32::try_from(start_offset).ok()?,
                    end_line: u32::try_from(end_line).ok()?,
                    end_offset: u32::try_from(end_offset).ok()?,
                    depth: u32::try_from(span.depth).ok()?,
                },
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(builder.create_vector(&items))
}

/// The line a byte offset falls in, and how far into it.
///
/// The driver counts in bytes; the editor counts in UTF-16 code units, and the
/// renderer cuts Dart strings — which are UTF-16 — at these offsets. Returning
/// the byte distance instead put every span on a line containing anything
/// outside ASCII in the wrong place, one column further out per byte the text
/// ran ahead of its unit count.
///
/// The column comes from [`buffer::utf16_column`] rather than from
/// [`buffer::locate`], which is otherwise the same function: that one reads line
/// starts carrying a sentinel for the end of the text, and the incremental
/// driver's do not.
fn locate(text: &str, line_starts: &[usize], offset: usize) -> (usize, usize) {
    match line_starts.binary_search(&offset) {
        Ok(index) => (index, 0),
        Err(index) => {
            let line = index.saturating_sub(1);
            let line_start = line_starts.get(line).copied().unwrap_or(0);
            (line, buffer::utf16_column(text, line_start, offset))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grammar with a string, a comment and a mode that contains itself.
    const GRAMMAR: &str = r##"{
        "name": "Doc",
        "keywords": {"keyword": "def end"},
        "contains": [
            {"scope": "string", "begin": "\"", "end": "\""},
            {"scope": "comment", "begin": "//", "end": "$"},
            {"scope": "paren", "begin": "\\{", "end": "\\}", "contains": [{"selfReferential": true}]}
        ]
    }"##;

    fn document(lines: usize) -> String {
        (0..lines)
            .map(|line| match line % 6 {
                0 => "def f()\n",
                1 => "  // note\n",
                2 => "  \"text\"\n",
                3 => "  {\n",
                4 => "  }\n",
                _ => "end\n",
            })
            .collect()
    }

    fn create(json: &str, text: &str) -> usize {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let json = builder.create_string(json);
        let text = builder.create_string(text);
        let subs = builder.create_vector::<flatbuffers::WIPOffset<fb::SubGrammar>>(&[]);
        let root = fb::HighlighterRequest::create(
            &mut builder,
            &fb::HighlighterRequestArgs {
                json: Some(json),
                sub_languages: Some(subs),
                text: Some(text),
            },
        );
        builder.finish(root, None);
        let request = builder.finished_data().to_vec();
        // SAFETY: `request` is a live buffer of the length given.
        unsafe { re_editor_highlighter_create(request.as_ptr(), request.len()) }
    }

    /// A `HighlighterSpansRequest` asking to be highlighted up to `to`.
    fn spans_request(to: u32) -> Vec<u8> {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let root = fb::HighlighterSpansRequest::create(
            &mut builder,
            &fb::HighlighterSpansRequestArgs { to },
        );
        builder.finish(root, None);
        builder.finished_data().to_vec()
    }

    /// Highlights up to `to` and answers with the lines that added, as
    /// `(scope, start_line, end_line)`.
    fn scan_to(highlighter: usize, to: u32) -> (Vec<(String, u32, u32)>, u32, u32) {
        let request = spans_request(to);
        let mut out_len = 0usize;
        // SAFETY: the handle is live until the caller frees it, `request` is a
        // live buffer, and `out_len` is a live local.
        let response = unsafe {
            re_editor_highlighter_spans(highlighter, request.as_ptr(), request.len(), &mut out_len)
        };
        assert!(!response.is_null(), "the document is answered");
        // SAFETY: the entry point returned this pair, of this length.
        let bytes = unsafe { std::slice::from_raw_parts(response, out_len) };
        let decoded = flatbuffers::root::<fb::HighlighterSpansResponse>(bytes).expect("decodes");
        let nodes = decoded
            .nodes()
            .into_iter()
            .flatten()
            .map(|node| {
                (
                    node.scope().unwrap_or_default().to_string(),
                    node.start_line(),
                    node.end_line(),
                )
            })
            .collect();
        let range = (decoded.from(), decoded.to());
        // SAFETY: the pair is exactly what the entry point returned.
        unsafe { crate::re_editor_free(response, out_len) };
        (nodes, range.0, range.1)
    }

    /// Every span in the document, as `(scope, start_line, end_line)`.
    fn spans(highlighter: usize) -> Vec<(String, u32, u32)> {
        // `u32::MAX` is past the end of any document, which the core clamps.
        scan_to(highlighter, u32::MAX).0
    }

    /// Applies an edit, returning the spans it says are new, and the range.
    fn splice(
        highlighter: usize,
        start: usize,
        removed: usize,
        added: &[&str],
    ) -> (Vec<(String, u32, u32)>, u32, u32, u32) {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let lines = added
            .iter()
            .map(|line| builder.create_string(line))
            .collect::<Vec<_>>();
        let lines = builder.create_vector(&lines);
        let root = fb::HighlighterSpliceRequest::create(
            &mut builder,
            &fb::HighlighterSpliceRequestArgs {
                start: start as u32,
                removed: removed as u32,
                added: Some(lines),
            },
        );
        builder.finish(root, None);
        let request = builder.finished_data().to_vec();

        let mut out_len = 0usize;
        // SAFETY: the handle is live, `request` outlives the call, and `out_len`
        // is a live local.
        let response = unsafe {
            re_editor_highlighter_update(highlighter, request.as_ptr(), request.len(), &mut out_len)
        };
        assert!(!response.is_null(), "the edit is answered");
        // SAFETY: the entry point returned this pair, of this length.
        let bytes = unsafe { std::slice::from_raw_parts(response, out_len) };
        let decoded = flatbuffers::root::<fb::HighlighterUpdateResponse>(bytes).expect("decodes");
        let nodes = decoded
            .nodes()
            .into_iter()
            .flatten()
            .map(|node| {
                (
                    node.scope().unwrap_or_default().to_string(),
                    node.start_line(),
                    node.end_line(),
                )
            })
            .collect();
        let (from, to, replaced) = (decoded.from(), decoded.to(), decoded.replaced());
        // SAFETY: the pair is exactly what the entry point returned.
        unsafe { crate::re_editor_free(response, out_len) };
        (nodes, from, to, replaced)
    }

    #[test]
    fn a_scan_answers_the_lines_it_added() {
        // What a caller draws with: it asks for the lines it is about to show,
        // and gets back the spans of everything between where it had got to and
        // there — not the whole document, which is the point of asking.
        let highlighter = create(GRAMMAR, &document(1_000));

        let (first, from, to) = scan_to(highlighter, 64);
        assert_eq!(from, 0, "nothing had been highlighted before this");
        assert!(to >= 64, "the scan reaches the line it was asked for");
        assert!(
            first.iter().all(|(_, start, _)| *start < to),
            "every span is inside the lines this scan covers: {first:?}"
        );

        let stopped = to;
        let (second, from, to) = scan_to(highlighter, 128);
        assert_eq!(
            from, stopped,
            "a scan carries on from where the last one stopped"
        );
        assert!(to >= 128, "and reaches the line this one was asked for");
        assert!(
            second.iter().all(|(_, start, _)| *start < to),
            "every span is inside the lines this scan covers: {second:?}"
        );

        // Asking again for a line already covered costs nothing and adds
        // nothing: there are no lines between the watermark and there.
        let (again, from, to) = scan_to(highlighter, 64);
        assert!(again.is_empty(), "nothing was added: {again:?}");
        assert_eq!(from, to, "a scan with nothing to do covers no lines");

        // SAFETY: the handle came from `create` and is not used again.
        re_editor_highlighter_free(highlighter);
    }

    #[test]
    fn a_highlighter_reports_the_whole_document() {
        let highlighter = create(GRAMMAR, &document(4));
        assert_eq!(
            spans(highlighter),
            vec![
                ("keyword".to_string(), 0, 0),
                ("comment".to_string(), 1, 1),
                ("string".to_string(), 2, 2),
                // The block opens on one line and closes on the next.
                ("paren".to_string(), 3, 4),
            ]
        );
        // SAFETY: the handle came from `create` and is not used again.
        re_editor_highlighter_free(highlighter);
    }

    #[test]
    fn an_edit_answers_with_the_lines_it_changed() {
        let highlighter = create(GRAMMAR, &document(200));
        // The states an edit resumes from are recorded by scanning, so this is
        // what a caller does on opening — the editor asks for the whole document
        // before it lets anyone type into it.
        assert!(!spans(highlighter).is_empty());
        let (nodes, from, to, replaced) = splice(highlighter, 1, 1, &["  // changed"]);
        assert_eq!((from, to, replaced), (0, 32, 32));
        assert!(
            nodes.iter().any(|(scope, ..)| scope == "comment"),
            "the changed line's comment is in the answer: {nodes:?}"
        );
        assert!(
            to - from <= 32,
            "an edit re-highlighted {} lines of 200",
            to - from
        );
        // SAFETY: the handle came from `create` and is not used again.
        re_editor_highlighter_free(highlighter);
    }

    /// Every span's scope and offsets, which the line-only helper above drops.
    fn placed(highlighter: usize) -> Vec<(String, u32, u32)> {
        let request = spans_request(u32::MAX);
        let mut out_len = 0usize;
        // SAFETY: the handle is live until the caller frees it, `request` is a
        // live buffer, and `out_len` is a live local.
        let response = unsafe {
            re_editor_highlighter_spans(highlighter, request.as_ptr(), request.len(), &mut out_len)
        };
        assert!(!response.is_null(), "the document is answered");
        // SAFETY: the entry point returned this pair, of this length.
        let bytes = unsafe { std::slice::from_raw_parts(response, out_len) };
        let decoded = flatbuffers::root::<fb::HighlighterSpansResponse>(bytes).expect("decodes");
        let nodes = decoded
            .nodes()
            .into_iter()
            .flatten()
            .map(|node| {
                (
                    node.scope().unwrap_or_default().to_string(),
                    node.start_offset(),
                    node.end_offset(),
                )
            })
            .collect();
        // SAFETY: the pair is exactly what the entry point returned.
        unsafe { crate::re_editor_free(response, out_len) };
        nodes
    }

    #[test]
    fn offsets_are_counted_in_utf16_units() {
        // The engine counts offsets in bytes; the editor draws Dart strings,
        // which are UTF-16 code units, so these have to be converted. A line of
        // nothing but ASCII cannot tell the two apart, and the corpus was all
        // ASCII — every span on a line holding anything else came out one
        // column too far out per byte the text ran ahead of its unit count.
        let cases = [
            // Two three-byte characters: eight bytes, four units.
            ("\"\u{65e5}\u{672c}\"\n", 4),
            // One astral character: four bytes, two units. This is the case that
            // tells UTF-16 apart from counting characters.
            ("\"\u{1F600}\"\n", 4),
        ];
        for (text, ends_at) in cases {
            let highlighter = create(GRAMMAR, text);
            let placed = placed(highlighter);
            let string = placed
                .iter()
                .find(|(scope, ..)| scope == "string")
                .unwrap_or_else(|| panic!("{text:?} highlighted a string: {placed:?}"));
            assert_eq!(
                (string.1, string.2),
                (0, ends_at),
                "the quoted span in {text:?} is {ends_at} UTF-16 units wide"
            );
            // SAFETY: the handle came from `create` and is not used again.
            re_editor_highlighter_free(highlighter);
        }
    }

    #[test]
    fn a_document_without_a_trailing_newline_is_highlighted_to_the_end() {
        // The editor's text is its lines joined with `\n`, so it does not end
        // with one. The scan stopped at the last line's *start*, which is the
        // end of the text only when a trailing newline puts a last start there —
        // so the final line went unscanned, and a document of one line came back
        // with nothing at all.
        let highlighter = create(GRAMMAR, "def f()");
        assert_eq!(spans(highlighter), vec![("keyword".to_string(), 0, 0)]);
        // SAFETY: the handle came from `create` and is not used again.
        re_editor_highlighter_free(highlighter);

        let highlighter = create(GRAMMAR, "def f()\n  \"text\"");
        assert_eq!(
            spans(highlighter),
            vec![("keyword".to_string(), 0, 0), ("string".to_string(), 1, 1),]
        );
        // SAFETY: the handle came from `create` and is not used again.
        re_editor_highlighter_free(highlighter);
    }

    #[test]
    fn a_grammar_that_does_not_compile_is_refused() {
        assert_eq!(create("{", &document(1)), 0);
    }

    #[test]
    fn a_handle_of_zero_is_answered_with_nothing() {
        let mut out_len = 0usize;
        // SAFETY: zero is a handle like any other to the signature, a null
        // request is acceptable, and `out_len` is a live local.
        let response =
            unsafe { re_editor_highlighter_update(0, std::ptr::null(), 0, &mut out_len) };
        assert!(response.is_null());
        assert_eq!(out_len, 0);
    }

    /// Releasing a highlighter twice must not release it twice, and a released
    /// handle must not reach the highlighter that was behind it.
    #[test]
    fn a_released_handle_answers_rather_than_dereferencing() {
        let highlighter = create(r#"{"name": "T"}"#, &document(1));
        assert_ne!(highlighter, 0);

        re_editor_highlighter_free(highlighter);
        re_editor_highlighter_free(highlighter);

        let mut out_len = 0usize;
        let request = spans_request(u32::MAX);
        // SAFETY: `request` is a live buffer and `out_len` is a live local.
        let response = unsafe {
            re_editor_highlighter_spans(highlighter, request.as_ptr(), request.len(), &mut out_len)
        };
        assert!(
            response.is_null(),
            "the released highlighter answers nothing"
        );
        assert_eq!(out_len, 0);
    }
}
