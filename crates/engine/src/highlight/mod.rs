//! A port of highlight.js: the engine behind the editor's syntax highlighting.
//!
//! The editor's highlighting came from `re_highlight`, a Dart port of
//! highlight.js. Measured on a 200,000-line file in its own language it took
//! **143 seconds** for one pass — and a pass is run on every change, because a
//! grammar's state carries from one line to the next. That is not a constant
//! factor to be shaved; it is the reason the editor stalls while typing.
//!
//! Porting it here is what makes the fix possible at all. The Dart port keeps
//! its per-line state in private fields, so nothing outside it can resume
//! mid-document; this owns that state, so re-highlighting can start from the
//! last line whose state is known and stop as soon as the state converges
//! again — which, while someone types, is the line they are on.
//!
//! # Shape
//!
//! * [`regex`] — the combined-regex matcher the engine is built on.
//! * [`mode`] — a grammar, as the JSON the Dart side sends.
//!
//! The rest is a faithful port of highlight.js, and faithfulness is the point:
//! the class names a grammar produces are what every theme in the ecosystem
//! keys on, so a difference here is not a bug in this code but a different set
//! of colours in every editor that uses it.

pub mod compiler;
pub mod engine;
pub mod incremental;
pub mod keywords;
pub mod mode;
pub mod regex;

pub use compiler::{compile, CompileError, Grammar, Mode, ModeId, Rule};
pub use engine::{Element, Engine, Highlighted, Node, State};
pub use incremental::{Incremental, Span, Update};

use std::collections::HashMap;

/// Compiles a grammar from the JSON the Dart side sends.
pub fn compile_json(json: &str) -> Result<Grammar, CompileError> {
    let spec = mode::LanguageSpec::from_json(json)
        .map_err(|_| CompileError::BadPattern("grammar JSON".to_string()))?;
    compile(&spec)
}

/// Highlights `code` with `grammar`.
///
/// `sub_grammars` holds the grammars a `subLanguage` rule refers to, by name.
/// A grammar that embeds another — Markdown with HTML, or a template language
/// with JSON — needs them; a rule whose sub-language is missing shows its text
/// plain rather than dropping it.
pub fn highlight(
    grammar: &Grammar,
    sub_grammars: &HashMap<String, Grammar>,
    code: &str,
) -> Highlighted {
    Engine::highlight(grammar, sub_grammars, code)
}

