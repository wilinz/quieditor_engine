//! A grammar, as the Dart side sends it.
//!
//! The 196 language definitions live in `re_highlight` as Dart data. They are
//! *data* — patterns, keywords, nesting — so they travel as JSON rather than
//! being rewritten here, which keeps one copy of each grammar and means a
//! language added upstream arrives without anything being ported.
//!
//! What does not travel is the five callbacks a handful of grammars use. They
//! are Dart closures, so the Dart side names them and [`Callback`] is the
//! Rust half of that mapping.
//!
//! # What arrives, and in what shape
//!
//! Every field highlight.js allows to be "a string or a list of strings" keeps
//! its two shapes here as well, because they are not interchangeable: a *list*
//! is a multi-part pattern whose parts are separate match groups, which is how
//! `beginScope: {1: 'a', 2: 'b'}` addresses them, and the compiler rejects a
//! list without such a map. Flattening both to a list would make the check
//! unfireable and the two indistinguishable.
//!
//! `scope`, `beginScope` and `endScope` keep their shapes for the same reason:
//! a string wraps the whole match in one scope, a map wraps individual groups.

use crate::highlight::regex::Dialect;
use serde::Deserialize;
use std::collections::HashMap;

/// A callback a grammar asks for by name.
///
/// Only five exist in the whole of `re_highlight`, and all five are
/// highlight.js's own helpers rather than anything a grammar author wrote. They
/// are listed here rather than passed as code because the alternative is a
/// round trip into Dart for every match a shebang rule considers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Callback {
    /// Stores the text the mode began with, so the end can be checked against
    /// it — `endSameAsBegin` in the grammar, which is how heredocs and
    /// here-strings work.
    #[serde(rename = "SAME_AS_BEGIN")]
    SameAsBegin,
    /// The other half: ignores an end that does not match what was begun.
    #[serde(rename = "SAME_AS_END")]
    SameAsEnd,
    /// The same, for a grammar whose begin captures its marker in either of two
    /// groups — PHP writes `match[1] ?? match[2]`, because one of its heredoc
    /// forms has the marker as the second group. The end is checked by
    /// [`SameAsEnd`], which compares the first group either way.
    #[serde(rename = "SAME_AS_BEGIN_SECOND")]
    SameAsBeginSecond,
    /// Ignores the match unless it is at the very start of the document.
    #[serde(rename = "SHEBANG")]
    Shebang,
    /// The JSX-or-type-arguments guess: `<T, A>` is a type parameter list, and
    /// `<div>` is an element. Decided by the character after the match.
    #[serde(rename = "JSX_OR_GENERIC")]
    JsxOrGeneric,
    /// Ignores the match unless it names a Mathematica system symbol.
    #[serde(rename = "MATHEMATICA_SYMBOL")]
    MathematicaSymbol,
    /// Ignores the match when the character before it is a dot, so that
    /// `obj.keyword` does not match a `beginKeywords` rule.
    ///
    /// Not one a grammar asks for: the compiler adds it to every mode with
    /// `beginKeywords`, which is the only way highlight.js has of expressing
    /// "this word, but not as a property access".
    #[serde(skip)]
    SkipIfPrecededByDot,
}

/// A pattern: one, or several whose groups are addressed by a scope map.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum Patterns {
    Single(String),
    /// Several alternatives, each its own match group.
    Many(Vec<String>),
}

impl Patterns {
    /// The alternatives, whether they were written as one or many.
    pub fn parts(&self) -> &[String] {
        match self {
            Patterns::Single(one) => std::slice::from_ref(one),
            Patterns::Many(many) => many,
        }
    }

    /// Whether the grammar wrote this as a multi-part pattern.
    ///
    /// Not the same as "has more than one part": the distinction is what the
    /// compiler keys its validity checks on.
    pub fn is_multi(&self) -> bool {
        matches!(self, Patterns::Many(_))
    }
}

/// A scope name, or one per match group.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ScopeSpec {
    /// One scope for the whole match.
    Single(String),
    /// A scope per group, by group number.
    PerGroup(HashMap<String, String>),
}

/// A sub-language, or a set of them to be detected among.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum SubLanguage {
    Named(String),
    Candidate(Vec<String>),
}

/// Raw keywords, in any of the three shapes highlight.js accepts.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum RawKeywords {
    /// `"for if then"`.
    Text(String),
    /// `["for", "if"]`.
    List(Vec<String>),
    /// `{"keyword": ..., "built_in": ...}`, nested to any depth.
    Scoped(HashMap<String, RawKeywords>),
}

