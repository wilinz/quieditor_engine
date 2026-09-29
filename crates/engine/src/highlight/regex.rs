//! The combined-regex matcher highlight.js is built on.
//!
//! A mode owns a list of rules — the `begin` of each of its sub-modes, its own
//! `end`, its `illegal`. At every position the engine has to find the nearest
//! place where *any* of them matches, then work out which one it was. Testing
//! them one at a time would be correct and far too slow: the common case is
//! scanning a long run of text in which nothing matches, and doing that once
//! per rule instead of once is the difference between milliseconds and seconds.
//!
//! So all the rules are welded into one alternation, each wrapped in a named
//! group, and the group that matched names the rule. That is highlight.js's
//! design and this is a port of it, down to the awkward part: a rule may
//! contain backreferences to its own capture groups, and wrapping it moves
//! those groups along. [`rewrite_backreferences`] walks each pattern and
//! renumbers them, which is the one piece of real cleverness in here.
//!
//! # Why the rules are not simply concatenated
//!
//! Because `(a)\1` wrapped naively as `((a)\1)` refers to the *outer* group,
//! not to `(a)`. Every backreference has to shift by the number of groups that
//! precede it once the wrapping is in place — and the patterns have to be read
//! closely to do that, since `(` inside a character class or after a backslash
//! is not a group at all.

use regress::{Flags, Match, Regex};

/// A rule that matched, and where.
#[derive(Debug, Clone)]
pub struct RuleMatch {
    /// The whole text the rule matched.
    pub text: String,
    /// The rule's own capture groups, `groups[0]` first. `groups[0]` is the
    /// whole match, as it is in JavaScript.
    pub groups: Vec<Option<String>>,
    /// Byte offset where the match starts.
    pub start: usize,
    /// Byte offset just past the match.
    pub end: usize,
    /// Which rule matched, as handed to [`MultiRegex::new`].
    pub rule: usize,
}

/// Matches any of a set of patterns at the nearest position.
///
/// Not thread-safe in the sense that matters here — it is used from one thread
/// per document, which is how the engine is driven.
pub struct MultiRegex {
    pattern: String,
    compiled: Regex,
    /// The capture-group number of each rule's wrapper, so a winner's own
    /// groups can be picked out of the combined match.
    ///
    /// Group numbers are the only handle the engine gives: the combined
    /// pattern is one regex, and "the groups belonging to this rule" means "the
    /// consecutive run of groups after its wrapper".
    wrapper_groups: Vec<usize>,
    /// How many groups each rule's own pattern opens.
    group_counts: Vec<usize>,
}

impl std::fmt::Debug for MultiRegex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiRegex")
            .field("rules", &self.group_counts.len())
            .finish_non_exhaustive()
    }
}

/// The dialect a grammar's patterns are read in.
///
/// Both switches are `RegExp` flags the Dart side sets when it compiles a
/// language, and they apply to every pattern that language contains — including
/// the ones the engine builds while it is running, which is why they travel
/// with the matcher rather than being resolved once at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Dialect {
    /// `RegExp(caseSensitive: false)` — the `i` flag. A grammar asks for it with
    /// `caseInsensitive`.
    pub case_insensitive: bool,
    /// `RegExp(unicode: true)` — the `u` flag. A grammar asks for it with
    /// `unicodeRegex`, and the three that do are Python, XML and Haskell. It
    /// changes what `.` and the character classes match, and makes `\u{…}` a
    /// code point rather than an escape.
    pub unicode: bool,
}

impl MultiRegex {
    /// Builds a matcher for `patterns`, which are the rules' sources in order.
    ///
    /// Returns `None` if the combined pattern will not compile, which happens
    /// when one of the rules is not valid ECMAScript — a grammar bug, but not
    /// one worth taking the editor down for.
    pub fn new(patterns: &[String], dialect: Dialect) -> Option<Self> {
        let names: Vec<String> = (0..patterns.len())
            .map(|index| format!("r{index}"))
            .collect();
        let group_counts: Vec<usize> = patterns.iter().map(|p| count_capture_groups(p)).collect();
        // Each wrapper is one group, and each rule before this one contributed
        // its wrapper plus its own groups. Groups are numbered from 1, so the
        // first wrapper is group 1.
        let mut wrapper_groups = Vec::with_capacity(patterns.len());
        let mut next = 1usize;
        for count in &group_counts {
            wrapper_groups.push(next);
            next += 1 + count;
        }

        let pattern = combine(patterns, &names);
        let compiled = Regex::with_flags(
            &pattern,
            Flags {
                icase: dialect.case_insensitive,
                unicode: dialect.unicode,
                // Always: highlight.js compiles `^` and `$` to mean the ends of
                // a *line*, because a mode's end is nearly always a line-shaped
                // thing — `$` ends a comment, `^` starts a shebang. Anchoring
                // them to the document instead would silently stop most
                // grammars from ever ending a mode.
                multiline: true,
                ..Flags::default()
            },
        )
        .ok()?;
        Some(Self {
            pattern,
            compiled,
            wrapper_groups,
            group_counts,
        })
    }

