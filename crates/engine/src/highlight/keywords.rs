//! Turning a grammar's keyword lists into a lookup.
//!
//! A grammar writes keywords in whatever shape is convenient — a space-separated
//! string, a list, or a map from scope name to more of the same — and some carry
//! an explicit relevance after a `|`. All of that flattens to one map from word
//! to how it should be coloured.
//!
//! The one subtlety is [`COMMON_KEYWORDS`]: words so ordinary in prose that
//! counting them as evidence of a language would make auto-detection useless.
//! They are still highlighted, but they do not add to a file's relevance score.

use crate::highlight::mode::RawKeywords;
use std::collections::HashMap;

/// The scope a keyword gets when its group does not name one.
const DEFAULT_SCOPE: &str = "keyword";

/// Words that are highlighted but do not count towards a language's relevance.
///
/// These are names that turn up in ordinary prose and in most programming
/// languages, so their presence says nothing about which language a file is
/// written in.
const COMMON_KEYWORDS: &[&str] = &[
    "of", "and", "for", "in", "not", "or", "if", "then", "parent", "list", "value",
];

/// What a keyword is called and how much it counts.
#[derive(Debug, Clone, PartialEq)]
pub struct KeywordData {
    /// The scope name from the grammar, before `classNameAliases` is applied.
    pub scope: String,
    /// How much a hit adds to the language's relevance.
    pub relevance: f64,
}

/// Flattens a grammar's keyword table.
pub fn compile(raw: &RawKeywords, case_insensitive: bool) -> HashMap<String, KeywordData> {
    let mut compiled = HashMap::new();
    collect(raw, case_insensitive, DEFAULT_SCOPE, &mut compiled);
    compiled
}

fn collect(
    raw: &RawKeywords,
    case_insensitive: bool,
    scope: &str,
    into: &mut HashMap<String, KeywordData>,
) {
    match raw {
        RawKeywords::Text(text) => compile_list(text.split(' '), case_insensitive, scope, into),
        RawKeywords::List(list) => compile_list(
            list.iter().map(String::as_str),
            case_insensitive,
            scope,
            into,
        ),
        RawKeywords::Scoped(map) => {
            for (nested_scope, nested) in map {
                collect(nested, case_insensitive, nested_scope, into);
            }
        }
    }
}

fn compile_list<'a>(
    words: impl Iterator<Item = &'a str>,
    case_insensitive: bool,
    scope: &str,
    into: &mut HashMap<String, KeywordData>,
) {
    for word in words {
        // A word may carry its own relevance: `while|5`.
        let mut parts = word.split('|');
        let word = parts.next().unwrap_or("");
        let explicit = parts.next();
        if word.is_empty() {
            continue;
        }
        let key = if case_insensitive {
            word.to_lowercase()
        } else {
            word.to_string()
        };
        into.insert(
            key,
            KeywordData {
                scope: scope.to_string(),
                relevance: score_for(word, explicit),
            },
        );
    }
}

/// The relevance a keyword contributes.
///
/// An explicit score always wins, including an explicit zero — a grammar can
/// force a common word to count if it wants to.
fn score_for(keyword: &str, explicit: Option<&str>) -> f64 {
    if let Some(explicit) = explicit {
        if !explicit.is_empty() {
            return explicit.parse().unwrap_or(0.0);
        }
    }
    if is_common(keyword) {
        0.0
    } else {
        1.0
    }
}

fn is_common(keyword: &str) -> bool {
    let lower = keyword.to_lowercase();
    COMMON_KEYWORDS.contains(&lower.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(raw: &str) -> RawKeywords {
        RawKeywords::Text(raw.to_string())
    }

    #[test]
    fn a_space_separated_string_becomes_words() {
        let compiled = compile(&text("for if then"), false);
        assert_eq!(compiled.len(), 3);
        assert_eq!(compiled["for"].scope, "keyword");
    }

    #[test]
    fn common_words_are_highlighted_but_count_for_nothing() {
        let compiled = compile(&text("for let"), false);
        // Both are keywords, so both get a scope...
        assert_eq!(compiled["for"].scope, "keyword");
        assert_eq!(compiled["let"].scope, "keyword");
        // ...but only the one that says something about the language scores.
        assert_eq!(compiled["for"].relevance, 0.0);
        assert_eq!(compiled["let"].relevance, 1.0);
    }

    #[test]
    fn an_explicit_relevance_wins_even_over_being_common() {
        // "for" is common and would score 0; `for|5` says otherwise.
        let compiled = compile(&text("for|5"), false);
        assert_eq!(compiled["for"].relevance, 5.0);
    }

    #[test]
    fn a_zero_relevance_can_be_forced() {
        let compiled = compile(&text("uncommon|0"), false);
        assert_eq!(compiled["uncommon"].relevance, 0.0);
    }

    #[test]
    fn nested_scopes_take_their_names_from_the_keys() {
        let raw = RawKeywords::Scoped(HashMap::from([
            ("keyword".to_string(), text("for")),
            ("built_in".to_string(), text("print")),
        ]));
        let compiled = compile(&raw, false);
        assert_eq!(compiled["for"].scope, "keyword");
        assert_eq!(compiled["print"].scope, "built_in");
    }

    #[test]
    fn nesting_can_go_all_the_way_down() {
        let raw = RawKeywords::Scoped(HashMap::from([(
            "meta".to_string(),
            RawKeywords::Scoped(HashMap::from([("key".to_string(), text("import"))])),
        )]));
        let compiled = compile(&raw, false);
        // The innermost key names the scope, not the outermost.
        assert_eq!(compiled["import"].scope, "key");
    }

    #[test]
    fn a_list_is_the_same_as_a_string() {
        let from_list = compile(
            &RawKeywords::List(vec!["for".to_string(), "if".to_string()]),
            false,
        );
        let from_text = compile(&text("for if"), false);
        assert_eq!(from_list, from_text);
    }

    #[test]
    fn a_case_insensitive_language_folds_its_keywords() {
        let compiled = compile(&text("SELECT From"), true);
        assert!(compiled.contains_key("select"));
        assert!(compiled.contains_key("from"));
        assert!(!compiled.contains_key("SELECT"));
    }

    #[test]
    fn a_case_sensitive_language_keeps_them_as_written() {
        let compiled = compile(&text("SELECT From"), false);
        assert!(compiled.contains_key("SELECT"));
        assert!(!compiled.contains_key("select"));
    }

    #[test]
    fn empty_words_are_skipped() {
        // Trailing or doubled spaces in a grammar's keyword string are common.
        let compiled = compile(&text("for  if "), false);
        assert_eq!(compiled.len(), 2);
        assert!(!compiled.contains_key(""));
    }

    #[test]
    fn a_word_may_contain_a_pipe_only_as_its_score() {
        // The split is on the first `|`, so a score that does not parse counts
        // as zero rather than dropping the keyword.
        let compiled = compile(&text("x|notanumber"), false);
        assert_eq!(compiled["x"].relevance, 0.0);
    }
}