/// A grammar node, as it arrives.
///
/// Field names match the JSON the Dart side writes, which in turn match the
/// Dart field names — so a grammar can be diffed against `re_highlight` by
/// reading the two side by side, which is how the port was checked.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ModeSpec {
    pub name: Option<String>,
    pub case_insensitive: bool,
    pub disable_autodetect: bool,
    pub superset_of: Option<String>,
    pub aliases: Vec<String>,
    pub class_name_aliases: HashMap<String, String>,

    /// The patterns a mode begins with. A list is a multi-part pattern.
    pub begin: Option<Patterns>,
    /// The patterns it ends with, in the same shape.
    pub end: Option<Patterns>,
    /// `match` is `begin` with no `end`; the compiler rewrites one into the
    /// other.
    pub r#match: Option<Patterns>,
    /// Patterns that are never legal in this mode, or `true` for "nothing is".
    pub illegal: Option<IllegalSpec>,
    /// Requires the text before the match to be present, without consuming it.
    pub before_match: Option<String>,

    pub scope: Option<ScopeSpec>,
    pub begin_scope: Option<ScopeSpec>,
    pub end_scope: Option<ScopeSpec>,

    pub contains: Vec<ModeSpec>,
    pub variants: Vec<ModeSpec>,
    pub starts: Option<Box<ModeSpec>>,
    pub sub_language: Option<SubLanguage>,

    pub keywords: Option<RawKeywords>,
    pub begin_keywords: Option<String>,
    pub lexemes: Option<String>,
    pub relevance: Option<f64>,
    pub label: Option<String>,

    pub ends_parent: bool,
    pub ends_with_parent: bool,
    pub end_same_as_begin: bool,
    pub exclude_begin: bool,
    pub exclude_end: bool,
    pub return_begin: bool,
    pub return_end: bool,
    pub skip: bool,
    /// "Reuse the enclosing mode here", for grammars that nest themselves.
    pub self_referential: bool,

    pub on_begin: Option<Callback>,
    pub on_end: Option<Callback>,
    pub before_begin: Option<Callback>,

    /// A reference to `refs` on the language root, resolved when compiling.
    pub r#ref: Option<String>,
}

/// `illegal` is either a set of patterns or the literal `true`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum IllegalSpec {
    Patterns(Patterns),
    /// `true` means "nothing here is legal", which highlight.js turns into a
    /// pattern that matches anything.
    Always(bool),
}

/// A grammar as it arrives, with the shared modes it refers to.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct LanguageSpec {
    pub name: Option<String>,
    pub case_insensitive: bool,
    /// `unicodeRegex`: whether the language's patterns are read as Unicode
    /// patterns. The Dart side reads this flag from the language and nowhere
    /// else, and so does this — a mode that sets it is ignored, exactly as it is
    /// there.
    pub unicode_regex: bool,
    pub disable_autodetect: bool,
    pub aliases: Vec<String>,
    pub class_name_aliases: HashMap<String, String>,
    pub superset_of: Option<String>,
    pub contains: Vec<ModeSpec>,
    pub keywords: Option<RawKeywords>,
    /// The modes `ref: "..."` points at. highlight.js hoists a grammar's shared
    /// sub-modes here so a definition can be written once and used in many
    /// places without being duplicated in the source.
    pub refs: HashMap<String, ModeSpec>,
}

impl LanguageSpec {
    /// The flags this language's patterns are read in.
    pub fn dialect(&self) -> Dialect {
        Dialect {
            case_insensitive: self.case_insensitive,
            unicode: self.unicode_regex,
        }
    }

