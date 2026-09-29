//! Bracket analysis: finding the regions the editor lets you collapse.
//!
//! A "chunk" is a balanced bracket pair spanning at least two lines — from the
//! line that opens it to the line that closes it. The editor draws a fold
//! marker beside the opening line and hides everything in between.
//!
//! This is a port of `DefaultCodeChunkAnalyzer` from `lib/src/code_chunk.dart`,
//! and it is deliberately faithful to that implementation, including its
//! quirks: see [`scan_line`] for the quote handling, which abandons the rest of
//! a line when it meets an unbalanced escape rather than guessing.
//!
//! # Why it moved
//!
//! The Dart version runs on every content change and takes ~230 ms on the
//! 108k-line sample that ships with the package — measured, see
//! `benchmark/hotpath_bench_test.dart`. Two things are wrong with it beyond the
//! language: it allocates a `String` per bracket, and its duplicate-open guard
//! is a linear scan of the chunks found so far, which is quadratic on
//! bracket-heavy input.
//!
//! Both are fixed here — scanning is over bytes, and the guard is a hash set —
//! but the *behaviour* is unchanged, so the existing Dart tests remain the
//! specification.

use std::collections::HashSet;

/// A collapsible region, as the line that opens it and the line that closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// Line index of the opening bracket.
    pub index: usize,
    /// Line index of the closing bracket.
    pub end: usize,
}

impl Chunk {
    /// How many lines the region hides when collapsed.
    pub fn collapse_size(&self) -> usize {
        self.end.saturating_sub(self.index + 1)
    }

    /// Whether collapsing this region would hide anything.
    pub fn can_collapse(&self) -> bool {
        self.collapse_size() > 0
    }
}

/// A bracket found in the document.
///
/// `value` is the ASCII byte itself rather than a `String`: every symbol the
/// analyser cares about is one byte, and comparing `u8`s avoids allocating one
/// string per bracket in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSymbol {
    pub value: u8,
    /// Index of the line this symbol sits on.
    pub line: usize,
}

/// The characters that can open or close a chunk.
const TOKENS: &[u8] = b"\"'()[]{}";

/// The byte that closes the region opened by `open`, if `open` opens one.
fn closing_for(open: u8) -> Option<u8> {
    match open {
        b'(' => Some(b')'),
        b'[' => Some(b']'),
        b'{' => Some(b'}'),
        _ => None,
    }
}

/// Finds every collapsible region in `lines`.
///
/// Takes an iterator rather than a slice so that a [`crate::buffer::Document`]
/// can be scanned in place. Collecting a document's lines into a `&[&str]` first
/// would allocate on every keystroke, which is most of what moving the document
/// into Rust was meant to avoid.
///
/// # Examples
///
/// ```
/// use quieditor_engine::chunk::{analyze, Chunk};
///
/// let chunks = analyze(["abc(", "abc", "abc)"]);
/// assert_eq!(chunks, vec![Chunk { index: 0, end: 2 }]);
/// ```
pub fn analyze<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut stack: Vec<ChunkSymbol> = Vec::new();
    // Lines that already open a chunk. A line can hold several brackets, and
    // the editor only ever folds one region per opening line, so the first
    // close wins and later ones are dropped. The Dart version asks this by
    // scanning the chunks found so far, which is quadratic; a set is not.
    let mut already_opened: HashSet<usize> = HashSet::new();

    for (line_index, line) in lines.into_iter().enumerate() {
        for symbol in scan_line(line, line_index) {
            if closing_for(symbol.value).is_some() {
                stack.push(symbol);
                continue;
            }
            // Anything that is not an opener is treated as a closer, which is
            // what the Dart version does: a stray `)` simply finds nothing to
            // close, and the stack is left alone.
            let wanted = symbol.value;
            // Walk back to the nearest bracket this one actually closes. The
            // ones skipped over were never closed, so they are discarded.
            while let Some(open) = stack.pop() {
                if closing_for(open.value) != Some(wanted) {
                    continue;
                }
                // A region has to span lines to be worth folding.
                if symbol.line > open.line && already_opened.insert(open.line) {
                    chunks.push(Chunk {
                        index: open.line,
                        end: symbol.line,
                    });
                }
                break;
            }
        }
    }

    chunks.sort_by_key(|chunk| chunk.index);
    chunks
}

