//! Finding text in a document.
//!
//! Search reads the *flattened* view — folded regions expanded — so that what
//! the user hid is still findable. See [`crate::buffer`] for why the document
//! carries both views.
//!
//! # Matching Dart's regular expressions
//!
//! This replaced `dart:core`'s `RegExp`, which is ECMAScript. That engine is
//! the API's contract — the patterns users have already written — so anything
//! behaving differently here would be a silent change in what the find panel
//! finds, and the kind of change nobody reports as a bug because it looks like
//! the document changed.
//!
//! So the engine is [`regress`], which implements the ECMAScript dialect rather
//! than Rust's own. The difference is not cosmetic: Rust's `regex` family treats
//! `\d`, `\w` and `\b` as Unicode by default, so `\w+` swallows `café` and `中文`
//! whole, where ECMAScript — and Dart — stop at the ASCII. On a document with
//! non-ASCII comments that is a visibly different set of matches.
//!
//! `regress`'s [`Flags`] map one to one onto JavaScript's `i`, `m`, `s` and `u`,
//! and the defaults line up with Dart's `RegExp`: case-sensitive unless asked,
//! not multiline, `.` not matching line breaks, and **not** in Unicode mode.
//!
//! Two things are still worth knowing:
//!
//! * A pattern that can match nothing steps by character, not by UTF-16 unit.
//!   Dart's `RegExp('').allMatches('😀')` reports three matches, one per code
//!   unit; this reports two, one per character — Rust cannot address the middle
//!   of a character. The find panel never asks: it returns early for an empty
//!   pattern, so a pattern like `a*` over text containing emoji is the only way
//!   to see it.
//! * A `regress` pattern is compiled per search. That is a parse of a few
//!   microseconds against a document scan of milliseconds, but it does mean the
//!   engine is not cached between searches the way `RegExp` objects can be.

use crate::buffer::{locate, Document};
use regress::{Flags, Regex};

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindOptions {
    pub pattern: String,
    pub case_sensitive: bool,
    /// Whether [`pattern`](Self::pattern) is a regular expression, or literal
    /// text to be matched exactly.
    pub regex: bool,
}

/// A match, as line and offset in the **flattened** view.
///
/// Offsets are UTF-16 code units, because that is what Flutter's text APIs and
/// the Dart editor model count in. The regex engine works in bytes, so the
/// conversion happens in [`locate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub start_line: usize,
    pub start_offset: usize,
    pub end_line: usize,
    pub end_offset: usize,
}

/// Why a search could not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindError {
    /// The pattern is not a valid regular expression. The find panel treats
    /// this the same as the Dart implementation does — as no result at all,
    /// rather than as an error to show.
    InvalidPattern(String),
}

impl std::fmt::Display for FindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FindError::InvalidPattern(message) => {
                write!(f, "invalid pattern: {message}")
            }
        }
    }
}

impl std::error::Error for FindError {}

/// Finds every match of [`FindOptions::pattern`] in a document.
///
/// Convenience for callers that have the document in hand; the work is in
/// [`find_in`], which a worker thread can call with a snapshot instead.
pub fn find(document: &Document, options: &FindOptions) -> Result<Vec<Match>, FindError> {
    find_in(document.flattened(), &document.flattened_starts(), options)
}

/// Finds every match of [`FindOptions::pattern`] in `text`.
///
/// `text` is the flattened view — folded regions expanded — and `starts` gives
/// the byte offset each of its lines begins at. Taking them rather than the
/// document is what lets a search run on a worker thread: the text can be
/// handed over as a snapshot, while the document cannot.
///
/// Matches come back in reading order and do not overlap, which is what
/// `RegExp.allMatches` gives and what the find panel steps through.
pub fn find_in(
    text: &str,
    starts: &[usize],
    options: &FindOptions,
) -> Result<Vec<Match>, FindError> {
    // A literal search is a regular expression that matches only itself, which
    // is how the Dart implementation spells it too — `RegExp(RegExp.escape(p))`
    // — so both engines see a pattern either way.
    let source = if options.regex {
        options.pattern.clone()
    } else {
        escape_literal(&options.pattern)
    };

    let regex = Regex::with_flags(
        &source,
        Flags {
            icase: !options.case_sensitive,
            // The rest keep their defaults, which are ECMAScript's: not
            // multiline, `.` does not match line breaks, and not Unicode mode.
            ..Flags::default()
        },
    )
    .map_err(|error| FindError::InvalidPattern(error.to_string()))?;

    let mut matches = Vec::new();
    for found in regex.find_iter(text) {
        let (start_line, start_offset) = locate(text, starts, found.start());
        let (end_line, end_offset) = locate(text, starts, found.end());
        matches.push(Match {
            start_line,
            start_offset,
            end_line,
            end_offset,
        });
    }
    Ok(matches)
}