    /// The combined source, for tests and diagnostics.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// Finds the nearest match at or after `from`.
    ///
    /// Not anchored: highlight.js wants "the next match", and its callers
    /// decide whether that is close enough. [`MultiRegex::match_at`] is the
    /// anchored form.
    pub fn find_from(&self, text: &str, from: usize) -> Option<RuleMatch> {
        let found = self.compiled.find_from(text, from).next()?;
        self.describe(&found, text)
    }

    /// Finds a match starting exactly at `from`, if there is one.
    ///
    /// This is the sticky form, which is what the engine almost always wants:
    /// a rule that matches twenty characters further on is not a rule that
    /// matches here.
    pub fn match_at(&self, text: &str, from: usize) -> Option<RuleMatch> {
        let found = self.find_from(text, from)?;
        if found.start == from {
            Some(found)
        } else {
            None
        }
    }

    /// Turns a raw match into the rule that produced it.
    ///
    /// With named groups this is a lookup rather than highlight.js's group
    /// arithmetic: the name of the group that captured is the rule's index.
    fn describe(&self, found: &Match, text: &str) -> Option<RuleMatch> {
        // The name of the group that captured names the rule. Exactly one of
        // them has, in practice: the alternatives are anchored at the same
        // position and the engine takes the first that fits. A group that did
        // not participate is absent rather than empty, and a rule *can* match
        // nothing, so the check has to be on presence and not on length.
        let mut rule = None;
        for (name, range) in found.named_groups() {
            if range.is_none() {
                continue;
            }
            let Some(index) = name
                .strip_prefix('r')
                .and_then(|rest| rest.parse::<usize>().ok())
            else {
                continue;
            };
            if rule.is_none_or(|current| index < current) {
                rule = Some(index);
            }
        }
        let rule = rule?;

        // The winning rule's own groups: the run immediately after its wrapper,
        // and no others. `groups[0]` is the whole match, as it is in
        // JavaScript, and the non-participating ones stay as `None` because a
        // grammar refers to them by position — `match[2]` is the second group
        // whether or not it captured anything.
        let all: Vec<Option<std::ops::Range<usize>>> = found.groups().collect();
        let wrapper = self.wrapper_groups.get(rule).copied()?;
        let mut groups = vec![Some(text[found.range()].to_string())];
        for index in wrapper + 1..=wrapper + self.group_counts[rule] {
            groups.push(
                all.get(index)
                    .cloned()
                    .flatten()
                    .map(|range| text[range].to_string()),
            );
        }

        Some(RuleMatch {
            text: text[found.range()].to_string(),
            groups,
            start: found.start(),
            end: found.end(),
            rule,
        })
    }
}

/// Welds the patterns into one alternation, each in its own named group.
fn combine(patterns: &[String], names: &[String]) -> String {
    let mut combined = String::new();
    let mut capture_groups = 0usize;
    for (index, pattern) in patterns.iter().enumerate() {
        if index > 0 {
            combined.push('|');
        }
        // The name has to come first, and the wrapping group is itself a group
        // the backreferences inside `pattern` must be shifted past — so the
        // offset counts it.
        combined.push_str(&format!("(?<{}>", names[index]));
        combined.push_str(&rewrite_backreferences(pattern, capture_groups + 1));
        combined.push(')');
        capture_groups += 1 + count_capture_groups(pattern);
    }
    combined
}

