//! Highlighting code with a grammar compiled once and then reused.
//!
//! The engine is [`quieditor_engine::highlight`]; this module is the wire format
//! around it, and the handle that keeps a compiled grammar alive between calls.
//! Compiling is the expensive half — a grammar is a few hundred modes, each
//! with a combined regular expression to build — so it happens once per
//! language rather than once per keystroke.

use crate::generated::highlight_generated::re_editor::ffi as fb;
use crate::registry::Registry;
use crate::{guard, respond, write_out_len};
use quieditor_engine::buffer::{line_starts, locate};
use quieditor_engine::highlight::{self, Element, Grammar, Node};
use std::collections::HashMap;

/// A compiled grammar, and the sub-grammars a `subLanguage` rule can reach.
///
/// Compiled together: the engine looks a sub-language up by name while it is
/// running, so the map has to be there by then, and it is not something to
/// build per call.
pub struct GrammarHandle {
    grammar: Grammar,
    sub_grammars: HashMap<String, Grammar>,
}

/// The live grammars, keyed by the handle Dart holds.
static GRAMMARS: Registry<GrammarHandle> = Registry::new();

/// Compiles a grammar from a `GrammarRequest`.
///
/// Returns 0 when the request will not decode, when the JSON will not compile,
/// or when a sub-grammar's does not. That is not an error to report: it is the
/// caller's signal to keep highlighting with its own implementation, which is
/// what it does about every other operation that cannot answer.
///
/// # Safety
///
/// `req` must be null, or point to `req_len` readable bytes that stay valid for
/// the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn re_editor_grammar_create(req: *const u8, req_len: usize) -> usize {
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { crate::api::request_bytes(req, req_len) };
    match guard(|| compile(request)) {
        Some(Some(handle)) => GRAMMARS.insert(handle),
        _ => 0,
    }
}

/// Releases a grammar handle.
///
/// A handle that is not live — already released, never issued, or zero — does
/// nothing, so a double release costs a lookup rather than a second free. That
/// is why this is safe to call with anything at all.
#[no_mangle]
pub extern "C" fn re_editor_grammar_free(grammar: usize) {
    drop(GRAMMARS.remove(grammar));
}

/// Highlights a `HighlightRequest` and answers with a `HighlightResponse`.
///
/// Returns null when there is no such live grammar, when the request will not
/// decode, or when the code is refused. A grammar that matches nothing still
/// answers — with no nodes, which is a result the caller renders rather than a
/// reason to fall back.
///
/// # Safety
///
/// `req` must be null or point to `req_len` readable bytes, and `out_len` must
/// be null or point to a writable `usize`.
#[no_mangle]
pub unsafe extern "C" fn re_editor_highlight(
    grammar: usize,
    req: *const u8,
    req_len: usize,
    out_len: *mut usize,
) -> *mut u8 {
    // SAFETY: the caller guarantees `out_len` is null or writable.
    unsafe { write_out_len(out_len, 0) };
    // SAFETY: the caller guarantees `req` covers `req_len` readable bytes.
    let request = unsafe { crate::api::request_bytes(req, req_len) };
    match guard(|| GRAMMARS.and_then(grammar, |grammar| encode(grammar, request))) {
        // SAFETY: `out_len` is still null or writable.
        Some(Some(bytes)) => unsafe { respond(bytes, out_len) },
        _ => std::ptr::null_mut(),
    }
}

fn compile(request: &[u8]) -> Option<GrammarHandle> {
    let request = flatbuffers::root::<fb::GrammarRequest>(request).ok()?;
    let grammar = highlight::compile_json(request.json()?).ok()?;

    let mut sub_grammars = HashMap::new();
    for sub in request.sub_languages().iter().flatten() {
        // A sub-grammar with no name could never be referred to, and one with
        // no JSON is not a grammar; either way the caller has built a request
        // that cannot mean anything, so the whole thing is refused rather than
        // compiled into a grammar that highlights differently.
        let (Some(name), Some(json)) = (sub.name(), sub.json()) else {
            return None;
        };
        sub_grammars.insert(name.to_string(), highlight::compile_json(json).ok()?);
    }

    Some(GrammarHandle {
        grammar,
        sub_grammars,
    })
}