    /// Reads a grammar from the JSON the Dart side writes.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minimal_grammar_parses() {
        let language = LanguageSpec::from_json(
            r#"{"name": "Test", "caseInsensitive": true, "contains": [{"begin": ["a"]}]}"#,
        )
        .expect("parses");
        assert_eq!(language.name.as_deref(), Some("Test"));
        assert!(language.case_insensitive);
        assert_eq!(language.contains.len(), 1);
        assert_eq!(
            language.contains[0].begin,
            Some(Patterns::Many(vec!["a".to_string()]))
        );
    }

    #[test]
    fn absent_fields_take_their_defaults() {
        // The Dart side omits nulls rather than writing them, so every field
        // has to tolerate being missing.
        let language = LanguageSpec::from_json("{}").expect("parses");
        assert!(language.contains.is_empty());
        assert!(language.refs.is_empty());
        let mode = ModeSpec::default();
        assert!(mode.begin.is_none());
        assert!(!mode.ends_with_parent);
    }

    #[test]
    fn keywords_parse_in_all_three_shapes() {
        let text: ModeSpec =
            serde_json::from_str(r#"{"keywords": "for if then"}"#).expect("parses");
        assert!(matches!(text.keywords, Some(RawKeywords::Text(_))));

        let list: ModeSpec =
            serde_json::from_str(r#"{"keywords": ["for", "if"]}"#).expect("parses");
        assert!(matches!(list.keywords, Some(RawKeywords::List(_))));

        let scoped: ModeSpec = serde_json::from_str(
            r#"{"keywords": {"keyword": "for if", "built_in": {"nested": "x"}}}"#,
        )
        .expect("parses");
        match scoped.keywords {
            Some(RawKeywords::Scoped(map)) => {
                assert!(matches!(map.get("keyword"), Some(RawKeywords::Text(_))));
                assert!(matches!(map.get("built_in"), Some(RawKeywords::Scoped(_))));
            }
            other => panic!("expected scoped keywords, got {other:?}"),
        }
    }

    #[test]
    fn scope_parses_as_one_name_or_one_per_group() {
        let single: ModeSpec = serde_json::from_str(r#"{"scope": "string"}"#).expect("parses");
        assert_eq!(single.scope, Some(ScopeSpec::Single("string".to_string())));

        let per_group: ModeSpec =
            serde_json::from_str(r#"{"beginScope": {"1": "keyword", "2": "title"}}"#)
                .expect("parses");
        match per_group.begin_scope {
            Some(ScopeSpec::PerGroup(map)) => {
                assert_eq!(map.get("1").map(String::as_str), Some("keyword"));
                assert_eq!(map.get("2").map(String::as_str), Some("title"));
            }
            other => panic!("expected per-group scopes, got {other:?}"),
        }
    }

    #[test]
    fn illegal_parses_as_patterns_or_as_true() {
        let patterns: ModeSpec =
            serde_json::from_str(r#"{"illegal": ["a", "b"]}"#).expect("parses");
        assert_eq!(
            patterns.illegal,
            Some(IllegalSpec::Patterns(Patterns::Many(vec![
                "a".to_string(),
                "b".to_string()
            ])))
        );
        let always: ModeSpec = serde_json::from_str(r#"{"illegal": true}"#).expect("parses");
        assert_eq!(always.illegal, Some(IllegalSpec::Always(true)));
    }

    #[test]
    fn a_callback_arrives_by_name() {
        let mode: ModeSpec =
            serde_json::from_str(r#"{"onBegin": "SHEBANG", "onEnd": "SAME_AS_END"}"#)
                .expect("parses");
        assert_eq!(mode.on_begin, Some(Callback::Shebang));
        assert_eq!(mode.on_end, Some(Callback::SameAsEnd));
        // An unknown name is refused rather than silently dropped: a grammar
        // that asks for behaviour this does not have should say so.
        assert!(serde_json::from_str::<ModeSpec>(r#"{"onBegin": "NOT_A_CALLBACK"}"#).is_err());
    }

    #[test]
    fn a_sub_language_parses_either_way() {
        let named: ModeSpec = serde_json::from_str(r#"{"subLanguage": "xml"}"#).expect("parses");
        assert_eq!(
            named.sub_language,
            Some(SubLanguage::Named("xml".to_string()))
        );
        let candidates: ModeSpec =
            serde_json::from_str(r#"{"subLanguage": ["xml", "html"]}"#).expect("parses");
        assert_eq!(
            candidates.sub_language,
            Some(SubLanguage::Candidate(vec![
                "xml".to_string(),
                "html".to_string()
            ]))
        );
    }

    #[test]
    fn references_and_their_table_parse() {
        let language = LanguageSpec::from_json(
            r#"{"contains": [{"ref": "~shared~"}], "refs": {"~shared~": {"begin": ["x"]}}}"#,
        )
        .expect("parses");
        assert_eq!(language.contains[0].r#ref.as_deref(), Some("~shared~"));
        assert!(language.refs.contains_key("~shared~"));
    }

    #[test]
    fn a_grammar_that_does_not_parse_is_an_error_not_a_panic() {
        assert!(LanguageSpec::from_json("{").is_err());
        assert!(LanguageSpec::from_json(r#"{"contains": "not a list"}"#).is_err());
    }
}