/// Shifts every backreference in `pattern` by `offset`.
///
/// The patterns have to be walked rather than searched: a `(` inside a
/// character class is a literal, `\(` is a literal, and `(?:` does not capture.
/// Getting any of those wrong shifts a backreference onto the wrong group, and
/// the symptom is a grammar that highlights subtly wrongly rather than one that
/// fails.
pub fn rewrite_backreferences(pattern: &str, offset: usize) -> String {
    let bytes = pattern.as_bytes();
    let mut out = String::with_capacity(pattern.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\\' => {
                // An escape, or a backreference. `\1`..`\9` are references;
                // anything else is passed through, including `\\`.
                let mut end = index + 1;
                while end < bytes.len() && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end > index + 1 && bytes[index + 1] != b'0' {
                    if let Ok(number) = pattern[index + 1..end].parse::<usize>() {
                        out.push_str(&format!("\\{}", number + offset));
                        index = end;
                        continue;
                    }
                }
                // A character class may follow an escape, and its `(` and `)`
                // are literals — but the class itself is handled below, so a
                // single escaped character is enough here.
                let escape_len = if index + 1 < bytes.len() && bytes[index + 1].is_ascii() {
                    2
                } else {
                    // Multi-byte, which cannot start an escape sequence that
                    // matters; copy the character whole.
                    let mut len = 1;
                    while index + len < bytes.len() && !pattern.is_char_boundary(index + len) {
                        len += 1;
                    }
                    len
                };
                out.push_str(&pattern[index..index + escape_len]);
                index += escape_len;
            }
            b'[' => {
                // A character class: copy it whole, so its contents cannot be
                // mistaken for groups or references.
                let end = find_class_end(bytes, index);
                out.push_str(&pattern[index..end]);
                index = end;
            }
            b'(' => {
                // `(?:`, `(?=`, `(?!`, `(?<=`, `(?<!` do not capture.
                if bytes.get(index + 1) == Some(&b'?') {
                    out.push_str("(?");
                    index += 2;
                } else {
                    out.push('(');
                    index += 1;
                }
            }
            _ => {
                let len = utf8_len(byte);
                out.push_str(&pattern[index..index + len]);
                index += len;
            }
        }
    }
    out
}

/// How many capture groups a pattern opens.
///
/// `(?<name>...)` does capture, unlike the other `(?` forms — the distinction
/// the walk in [`rewrite_backreferences`] has to make too.
pub fn count_capture_groups(pattern: &str) -> usize {
    let bytes = pattern.as_bytes();
    let mut count = 0;
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'[' => index = find_class_end(bytes, index),
            b'(' => {
                if bytes.get(index + 1) == Some(&b'?') {
                    // A named group is the one `(?` form that captures.
                    if bytes.get(index + 2) == Some(&b'<')
                        && !matches!(bytes.get(index + 3), Some(&b'=') | Some(&b'!'))
                    {
                        count += 1;
                    }
                    index += 2;
                } else {
                    count += 1;
                    index += 1;
                }
            }
            _ => index += utf8_len(bytes[index]),
        }
    }
    count
}

/// The index just past the character class starting at `start`.
fn find_class_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 1;
    // A `]` immediately after `[` or `[^` is a literal, not the end.
    if bytes.get(index) == Some(&b'^') {
        index += 1;
    }
    if bytes.get(index) == Some(&b']') {
        index += 1;
    }
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b']' => return index + 1,
            _ => index += 1,
        }
    }
    bytes.len()
}