/// True when the byte before `index` is a backslash.
fn is_pre_escape(bytes: &[u8], index: usize) -> bool {
    index > 0 && bytes[index - 1] == b'\\'
}

/// Where the scanner is with respect to quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quoting {
    /// Outside any quote: brackets count.
    Normal,
    /// Inside a single-quoted run: brackets do not count.
    Single,
    /// Inside a double-quoted run: brackets do not count.
    Double,
}

/// Collects the brackets on one line that are not inside a string.
///
/// The state machine mirrors `DefaultCodeChunkAnalyzer._parseLine`, including
/// its handling of backslashes, which is quirky enough to be worth spelling
/// out. A `"` or `'` preceded by a backslash does not open a string; instead it
/// flips an "escaped quote" flag. That flag suppresses brackets until a later
/// quote is itself preceded by a backslash, which clears it. If a quote arrives
/// while the flag is set and is *not* escaped, the line is given up on
/// entirely — the analyser treats the quoting as too broken to reason about.
///
/// Whitespace is stripped first. That is not an optimisation detail: trimming
/// can only remove characters that are never brackets, so it cannot change
/// which symbols are found. It is kept because the Dart version does it, and
/// being able to diff the two implementations line by line is worth more here
/// than saving a pass.
fn scan_line(line: &str, index: usize) -> Vec<ChunkSymbol> {
    let text = line.trim();
    if text.is_empty() {
        return Vec::new();
    }
    // Scanning bytes rather than chars is safe and exact: every token is
    // ASCII, and a UTF-8 continuation byte is always >= 0x80, so it can never
    // be mistaken for one. The backslash check is safe for the same reason —
    // a continuation byte can never be 0x5c.
    let bytes = text.as_bytes();
    let mut symbols = Vec::new();
    let mut quoting = Quoting::Normal;
    let mut escaped_single = false;
    let mut escaped_double = false;

    for i in 0..bytes.len() {
        let byte = bytes[i];
        if !TOKENS.contains(&byte) {
            continue;
        }
        match quoting {
            Quoting::Single => {
                if byte == b'\'' && !is_pre_escape(bytes, i) {
                    quoting = Quoting::Normal;
                }
            }
            Quoting::Double => {
                if byte == b'"' && !is_pre_escape(bytes, i) {
                    quoting = Quoting::Normal;
                }
            }
            Quoting::Normal => match byte {
                b'\'' => {
                    if escaped_single {
                        if is_pre_escape(bytes, i) {
                            escaped_single = false;
                        } else {
                            // An escaped quote followed by a bare one: the
                            // quoting is unbalanced, so stop reading the line.
                            break;
                        }
                    } else if is_pre_escape(bytes, i) {
                        escaped_single = true;
                    } else {
                        quoting = Quoting::Single;
                    }
                }
                b'"' => {
                    if escaped_double {
                        if is_pre_escape(bytes, i) {
                            escaped_double = false;
                        } else {
                            break;
                        }
                    } else if is_pre_escape(bytes, i) {
                        escaped_double = true;
                    } else {
                        quoting = Quoting::Double;
                    }
                }
                _ => {
                    if !escaped_single && !escaped_double {
                        symbols.push(ChunkSymbol {
                            value: byte,
                            line: index,
                        });
                    }
                }
            },
        }
    }

    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors `DefaultCodeChunkAnalyzer.parse` so the symbol-level cases from
    /// `test/code_chunk_default_analyzer_test.dart` carry over directly.
    fn parse(lines: &[&str]) -> Vec<ChunkSymbol> {
        lines
            .iter()
            .enumerate()
            .flat_map(|(index, line)| scan_line(line, index))
            .collect()
    }

    fn symbols(pairs: &[(u8, usize)]) -> Vec<ChunkSymbol> {
        pairs
            .iter()
            .map(|&(value, line)| ChunkSymbol { value, line })
            .collect()
    }

    fn chunks(pairs: &[(usize, usize)]) -> Vec<Chunk> {
        pairs
            .iter()
            .map(|&(index, end)| Chunk { index, end })
            .collect()
    }

    #[test]
    fn empty_and_bracket_free_lines_yield_nothing() {
        assert_eq!(parse(&[""]), vec![]);
        assert_eq!(parse(&["abc"]), vec![]);
        assert_eq!(parse(&["   "]), vec![]);
        // Whitespace-only lines are skipped before scanning, so indentation
        // does not produce symbols either.
        assert_eq!(parse(&["\t "]), vec![]);
    }

    #[test]
    fn every_bracket_kind_is_recognised() {
        assert_eq!(parse(&["("]), symbols(&[(b'(', 0)]));
        assert_eq!(parse(&[")"]), symbols(&[(b')', 0)]));
        assert_eq!(parse(&["["]), symbols(&[(b'[', 0)]));
        assert_eq!(parse(&["]"]), symbols(&[(b']', 0)]));
        assert_eq!(parse(&["{"]), symbols(&[(b'{', 0)]));
        assert_eq!(parse(&["}"]), symbols(&[(b'}', 0)]));
        assert_eq!(parse(&["()"]), symbols(&[(b'(', 0), (b')', 0)]));
        assert_eq!(parse(&["[]"]), symbols(&[(b'[', 0), (b']', 0)]));
        assert_eq!(parse(&["{("]), symbols(&[(b'{', 0), (b'(', 0)]));
    }

    #[test]
    fn brackets_are_indexed_by_line() {
        assert_eq!(parse(&["(", "", "("]), symbols(&[(b'(', 0), (b'(', 2)]));
    }

    #[test]
    fn brackets_inside_quotes_are_ignored() {
        assert_eq!(parse(&["\""]), vec![]);
        assert_eq!(parse(&["'"]), vec![]);
        assert_eq!(parse(&["\\\"\\\""]), vec![]);
        assert_eq!(parse(&["\\'\\'"]), vec![]);
        assert_eq!(parse(&["\"abc\""]), vec![]);
        assert_eq!(parse(&["'abc'"]), vec![]);
        assert_eq!(parse(&["\"'abc'\""]), vec![]);
        assert_eq!(parse(&["\"(\""]), vec![]);
        assert_eq!(parse(&["'('"]), vec![]);
    }

    #[test]
    fn unbalanced_quoting_abandons_the_rest_of_the_line() {
        // An opening quote that never closes hides everything after it.
        assert_eq!(parse(&["'("]), vec![]);
        assert_eq!(parse(&["\"("]), vec![]);
        assert_eq!(parse(&["'('')"]), vec![]);
        assert_eq!(parse(&["\"'()[]{}"]), vec![]);
    }

    #[test]
    fn quoting_that_closes_again_exposes_later_brackets() {
        assert_eq!(parse(&["\"'()\"[]"]), symbols(&[(b'[', 0), (b']', 0)]));
        assert_eq!(parse(&["\"\\\"()\"[]"]), symbols(&[(b'[', 0), (b']', 0)]));
    }

    #[test]
    fn an_escaped_quote_suppresses_brackets_until_it_is_cleared() {
        // `\"()"[]` — the leading escaped quote turns the suppression flag on,
        // so the brackets are dropped; the later bare quote, with no backslash
        // before it, gives up on the line.
        assert_eq!(parse(&["\\\"()\"[]"]), vec![]);
    }

    #[test]
    fn a_single_line_never_forms_a_chunk() {
        // Both brackets on one line is not a foldable region.
        assert_eq!(analyze(["abc(){}[]"]), vec![]);
        assert_eq!(analyze([""]), vec![]);
        assert_eq!(analyze(["abc"]), vec![]);
    }

    #[test]
    fn adjacent_brackets_do_form_a_chunk() {
        // The region spans two lines, which is the whole requirement: a
        // bracket pair on a *single* line is the case that folds nothing.
        assert_eq!(analyze(["((", "))"]), chunks(&[(0, 1)]));
    }

    #[test]
    fn a_bracket_pair_across_lines_forms_a_chunk() {
        assert_eq!(
            analyze(["abc(", "abc", "abc", "abc", "abc)"]),
            chunks(&[(0, 4)])
        );
    }

    #[test]
    fn nested_regions_are_reported_innermost_first_but_sorted() {
        assert_eq!(
            analyze(["abc(", "abc[", "abc{}", "abc]", "abc)"]),
            chunks(&[(0, 4), (1, 3)])
        );
    }

    #[test]
    fn regions_are_sorted_by_opening_line() {
        assert_eq!(
            analyze(["{", "}", "{", "}", "{", "}"]),
            chunks(&[(0, 1), (2, 3), (4, 5)])
        );
    }

    #[test]
    fn unclosed_brackets_are_discarded() {
        // The `[` on line 1 is closed by `]`, but the outer `(` is closed by
        // the `)` on line 4 and skips over the unmatched `[` on line 3.
        assert_eq!(
            analyze(["abc(", "abc[", "abc", "abc[", "abc)"]),
            chunks(&[(0, 4)])
        );
    }

    #[test]
    fn repeated_single_bracket_lines_pair_sequentially() {
        assert_eq!(
            analyze(["abc(", "abc[[[[", "abc", "abc]]]]", "abc)"]),
            chunks(&[(0, 4), (1, 3)])
        );
    }

    #[test]
    fn every_close_line_pairs_with_the_next_open_line() {
        assert_eq!(
            analyze(["abc(", "[[[[", "abc", "]", "]", "]", "]", "abc)"]),
            chunks(&[(0, 7), (1, 3)])
        );
    }

    #[test]
    fn adjacent_lines_pair_up_rather_than_nesting() {
        // Each close takes the nearest open, so this is two sibling regions,
        // not one nested inside the other.
        assert_eq!(analyze(["(", "(", ")", ")"]), chunks(&[(0, 3), (1, 2)]));
    }

    #[test]
    fn only_the_first_region_per_opening_line_is_kept() {
        // Both `[`s sit on line 0, so both closes want to open a region there.
        // The editor folds one region per opening line: the first close wins
        // and the second is dropped. This is the case the Dart version answers
        // with a linear scan of the chunks found so far.
        assert_eq!(analyze(["[[", "]", "]"]), chunks(&[(0, 1)]));
    }

    #[test]
    fn collapse_size_counts_the_hidden_lines() {
        assert_eq!(Chunk { index: 0, end: 4 }.collapse_size(), 3);
        assert!(Chunk { index: 0, end: 4 }.can_collapse());
        assert_eq!(Chunk { index: 0, end: 1 }.collapse_size(), 0);
        assert!(!Chunk { index: 0, end: 1 }.can_collapse());
        // Degenerate input must not underflow.
        assert_eq!(Chunk { index: 5, end: 2 }.collapse_size(), 0);
    }

    #[test]
    fn the_shipped_json_samples_produce_the_expected_chunks() {
        // Deliberately the *same* fixture files the Dart suite uses, and the
        // same expected values (`test/code_chunk_default_analyzer_test.dart`).
        // Reaching out of the crate for them is the point: two copies of a
        // fixture would eventually disagree, and the disagreement would be
        // invisible.
        let pretty = include_str!("../../../test/data/json_pretty.json");
        let pretty_lines: Vec<&str> = pretty.lines().collect();
        assert_eq!(
            analyze(pretty_lines),
            chunks(&[(0, 17), (1, 5), (2, 4), (6, 15), (7, 10), (11, 14)])
        );

        let flat = include_str!("../../../test/data/json_flatted.json");
        let flat_lines: Vec<&str> = flat.lines().collect();
        assert_eq!(analyze(flat_lines), vec![]);
    }
}