/// Escapes `text` so that it matches only itself.
///
/// Escaping the metacharacters, and nothing else, is what `RegExp.escape` does
/// — `regress` accepts a backslash before a punctuation character as that
/// character, which is ECMAScript's "identity escape" outside Unicode mode.
/// Escaping more than necessary would work too, but it would also rely on
/// behaviour the specification only permits for compatibility.
fn escape_literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(
            character,
            '\\' | '^' | '$' | '.' | '|' | '?' | '*' | '+' | '(' | ')' | '[' | ']' | '{' | '}'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Line;

    fn document(text: &str) -> Document {
        Document::from_text(text)
    }

    fn options(pattern: &str) -> FindOptions {
        FindOptions {
            pattern: pattern.to_string(),
            case_sensitive: true,
            regex: false,
        }
    }

    /// Matches as `(start_line, start_offset, end_line, end_offset)`.
    fn matches(document: &Document, options: &FindOptions) -> Vec<(usize, usize, usize, usize)> {
        find(document, options)
            .expect("the pattern should be valid")
            .into_iter()
            .map(|m| (m.start_line, m.start_offset, m.end_line, m.end_offset))
            .collect()
    }

    #[test]
    fn finds_a_literal_on_one_line() {
        let document = document("hello world");
        assert_eq!(matches(&document, &options("world")), vec![(0, 6, 0, 11)]);
    }

    #[test]
    fn finds_every_occurrence_in_reading_order() {
        let document = document("a\nb\na\nba");
        assert_eq!(
            matches(&document, &options("a")),
            vec![(0, 0, 0, 1), (2, 0, 2, 1), (3, 1, 3, 2)]
        );
    }

    #[test]
    fn reports_offsets_within_the_line_not_within_the_document() {
        let document = document("abc\ndef");
        assert_eq!(matches(&document, &options("def")), vec![(1, 0, 1, 3)]);
    }

    #[test]
    fn a_match_can_span_lines() {
        let document = document("abc\ndef");
        assert_eq!(matches(&document, &options("c\nd")), vec![(0, 2, 1, 1)]);
    }

    #[test]
    fn looking_for_nothing_finds_it_everywhere() {
        // `RegExp('').allMatches('ab')` gives three matches, one per position.
        let document = document("ab");
        assert_eq!(
            matches(&document, &options("")),
            vec![(0, 0, 0, 0), (0, 1, 0, 1), (0, 2, 0, 2)]
        );
    }

    #[test]
    fn an_empty_pattern_steps_by_character_not_by_utf16_unit() {
        // "😀" is one character, two UTF-16 units and four bytes. Dart reports
        // three matches here, one per code unit; this reports two, one per
        // character, and reports the second at UTF-16 offset 2 rather than 1.
        //
        // Pinned rather than fixed — Rust cannot address the middle of a
        // character, and the find panel returns early for an empty pattern so
        // never asks. The point of the test is that stepping lands on a
        // character boundary: stepping one byte would slice the emoji in half
        // and panic on the slice.
        let document = document("😀");
        let found = find(&document, &options("")).expect("valid");
        assert_eq!(found.len(), 2);
        assert_eq!((found[0].start_line, found[0].start_offset), (0, 0));
        assert_eq!((found[1].start_line, found[1].start_offset), (0, 2));
    }

    #[test]
    fn case_insensitive_matching() {
        let document = document("Foo foo FOO");
        let insensitive = FindOptions {
            pattern: "foo".to_string(),
            case_sensitive: false,
            regex: false,
        };
        assert_eq!(
            matches(&document, &insensitive),
            vec![(0, 0, 0, 3), (0, 4, 0, 7), (0, 8, 0, 11)]
        );
        assert_eq!(matches(&document, &options("foo")), vec![(0, 4, 0, 7)]);
    }

    #[test]
    fn a_literal_pattern_is_not_treated_as_a_regular_expression() {
        let document = document("a.c abc");
        // The dot is a dot, not "any character".
        assert_eq!(matches(&document, &options("a.c")), vec![(0, 0, 0, 3)]);
    }

    #[test]
    fn a_regular_expression_is_used_as_one() {
        let document = document("a.c abc");
        let regex = FindOptions {
            pattern: "a.c".to_string(),
            case_sensitive: true,
            regex: true,
        };
        assert_eq!(matches(&document, &regex), vec![(0, 0, 0, 3), (0, 4, 0, 7)]);
    }

    #[test]
    fn character_classes_are_ascii_like_ecmascript() {
        // ECMAScript's `\d` is [0-9]; Rust's `regex` family would also match
        // the Arabic-Indic digit, which is the difference that made this use an
        // ECMAScript engine rather than a Rust one.
        let document = document("1 ٣ 2");
        let digits = FindOptions {
            pattern: r"\d".to_string(),
            case_sensitive: true,
            regex: true,
        };
        assert_eq!(
            matches(&document, &digits),
            vec![(0, 0, 0, 1), (0, 4, 0, 5)]
        );
    }

    #[test]
    fn word_characters_are_ascii_like_ecmascript() {
        // `\w+` stopping at the accent is the behaviour a user's existing
        // patterns depend on; a Unicode-aware engine swallows "café" whole.
        let document = document("café 中文 x");
        let words = FindOptions {
            pattern: r"\w+".to_string(),
            case_sensitive: true,
            regex: true,
        };
        // "caf" then "x": the accent, the space and both ideographs are not
        // word characters here, which is what ECMAScript says.
        assert_eq!(matches(&document, &words), vec![(0, 0, 0, 3), (0, 8, 0, 9)]);
    }

    #[test]
    fn a_pattern_containing_a_dot_compiles() {
        // The regression this guards is not hypothetical: it is what ruled out
        // turning Unicode mode off, and it would take the commonest
        // metacharacter in the language with it if it ever came back.
        let document = document("axc abc");
        let with_dot = FindOptions {
            pattern: "a.c".to_string(),
            case_sensitive: true,
            regex: true,
        };
        assert_eq!(
            matches(&document, &with_dot),
            vec![(0, 0, 0, 3), (0, 4, 0, 7)]
        );
    }

    #[test]
    fn lookahead_is_supported_because_ecmascript_has_it() {
        let document = document("foo1 bar2");
        let lookahead = FindOptions {
            pattern: r"[a-z]+(?=\d)".to_string(),
            case_sensitive: true,
            regex: true,
        };
        assert_eq!(
            matches(&document, &lookahead),
            vec![(0, 0, 0, 3), (0, 5, 0, 8)]
        );
    }

    #[test]
    fn an_invalid_pattern_is_an_error_not_a_panic() {
        let document = document("abc");
        let invalid = FindOptions {
            pattern: "(".to_string(),
            case_sensitive: true,
            regex: true,
        };
        assert!(matches!(
            find(&document, &invalid),
            Err(FindError::InvalidPattern(_))
        ));
    }

    #[test]
    fn offsets_count_utf16_units_not_bytes_or_characters() {
        // "日" is one UTF-16 unit in three UTF-8 bytes; "😀" is two units in
        // four. Counting the wrong one puts every later match on the wrong
        // column.
        let document = document("日😀x");
        assert_eq!(matches(&document, &options("x")), vec![(0, 3, 0, 4)]);
    }

    #[test]
    fn folded_content_is_searched() {
        // The reason the document carries a flattened view at all: text the
        // user folded away has to stay findable.
        let mut document = Document::from_lines(&["{", "}"]);
        let folded = Line::with_hidden(
            "{",
            vec!["hidden here".to_string(), "and here too".to_string()],
        );
        document.splice(0, 1, &[folded]).expect("fits");

        assert_eq!(matches(&document, &options("hidden")), vec![(1, 0, 1, 6)]);
        assert_eq!(
            matches(&document, &options("here too")),
            vec![(2, 4, 2, 12)]
        );
        // And the closing line is still where the flattened view puts it.
        assert_eq!(matches(&document, &options("}")), vec![(3, 0, 3, 1)]);
    }

    #[test]
    fn searching_an_empty_document_finds_nothing_to_find() {
        let document = document("");
        assert_eq!(
            matches(&document, &options("a")),
            Vec::<(usize, usize, usize, usize)>::new()
        );
        // But nothing is still a position, so an empty pattern has one match.
        assert_eq!(matches(&document, &options("")), vec![(0, 0, 0, 0)]);
    }

    #[test]
    fn a_match_at_the_very_end_lands_on_the_last_line() {
        let document = document("ab\ncd");
        assert_eq!(matches(&document, &options("d")), vec![(1, 1, 1, 2)]);
        // The end offset of a match is exclusive, so it can sit at the end of
        // the document — where there is no character to land on.
        let whole = FindOptions {
            pattern: "ab\ncd".to_string(),
            case_sensitive: true,
            regex: true,
        };
        assert_eq!(matches(&document, &whole), vec![(0, 0, 1, 2)]);
    }
}