/// The length in bytes of the character starting with `byte`.
fn utf8_len(byte: u8) -> usize {
    match byte {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(pattern: &str) -> Vec<String> {
        vec![pattern.to_string()]
    }

    #[test]
    fn a_lone_pattern_is_wrapped_in_a_named_group() {
        assert_eq!(combine(&rule("abc"), &["r0".to_string()]), "(?<r0>abc)");
    }

    #[test]
    fn alternatives_are_separated() {
        let patterns = rule("a").into_iter().chain(rule("b")).collect::<Vec<_>>();
        assert_eq!(
            combine(&patterns, &["r0".to_string(), "r1".to_string()]),
            "(?<r0>a)|(?<r1>b)"
        );
    }

    #[test]
    fn a_backreference_is_shifted_past_the_wrapping_group() {
        // `(x)\1` becomes `(?<r0>(x)\2)`: its own group moved from 1 to 2 when
        // the wrapper took 1.
        assert_eq!(
            combine(&rule("(x)\\1"), &["r0".to_string()]),
            "(?<r0>(x)\\2)"
        );
    }

    #[test]
    fn a_backreference_shifts_by_the_groups_before_it() {
        // Two rules: the first has one group, so the second's wrapper is group
        // 3 and its own `(y)` is 4.
        let patterns = rule("(x)\\1")
            .into_iter()
            .chain(rule("(y)\\1"))
            .collect::<Vec<_>>();
        assert_eq!(
            combine(&patterns, &["r0".to_string(), "r1".to_string()]),
            "(?<r0>(x)\\2)|(?<r1>(y)\\4)"
        );
    }

    #[test]
    fn a_parenthesis_in_a_character_class_is_not_a_group() {
        assert_eq!(rewrite_backreferences("[(]a", 5), "[(]a");
        assert_eq!(count_capture_groups("[(]a"), 0);
    }

    #[test]
    fn a_bracket_immediately_after_the_open_is_literal() {
        assert_eq!(find_class_end(br"[]]x".as_slice(), 0), 3);
        assert_eq!(count_capture_groups("[]]"), 0);
    }

    #[test]
    fn an_escaped_parenthesis_is_not_a_group() {
        assert_eq!(rewrite_backreferences("\\(a", 5), "\\(a");
        assert_eq!(count_capture_groups("\\(a"), 0);
    }

    #[test]
    fn non_capturing_groups_are_not_counted() {
        assert_eq!(count_capture_groups("(?:a)(?=b)(?!c)"), 0);
        assert_eq!(count_capture_groups("(a)(?:b)"), 1);
    }

    #[test]
    fn a_named_group_is_counted() {
        // It captures, unlike the other `(?` forms — and `(?<=` / `(?<!` are
        // lookbehind, not names.
        assert_eq!(count_capture_groups("(?<name>a)"), 1);
        assert_eq!(count_capture_groups("(?<=a)"), 0);
        assert_eq!(count_capture_groups("(?<!a)"), 0);
    }

    #[test]
    fn an_escape_is_copied_whole() {
        assert_eq!(rewrite_backreferences("a\\db", 5), "a\\db");
        assert_eq!(rewrite_backreferences("a\\\\1", 5), "a\\\\1");
    }

    #[test]
    fn the_nearest_match_wins() {
        let matcher = MultiRegex::new(&["foo".to_string(), "bar".to_string()], Dialect::default())
            .expect("valid");
        let found = matcher.match_at("xx bar foo", 3).expect("matches at 3");
        assert_eq!(found.rule, 1);
        assert_eq!(found.text, "bar");
        // Not sticky: nothing matches at 0.
        assert!(matcher.match_at("xx bar foo", 0).is_none());
        // But `find_from` still finds the next one.
        assert_eq!(matcher.find_from("xx bar foo", 0).map(|m| m.start), Some(3));
    }

    #[test]
    fn the_winning_rule_is_reported_by_name() {
        let matcher = MultiRegex::new(&["a".to_string(), "b".to_string()], Dialect::default())
            .expect("valid");
        assert_eq!(matcher.match_at("ab", 0).map(|m| m.rule), Some(0));
        assert_eq!(matcher.match_at("ab", 1).map(|m| m.rule), Some(1));
    }

    #[test]
    fn a_rules_own_groups_come_back_with_it() {
        let matcher = MultiRegex::new(&["(a)(b)".to_string(), "c".to_string()], Dialect::default())
            .expect("valid");
        let found = matcher.match_at("ab", 0).expect("matches");
        assert_eq!(found.rule, 0);
        assert_eq!(found.groups[0].as_deref(), Some("ab"));
        assert_eq!(found.groups[1].as_deref(), Some("a"));
        assert_eq!(found.groups[2].as_deref(), Some("b"));
    }

    #[test]
    fn a_backreference_still_refers_to_its_own_group() {
        // The whole point of the renumbering: `(a)\1` wrapped must still match
        // "aa" and not "a" followed by something else.
        let matcher = MultiRegex::new(&["(a)\\1".to_string()], Dialect::default()).expect("valid");
        assert!(matcher.match_at("aa", 0).is_some());
        assert!(matcher.match_at("ab", 0).is_none());
    }

    #[test]
    fn a_grammar_that_does_not_compile_is_refused_rather_than_guessed_at() {
        assert!(MultiRegex::new(&["(".to_string()], Dialect::default()).is_none());
    }

    #[test]
    fn case_insensitivity_is_the_languages_to_decide() {
        let sensitive = MultiRegex::new(&["abc".to_string()], Dialect::default()).expect("valid");
        assert!(sensitive.match_at("ABC", 0).is_none());
        let insensitive = MultiRegex::new(
            &["abc".to_string()],
            Dialect {
                case_insensitive: true,
                ..Dialect::default()
            },
        )
        .expect("valid");
        assert!(insensitive.match_at("ABC", 0).is_some());
    }
}