/// The scopes a piece of code was highlighted with, for tests and diagnostics.
///
/// Walks the tree and reports every node that carries a scope, which is what a
/// theme keys on and therefore what has to match between this and the Dart
/// implementation.
pub fn scopes(node: &Node, into: &mut Vec<(String, String)>) {
    for child in &node.children {
        if let Element::Node(child) = child {
            if let Some(scope) = &child.scope {
                into.push((scope.clone(), child.text()));
            }
            scopes(child, into);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes_of(grammar: &str, code: &str) -> Vec<(String, String)> {
        let compiled = compile_json(grammar).expect("the grammar compiles");
        let empty = HashMap::new();
        let highlighted = highlight(&compiled, &empty, code);
        assert_eq!(
            highlighted.root.text(),
            code,
            "highlighting changed the text, which is always a bug"
        );
        let mut scopes = Vec::new();
        super::scopes(&highlighted.root, &mut scopes);
        scopes
    }

    const KEYWORDS_AND_STRINGS: &str = r#"{
        "name": "Test",
        "keywords": {"keyword": "for if then"},
        "contains": [
            {"scope": "string", "begin": "\"", "end": "\""},
            {"scope": "comment", "begin": "//", "end": "$"}
        ]
    }"#;

    #[test]
    fn a_lone_keyword_is_coloured() {
        assert_eq!(
            scopes_of(KEYWORDS_AND_STRINGS, "for"),
            vec![("keyword".to_string(), "for".to_string())]
        );
    }

    #[test]
    fn a_word_that_is_not_a_keyword_is_left_plain() {
        assert_eq!(scopes_of(KEYWORDS_AND_STRINGS, "foreach"), Vec::new());
    }

    #[test]
    fn keywords_and_their_surroundings_keep_their_order() {
        // The text between keywords has to come out uncoloured and in place,
        // which is the thing a keyword pass gets wrong when it collects rather
        // than emits as it walks.
        assert_eq!(
            scopes_of(KEYWORDS_AND_STRINGS, "if x then"),
            vec![
                ("keyword".to_string(), "if".to_string()),
                ("keyword".to_string(), "then".to_string()),
            ]
        );
    }

    #[test]
    fn a_sub_mode_colours_what_it_covers() {
        assert_eq!(
            scopes_of(KEYWORDS_AND_STRINGS, "x = \"abc\""),
            vec![("string".to_string(), "\"abc\"".to_string())]
        );
    }

    #[test]
    fn a_mode_ends_where_its_end_says() {
        assert_eq!(
            scopes_of(KEYWORDS_AND_STRINGS, "a // note\nb"),
            vec![("comment".to_string(), "// note".to_string())]
        );
    }

    #[test]
    fn a_keyword_inside_a_string_stays_a_string() {
        // The string mode has no keywords, so `for` inside it must not be
        // coloured — the mode's keywords are the ones that apply.
        assert_eq!(
            scopes_of(KEYWORDS_AND_STRINGS, "\"for\""),
            vec![("string".to_string(), "\"for\"".to_string())]
        );
    }

    const NESTING: &str = r#"{
        "name": "Nesting",
        "contains": [
            {
                "scope": "string",
                "begin": "\"",
                "end": "\"",
                "contains": [{"scope": "escape", "begin": "\\\\."}]
            }
        ]
    }"#;

    #[test]
    fn a_sub_mode_can_contain_another() {
        assert_eq!(
            scopes_of(NESTING, "\"a\\nb\""),
            // Outermost first: a node's scope is reported before the scopes of
            // the nodes nested inside it, matching how the Dart renderer
            // qualifies an inner scope by its parent.
            vec![
                ("string".to_string(), "\"a\\nb\"".to_string()),
                ("escape".to_string(), "\\n".to_string()),
            ]
        );
    }

    /// A rule that colours some of its parts and not others, which is how
    /// highlight.js writes `def`, `class` and the like: the parts together are
    /// the match, and the scope map says which of them are coloured.
    const GROUP_SCOPE: &str = r#"{
        "name": "GroupScope",
        "contains": [
            {
                "match": ["def", "\\s+", "([a-zA-Z_]\\w*)"],
                "scope": {"1": "keyword", "3": "title.function"}
            }
        ]
    }"#;

    #[test]
    fn a_scope_map_colours_the_parts_it_names_and_keeps_the_rest() {
        // The space is neither part: the rule colours `def` and `hello` and
        // says nothing about what is between them, so it comes out as text. It
        // used to come out as nothing at all, along with the two coloured
        // parts, which is a document that changes under the cursor.
        assert_eq!(
            scopes_of(GROUP_SCOPE, "def hello"),
            vec![
                ("keyword".to_string(), "def".to_string()),
                ("title.function".to_string(), "hello".to_string()),
            ]
        );
    }

    /// A language asking for Unicode patterns, and the same language without
    /// the flag — Python, XML and Haskell are the three shipped ones that ask.
    ///
    /// The pattern is a Unicode property escape, which no pattern can use
    /// unless the flag is set: read without it the escape is not an escape, and
    /// the rule matches nothing. That is what makes the pair below a test
    /// rather than an assertion that passes either way.
    const UNICODE_REGEX: &str = r#"{
        "name": "Unicode",
        "unicodeRegex": true,
        "contains": [{"scope": "letter", "begin": "\\p{L}+"}]
    }"#;

    const PLAIN_REGEX: &str = r#"{
        "name": "Plain",
        "unicodeRegex": false,
        "contains": [{"scope": "letter", "begin": "\\p{L}+"}]
    }"#;

    #[test]
    fn a_language_asking_for_unicode_patterns_gets_them() {
        assert_eq!(
            scopes_of(UNICODE_REGEX, "abc 123"),
            vec![("letter".to_string(), "abc".to_string())]
        );
        assert_eq!(
            scopes_of(PLAIN_REGEX, "abc 123"),
            Vec::new(),
            "without the flag the same pattern matches nothing, which is what \
             the assertion above is measuring"
        );
    }

    /// A mode that contains itself, which is how a grammar says "and anything
    /// nested inside this is one of these too". Bash writes its `${...}`
    /// substitution this way, and so do several others.
    const SELF_NESTING: &str = r#"{
        "name": "SelfNesting",
        "contains": [
            {
                "scope": "subst",
                "begin": "\\$\\{",
                "end": "\\}",
                "contains": [{"selfReferential": true}]
            }
        ]
    }"#;

    #[test]
    fn a_mode_can_contain_itself() {
        // The nesting is in the text rather than the grammar, so the compiled
        // mode is a graph with a cycle in it. Compiling one used to descend into
        // the same spec again and again — a stack overflow on a real grammar,
        // and the depth limit on a small one.
        assert_eq!(
            scopes_of(SELF_NESTING, "${a${b}c}"),
            vec![
                ("subst".to_string(), "${a${b}c}".to_string()),
                ("subst".to_string(), "${b}".to_string()),
            ]
        );
    }

    const SAME_AS_BEGIN: &str = r#"{
        "name": "Heredoc",
        "caseInsensitive": false,
        "contains": [
            {
                "scope": "string",
                "begin": "<<(\\w+)",
                "end": "(\\w+)",
                "onBegin": "SAME_AS_BEGIN",
                "onEnd": "SAME_AS_END"
            }
        ]
    }"#;

    #[test]
    fn a_heredoc_only_ends_on_its_own_marker() {
        // `endSameAsBegin` in a grammar is these two callbacks: one remembers
        // the word the mode began with, the other refuses an end that is not
        // that word. The end pattern is a bare word, so the callback is the
        // whole of what makes it the marker: `not` is refused, and the first
        // `EOF` ends the mode — leaving everything after it uncoloured, which
        // is what tells this apart from a mode that never ends at all.
        let scopes = scopes_of(SAME_AS_BEGIN, "<<EOF\nnot EOF\nEOF done\nEOF");
        assert_eq!(
            scopes,
            vec![("string".to_string(), "<<EOF\nnot EOF".to_string())]
        );
    }

    #[test]
    fn a_grammar_that_does_not_parse_is_an_error() {
        assert!(compile_json("{").is_err());
    }

    #[test]
    fn a_ref_to_a_missing_mode_is_an_error() {
        assert!(compile_json(r#"{"contains": [{"ref": "missing"}]}"#).is_err());
    }

    #[test]
    fn a_ref_uses_the_mode_it_names() {
        // The grammar writes the sub-mode once and refers to it, which is how
        // the real ones avoid repeating themselves.
        let scopes = scopes_of(
            r#"{
                "contains": [{"ref": "~str~"}],
                "refs": {"~str~": {"scope": "string", "begin": "\"", "end": "\""}}
            }"#,
            "\"x\"",
        );
        assert_eq!(scopes, vec![("string".to_string(), "\"x\"".to_string())]);
    }
}