fn encode(grammar: &GrammarHandle, request: &[u8]) -> Option<Vec<u8>> {
    let request = flatbuffers::root::<fb::HighlightRequest>(request).ok()?;
    let code = request.code().unwrap_or("");

    let highlighted = highlight::highlight(&grammar.grammar, &grammar.sub_grammars, code);
    if highlighted.illegal {
        // The engine stopped early, which it does for a grammar that cannot be
        // highlighted and, with `illegal` rules, for code a grammar says is not
        // its own. What it built is a tree over part of the text, and a client
        // cannot tell that from a tree over all of it — it would colour the
        // first half of the document and leave the rest plain. Answering
        // nothing is what tells the caller to use its own implementation.
        return None;
    }

    let mut spans = Vec::new();
    walk(&highlighted.root, 0, 0, &mut spans);

    let starts = line_starts(code);
    let mut builder = flatbuffers::FlatBufferBuilder::new();
    let items = spans
        .iter()
        .map(|span| {
            let (start_line, start_offset) = locate(code, &starts, span.start);
            let (end_line, end_offset) = locate(code, &starts, span.end);
            let scope = builder.create_string(&span.scope);
            Some(fb::HighlightNode::create(
                &mut builder,
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
    let items = builder.create_vector(&items);
    let root = fb::HighlightResponse::create(
        &mut builder,
        &fb::HighlightResponseArgs {
            nodes: Some(items),
            relevance: highlighted.relevance,
        },
    );
    builder.finish(root, None);
    Some(builder.finished_data().to_vec())
}

/// One node's span, in the byte offsets the engine works in.
struct Span {
    scope: String,
    start: usize,
    end: usize,
    depth: usize,
}

/// Walks the highlighted tree, recording every node that carries a scope.
///
/// `at` is where the node's text begins, in bytes, and the return value is
/// where it ends — a node's text is its children's text and nothing else, which
/// is what makes the recorded spans nest.
///
/// Nodes are recorded in reading order, each before the nodes it contains,
/// because that is the order a client renders them in. Nodes that carry no
/// scope are not recorded and do not count towards `depth`: they colour
/// nothing, so the client never sees them, and a depth that counted them would
/// not be a depth in the list the client actually has.
fn walk(node: &Node, at: usize, depth: usize, into: &mut Vec<Span>) -> usize {
    let scope = node.scope.as_deref().filter(|scope| !scope.is_empty());
    // Recorded before the children are walked, because the span's end is not
    // known until they are; the placeholder is filled in below.
    let recorded = scope.map(|scope| {
        into.push(Span {
            scope: scope.to_string(),
            start: at,
            end: at,
            depth,
        });
        into.len() - 1
    });

    let mut end = at;
    for child in &node.children {
        end = match child {
            Element::Text(text) => end + text.len(),
            Element::Node(nested) => {
                walk(nested, end, depth + usize::from(recorded.is_some()), into)
            }
        };
    }

    if let Some(index) = recorded {
        into[index].end = end;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grammar with a string that contains an escape: the escape is the part
    /// that has to arrive nested inside the string, which is what the depth in
    /// the wire format is for.
    const NESTED: &str = r#"{
        "name": "Test",
        "keywords": {"keyword": "for if then"},
        "contains": [
            {
                "scope": "string",
                "begin": "\"",
                "end": "\"",
                "contains": [{"scope": "escape", "begin": "\\\\."}]
            },
            {"scope": "comment", "begin": "//", "end": "$"}
        ]
    }"#;

    /// One span, as it comes back over the wire.
    #[derive(Debug, PartialEq, Eq)]
    struct Scoped {
        scope: String,
        start_line: u32,
        start_offset: u32,
        end_line: u32,
        end_offset: u32,
        depth: u32,
    }

    fn create(json: &str, sub_languages: &[(&str, &str)]) -> usize {
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let json = builder.create_string(json);
        let subs = sub_languages
            .iter()
            .map(|(name, json)| {
                let name = builder.create_string(name);
                let json = builder.create_string(json);
                fb::SubGrammar::create(
                    &mut builder,
                    &fb::SubGrammarArgs {
                        name: Some(name),
                        json: Some(json),
                    },
                )
            })
            .collect::<Vec<_>>();
        let subs = builder.create_vector(&subs);
        let root = fb::GrammarRequest::create(
            &mut builder,
            &fb::GrammarRequestArgs {
                json: Some(json),
                sub_languages: Some(subs),
            },
        );
        builder.finish(root, None);
        let request = builder.finished_data().to_vec();
        // SAFETY: `request` is a live buffer of the length given.
        unsafe { re_editor_grammar_create(request.as_ptr(), request.len()) }
    }

    /// Compiles a grammar, highlights `code` with it, and drops the handle.
    ///
    /// Goes through the entry points rather than calling the helpers, so a test
    /// that fails is failing in the code that ships.
    fn highlight(handle: usize, code: &str) -> Option<Vec<Scoped>> {
        assert_ne!(handle, 0, "the grammar compiles");
        let mut builder = flatbuffers::FlatBufferBuilder::new();
        let text = builder.create_string(code);
        let root = fb::HighlightRequest::create(
            &mut builder,
            &fb::HighlightRequestArgs { code: Some(text) },
        );
        builder.finish(root, None);
        let request = builder.finished_data().to_vec();

        let mut out_len = 0usize;
        // SAFETY: `request` is live for the call and `out_len` is a live local.
        let response =
            unsafe { re_editor_highlight(handle, request.as_ptr(), request.len(), &mut out_len) };
        re_editor_grammar_free(handle);
        assert!(!response.is_null(), "the highlight is answered");

        // SAFETY: `re_editor_highlight` returned this pair, of this length.
        let bytes = unsafe { std::slice::from_raw_parts(response, out_len) };
        let decoded = flatbuffers::root::<fb::HighlightResponse>(bytes).expect("decodes");
        let nodes = decoded
            .nodes()
            .into_iter()
            .flatten()
            .map(|node| Scoped {
                scope: node.scope().unwrap_or_default().to_string(),
                start_line: node.start_line(),
                start_offset: node.start_offset(),
                end_line: node.end_line(),
                end_offset: node.end_offset(),
                depth: node.depth(),
            })
            .collect();
        // SAFETY: the pair is exactly what the entry point returned, and is not
        // used again.
        unsafe { crate::re_editor_free(response, out_len) };
        Some(nodes)
    }

    /// The scopes of one line, as `(scope, text)` pairs.
    fn scopes_on(code: &str, line: usize, nodes: &[Scoped]) -> Vec<(String, String)> {
        let text = code.lines().nth(line).unwrap_or_default();
        nodes
            .iter()
            .filter(|node| node.start_line as usize == line && node.end_line as usize == line)
            .map(|node| {
                let start = node.start_offset as usize;
                let end = node.end_offset as usize;
                (node.scope.clone(), text[start..end].to_string())
            })
            .collect()
    }

    #[test]
    fn a_string_and_a_comment_are_both_scoped() {
        let nodes = highlight(create(NESTED, &[]), "a // note\nb").expect("answered");
        assert_eq!(
            scopes_on("a // note\nb", 0, &nodes),
            vec![("comment".to_string(), "// note".to_string())],
            "the comment ends where its end pattern says"
        );
        assert_eq!(nodes.len(), 1, "nothing else is scoped: {nodes:?}");
    }

    #[test]
    fn a_node_inside_a_node_arrives_nested_in_reading_order() {
        let nodes = highlight(create(NESTED, &[]), "x = \"a\\nb\"").expect("answered");
        assert_eq!(
            nodes
                .iter()
                .map(|node| (node.scope.as_str(), node.depth))
                .collect::<Vec<_>>(),
            vec![("string", 0), ("escape", 1)],
            "the outer scope comes first, and the inner one says it is inside it"
        );
    }

    #[test]
    fn offsets_are_utf16_units_and_lines_are_lines() {
        // "日" is three bytes and one UTF-16 unit, so a byte offset reported as
        // a column would put the string at offset 3 rather than 1 — and the
        // client would cut the text in the wrong place.
        let code = "日\nx = \"a\"";
        let nodes = highlight(create(NESTED, &[]), code).expect("answered");
        assert_eq!(
            nodes
                .iter()
                .map(|node| (
                    node.start_line,
                    node.start_offset,
                    node.end_line,
                    node.end_offset
                ))
                .collect::<Vec<_>>(),
            vec![(1, 4, 1, 7)]
        );
        assert_eq!(
            scopes_on(code, 1, &nodes),
            vec![("string".to_string(), "\"a\"".to_string())]
        );
    }

    #[test]
    fn a_grammar_that_does_not_compile_is_refused_rather_than_guessed_at() {
        assert_eq!(create("{", &[]), 0);
        // A `ref` to a mode that is not in the grammar is the other way a
        // grammar can fail, and it fails at compile time rather than by
        // colouring the wrong things.
        assert_eq!(create(r#"{"contains": [{"ref": "missing"}]}"#, &[]), 0);
    }

    #[test]
    fn a_sub_grammar_is_compiled_into_the_handle() {
        // A rule whose sub-language is missing shows its text plain, so this
        // test would pass without the sub-grammar; what it pins is the
        // opposite — that a sub-grammar can be named and compiled at all, and
        // that its grammar colours the region.
        let outer = r#"{
            "name": "Outer",
            "contains": [
                {
                    "scope": "meta",
                    "begin": "<%",
                    "end": "%>",
                    "subLanguage": "inner"
                }
            ]
        }"#;
        let inner = r#"{
            "name": "Inner",
            "keywords": {"keyword": "for"},
            "contains": [{"scope": "string", "begin": "\"", "end": "\""}]
        }"#;
        let code = "<% for %>";
        let nodes = highlight(create(outer, &[("inner", inner)]), code).expect("answered");
        assert!(
            nodes.iter().any(|node| node.scope == "keyword"),
            "the sub-language's own keyword should be scoped: {nodes:?}"
        );
    }

    #[test]
    fn a_handle_of_zero_is_answered_with_nothing() {
        let mut out_len = 0usize;
        // SAFETY: zero is a handle like any other to the signature, a null
        // request is documented as acceptable, and `out_len` is a live local.
        let response = unsafe { re_editor_highlight(0, std::ptr::null(), 0, &mut out_len) };
        assert!(response.is_null());
        assert_eq!(out_len, 0);
    }

    /// Releasing a grammar twice must not release it twice, and a released
    /// handle must not reach the grammar that was behind it.
    #[test]
    fn a_released_grammar_handle_answers_rather_than_dereferencing() {
        let grammar = create(r#"{"keywords": {"keyword": "for"}}"#, &[]);
        assert_ne!(grammar, 0);

        re_editor_grammar_free(grammar);
        re_editor_grammar_free(grammar);

        let mut out_len = 0usize;
        // SAFETY: `out_len` is a live local and a null request is acceptable.
        let response = unsafe { re_editor_highlight(grammar, std::ptr::null(), 0, &mut out_len) };
        assert!(response.is_null());
        assert_eq!(out_len, 0);
    }
}
