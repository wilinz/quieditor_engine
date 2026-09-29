//! The document, held on the Rust side of the boundary.
//!
//! Every operation the editor pushes into Rust — bracket analysis, search,
//! syntax highlighting — needs the whole document, and it needs it on every
//! keystroke. Sending it each time costs more than the work: encoding 4.6 MB to
//! UTF-8 and copying it across the ABI measured ~8 ms, against ~1–2 ms for the
//! analysis that followed.
//!
//! So the document lives here instead, and Dart sends only what changed. The
//! interesting part is not the storage — it is [`Document::splice`], which has
//! to keep the list of lines and the text they came from in agreement.
//!
//! # Two views of the same document
//!
//! The editor reads the document in two different ways, and they are not the
//! same sequence of lines:
//!
//! * **Collapsed.** One entry per line the user can see the start of. A line
//!   whose chunk is folded is a single entry holding the visible part. Bracket
//!   analysis reads this: a folded region must stay folded, not reappear.
//! * **Flattened.** Folded regions expanded back out, one entry per line of
//!   text. Search reads this: it should find what is hidden, too.
//!
//! [`Line::hidden`] is what reconciles them. It is empty for almost every line —
//! a line only hides anything once the user folds something — so carrying it
//! costs nothing on the path that runs on every keystroke, and the flattened
//! view can be produced on demand rather than rebuilt by Dart each time.
//!
//! # Why not a rope
//!
//! A rope would give O(log n) character offsets, which this does not have. It
//! also cannot hand out a `&str` for a line that spans several chunks, so every
//! one of the per-line scans below — and there is one per line, per keystroke —
//! would allocate. Every operation here is line-oriented, and a list of lines
//! does that in O(1) with no allocation at all.

use std::fmt;
use std::sync::Arc;

/// One line of the document: its own text, and whatever its folded chunks hide.
///
/// Mirrors the Dart model's `CodeLine`. [`hidden`](Line::hidden) is already
/// flattened — a folded chunk of a folded chunk contributes its lines here
/// directly, not nested — because that is the order both views are read in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Line {
    /// The text of the line itself. This is what bracket analysis sees.
    pub text: String,
    /// Lines hidden beneath this one, in flattened order.
    pub hidden: Vec<String>,
}

impl Line {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            hidden: Vec::new(),
        }
    }

    pub fn with_hidden(text: impl Into<String>, hidden: Vec<String>) -> Self {
        Self {
            text: text.into(),
            hidden,
        }
    }

    /// Every line this one contributes to the flattened view, its own first.
    pub fn flattened(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.text.as_str()).chain(self.hidden.iter().map(String::as_str))
    }

    /// How many flattened lines this one contributes.
    pub fn flattened_len(&self) -> usize {
        1 + self.hidden.len()
    }
}

/// A document kept as a list of lines.
///
/// The lines are the same ones the Dart model has: `text.split('\n')`, so an
/// empty document is one empty line rather than none.
#[derive(Debug, Clone)]
pub struct Document {
    lines: Vec<Line>,
    /// The flattened view, joined with `\n`, kept up to date as the document is
    /// edited rather than rebuilt on demand.
    ///
    /// Rebuilding it is a pass over the whole document — 5 ms on 100,000 lines
    /// — and search needs it on every keystroke while the find panel is open.
    /// Maintaining it here costs a memmove of whatever follows the edit, which
    /// is nothing at all for an edit at the end of the document and under half
    /// a millisecond for one at the start.
    ///
    /// Behind an [`Arc`] so a search can hand it to a worker thread in O(1).
    /// An edit then has to copy it once if a search is in flight, which is a
    /// single memcpy rather than a rebuild.
    flattened: Arc<String>,
    /// Incremented whenever the contents change, so a caller can tell whether
    /// cached work is still valid without comparing the text.
    revision: u64,
}

/// Compares the lines, not the derived flattened text — the latter follows from
/// the former, and comparing both would double the cost of every check.
impl PartialEq for Document {
    fn eq(&self, other: &Self) -> bool {
        self.lines == other.lines && self.revision == other.revision
    }
}

impl Eq for Document {}

/// A splice that does not line up with the document it was applied to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpliceError {
    /// The line the splice claimed to start at.
    pub start: usize,
    /// How many lines it claimed to remove.
    pub removed: usize,
    /// How many lines the document actually has.
    pub line_count: usize,
}

impl fmt::Display for SpliceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "splice at line {} removing {} does not fit a document of {} lines",
            self.start, self.removed, self.line_count
        )
    }
}

impl std::error::Error for SpliceError {}

impl Document {
    /// Builds a document from the text the lines were joined with `\n`, with
    /// nothing folded.
    ///
    /// This is the same split the Dart model performs, so the two agree on what
    /// a line is, including an empty document being a single empty line.
    pub fn from_text(text: &str) -> Self {
        Self::from_owned_lines(text.split('\n').map(Line::new).collect())
    }

    pub fn from_lines(lines: &[&str]) -> Self {
        Self::from_owned_lines(lines.iter().map(|line| Line::new(*line)).collect())
    }

    /// Builds a document from lines that are already in hand.
    ///
    /// The revision starts at zero like any other new document, which is why
    /// this exists rather than building an empty document and splicing into it:
    /// a caller polling the revision to decide whether its cached work is still
    /// good would otherwise see a document that looks edited before anyone has
    /// touched it.
    pub fn from_owned_lines(mut lines: Vec<Line>) -> Self {
        if lines.is_empty() {
            lines.push(Line::default());
        }
        let flattened = join_flattened(&lines);
        Self {
            lines,
            flattened: Arc::new(flattened),
            revision: 0,
        }
    }

    /// The number of lines in the collapsed view.
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        // A document is never empty of *lines* — an empty document is one empty
        // line — so this asks whether there is any text at all.
        self.lines
            .iter()
            .all(|line| line.text.is_empty() && line.hidden.is_empty())
    }

    /// The text of the line at `index` in the collapsed view.
    pub fn line(&self, index: usize) -> Option<&str> {
        self.lines.get(index).map(|line| line.text.as_str())
    }

    /// The lines of the collapsed view.
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// The collapsed view, for the scans that only look at what is visible.
    pub fn iter(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().map(|line| line.text.as_str())
    }

    /// The flattened view — folded regions expanded — as one string, its lines
    /// joined with `\n`.
    ///
    /// Kept current as the document is edited, so reading it is free.
    pub fn flattened(&self) -> &str {
        &self.flattened
    }

    /// The flattened view as a value a worker thread can hold.
    ///
    /// O(1): the point of keeping the text in an [`Arc`] is that handing it to
    /// another thread costs nothing, however large the document is.
    pub fn flattened_snapshot(&self) -> Arc<String> {
        Arc::clone(&self.flattened)
    }

    /// The flattened view, line by line.
    pub fn flattened_lines(&self) -> impl Iterator<Item = &str> {
        self.lines.iter().flat_map(Line::flattened)
    }

    /// How many lines the flattened view has.
    pub fn flattened_len(&self) -> usize {
        self.lines.iter().map(Line::flattened_len).sum()
    }

    /// The byte offset each flattened line begins at, plus a sentinel for the
    /// end of the text.
    ///
    /// Derived from the text rather than kept alongside it: only a search asks,
    /// and one pass over the text is nothing next to the scan that follows —
    /// which is usually on another thread anyway.
    ///
    /// Offsets are bytes, not UTF-16 units, because the regex engine indexes by
    /// byte. Callers reporting positions back to Dart convert per match, which
    /// only has to look at the one line the match fell on.
    pub fn flattened_starts(&self) -> Vec<usize> {
        line_starts(&self.flattened)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Replaces `removed` lines starting at `start` with `added`.
    ///
    /// This is the operation the Dart side drives on every keystroke: it
    /// compares the document it last sent against the current one, works out
    /// the one span that differs, and sends only that. Everything else is a
    /// no-op here.
    ///
    /// Returns whether anything actually changed. Replacing a line with an
    /// identical one is not a change, and reporting it as one would make
    /// `revision` meaningless to anyone using it to skip work.
    pub fn splice(
        &mut self,
        start: usize,
        removed: usize,
        added: &[Line],
    ) -> Result<bool, SpliceError> {
        if start > self.lines.len() || start + removed > self.lines.len() {
            return Err(SpliceError {
                start,
                removed,
                line_count: self.lines.len(),
            });
        }

        let unchanged = removed == added.len()
            && self.lines[start..start + removed]
                .iter()
                .zip(added)
                .all(|(old, new)| old == new);
        if unchanged {
            return Ok(false);
        }

        // Where the replaced lines sit in the flattened text, worked out before
        // the lines change under us.
        let first_flat_line: usize = self.lines[..start].iter().map(Line::flattened_len).sum();
        let replaced_flat_lines: usize = self.lines[start..start + removed]
            .iter()
            .map(Line::flattened_len)
            .sum();
        let total_flat_lines = self.flattened_len();
        let at = offset_of_flat_line(&self.lines, first_flat_line);
        // Both ends of the replaced span, read off the lines as they are now —
        // after the splice below they describe a different document.
        let past = offset_of_flat_line(&self.lines, first_flat_line + replaced_flat_lines);
        let replacement = join_flattened(added);

        self.lines
            .splice(start..start + removed, added.iter().cloned());
        // A document always has at least one line, because its text is
        // `lines.join("\n")` and the empty text is one empty line — the same
        // rule Dart's `split('\n')` gives. Deleting the last line therefore
        // leaves an empty one behind rather than leaving nothing.
        if self.lines.is_empty() {
            self.lines.push(Line::default());
        }

        // The same join rules the lines follow, one level down. The separator
        // between two flattened lines belongs to the one before it, so removing
        // a run has to take the newline that followed it, unless the run took
        // the newline that preceded it instead because nothing follows.
        {
            let flattened = Arc::make_mut(&mut self.flattened);
            let ends_document = first_flat_line + replaced_flat_lines == total_flat_lines;
            let range = if !ends_document {
                // Something follows: the run takes the separator after it with
                // it, leaving the one before to separate what remains.
                (at, past)
            } else if replaced_flat_lines > 0 && first_flat_line > 0 {
                // The run is the tail, so it takes the separator *before* it —
                // there is none after, and without this the document would end
                // in a blank line.
                (at - 1, flattened.len())
            } else {
                // Nothing follows and nothing is removed: appending, or
                // replacing the whole document.
                (at, flattened.len())
            };
            let tail = flattened.split_off(range.1);
            flattened.truncate(range.0);
            if !replacement.is_empty() {
                // A separator is needed wherever the edit left content on the
                // other side of it. Neither the run before nor the tail after
                // supplies one, so it goes here.
                if tail.is_empty() && range.0 > 0 {
                    flattened.push('\n');
                }
                flattened.push_str(&replacement);
                if !tail.is_empty() {
                    flattened.push('\n');
                }
            }
            flattened.push_str(&tail);
        }

        self.revision += 1;
        Ok(true)
    }
}

/// The byte offset each line of `text` begins at, plus a sentinel for the end.
pub fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(text.len() / 40 + 2);
    starts.push(0);
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    // One entry per line so far, plus a sentinel holding the end of the text —
    // so a match running to the very end still finds a line to land on, and an
    // empty document still has the one line it is made of.
    starts.push(text.len());
    starts
}

/// Turns a byte offset in `text` into a line and a UTF-16 offset within it.
///
/// [starts] is the byte offset each line begins at, as [`line_starts`] returns
/// it, sentinel included. Offsets are UTF-16 code units because that is what
/// Flutter's text APIs and the Dart editor model count in, while the engines
/// that produce offsets here work in bytes.
///
/// Shared by search and highlighting rather than written twice: both report
/// what they found the same way, and a span that disagreed with a match about
/// where a line ends would be a bug in whichever one was read second.
pub fn locate(text: &str, starts: &[usize], offset: usize) -> (usize, usize) {
    // The sentinel is the end of the text rather than the start of a line, so
    // an offset there belongs to the last real line.
    let last = starts.len().saturating_sub(2);
    let line = match starts.binary_search(&offset) {
        Ok(index) => index.min(last),
        // Not on a boundary, so it falls inside the line the insertion point
        // follows. The first line starts at 0, so this cannot underflow.
        Err(index) => index.saturating_sub(1).min(last),
    };
    let line_start = starts.get(line).copied().unwrap_or(0);
    (line, utf16_column(text, line_start, offset))
}

/// How many UTF-16 code units `text` from `line_start` to `offset` covers.
///
/// The conversion from the byte offsets the engines work in to the UTF-16 ones
/// the editor draws in, in one place: it was written twice, and the highlighting
/// that keeps its own line starts is the copy that got it wrong. A caller that
/// cannot hand [`locate`] the sentinel it wants — because its line starts come
/// from somewhere that does not carry one — calls this directly rather than
/// repeating the arithmetic.
pub fn utf16_column(text: &str, line_start: usize, offset: usize) -> usize {
    let end = offset.min(text.len());
    // Both engines only ever match on character boundaries, so this slice is
    // sound; the clamp above only comes into play for a sentinel offset.
    text.get(line_start..end)
        .map(|prefix| prefix.encode_utf16().count())
        .unwrap_or(0)
}

/// The flattened view of `lines`, joined with `\n`.
fn join_flattened(lines: &[Line]) -> String {
    let mut text = String::new();
    let mut first = true;
    for line in lines.iter().flat_map(Line::flattened) {
        if !first {
            text.push('\n');
        }
        first = false;
        text.push_str(line);
    }
    text
}

/// How many bytes `line` and everything it hides occupy in the flattened text,
/// separators aside.
fn flattened_bytes(line: &Line) -> usize {
    line.text.len()
        + line
            .hidden
            .iter()
            .map(|hidden| hidden.len() + 1)
            .sum::<usize>()
}

/// Where flattened line `index` begins in the flattened text.
///
/// `index` may be the one-past-the-end position, which answers with the end of
/// the text — which is where an insertion at the end belongs.
fn offset_of_flat_line(lines: &[Line], index: usize) -> usize {
    let mut offset = 0usize;
    let mut remaining = index;
    for line in lines {
        if remaining == 0 {
            return offset;
        }
        let count = line.flattened_len();
        if remaining < count {
            // Inside this line: past its own text, then through what it hides.
            offset += line.text.len() + 1;
            let mut inner = remaining - 1;
            for hidden in &line.hidden {
                if inner == 0 {
                    return offset;
                }
                inner -= 1;
                offset += hidden.len() + 1;
            }
            return offset;
        }
        remaining -= count;
        offset += flattened_bytes(line) + 1;
    }
    // Past the end, which is the end of the text rather than the byte after it.
    offset.saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_of(document: &Document) -> Vec<&str> {
        document.iter().collect()
    }

    fn flattened_of(document: &Document) -> Vec<&str> {
        document.flattened_lines().collect()
    }

    /// Everything the document promises must survive every splice: both views,
    /// the text the flattened one joins to, and the offsets into it.
    #[track_caller]
    fn check(document: &Document, collapsed: &[&str], flattened: &[&str]) {
        assert_eq!(lines_of(document), collapsed, "collapsed view");
        assert_eq!(flattened_of(document), flattened, "flattened view");
        assert_eq!(document.line_count(), collapsed.len());
        assert_eq!(document.flattened_len(), flattened.len());

        // The maintained flattened text must be exactly what the lines join to.
        // Every splice is checked against this, which is what makes it safe to
        // keep the text up to date by arithmetic instead of rebuilding it.
        let text = document.flattened();
        assert_eq!(text, flattened.join("\n"), "maintained flattened text");

        // The offset table has to describe the text it claims to.
        let starts = document.flattened_starts();
        assert_eq!(text, flattened.join("\n"), "joined text");
        assert_eq!(
            starts.len(),
            flattened.len() + 1,
            "one start per line, plus a sentinel"
        );
        assert_eq!(
            *starts.last().unwrap(),
            text.len(),
            "sentinel is not the end"
        );
        for (index, line) in flattened.iter().enumerate() {
            let start = starts[index];
            let end = start + line.len();
            assert_eq!(&text[start..end], *line, "flattened line {index}");
            if index + 1 < flattened.len() {
                assert_eq!(
                    text.as_bytes()[end],
                    b'\n',
                    "line {index} has no newline after it"
                );
                assert_eq!(
                    starts[index + 1],
                    end + 1,
                    "line {} does not follow {index}",
                    index + 1
                );
            }
        }
    }

    /// The common case: nothing folded, so both views are the same.
    #[track_caller]
    fn check_flat(document: &Document, expected: &[&str]) {
        check(document, expected, expected);
    }

    #[test]
    fn an_empty_text_is_one_empty_line() {
        let document = Document::from_text("");
        assert_eq!(document.line_count(), 1);
        check_flat(&document, &[""]);
        assert!(document.is_empty());
    }

    #[test]
    fn a_trailing_newline_adds_an_empty_line() {
        check_flat(&Document::from_text("a\nb\n"), &["a", "b", ""]);
        check_flat(&Document::from_text("a\nb"), &["a", "b"]);
    }

    #[test]
    fn from_lines_matches_from_text() {
        let lines = ["a", "", "c"];
        assert_eq!(Document::from_lines(&lines), Document::from_text("a\n\nc"));
    }

    #[test]
    fn replacing_one_line_in_the_middle() {
        let mut document = Document::from_text("a\nb\nc");
        assert_eq!(document.splice(1, 1, &[Line::new("X")]), Ok(true));
        check_flat(&document, &["a", "X", "c"]);
    }

    #[test]
    fn replacing_the_last_line_leaves_no_trailing_newline() {
        let mut document = Document::from_text("a\nb\nc");
        assert_eq!(document.splice(2, 1, &[Line::new("Y")]), Ok(true));
        check_flat(&document, &["a", "b", "Y"]);
    }

    #[test]
    fn removing_the_last_line() {
        // The newline that used to separate it has to go too, or the document
        // would be left with a trailing empty line that nothing asked for.
        let mut document = Document::from_text("a\nb\nc");
        assert_eq!(document.splice(2, 1, &[]), Ok(true));
        check_flat(&document, &["a", "b"]);
    }

    #[test]
    fn removing_every_line_leaves_one_empty_line() {
        let mut document = Document::from_text("a\nb\nc");
        assert_eq!(document.splice(0, 3, &[]), Ok(true));
        check_flat(&document, &[""]);
        assert!(document.is_empty());
    }

    #[test]
    fn inserting_lines_at_the_front_and_back() {
        let mut document = Document::from_text("a\nb");
        assert_eq!(
            document.splice(0, 0, &[Line::new("X"), Line::new("Y")]),
            Ok(true)
        );
        check_flat(&document, &["X", "Y", "a", "b"]);
        assert_eq!(document.splice(4, 0, &[Line::new("Z")]), Ok(true));
        check_flat(&document, &["X", "Y", "a", "b", "Z"]);
    }

    #[test]
    fn replacing_many_lines_with_one_and_back() {
        let mut document = Document::from_text("a\nb\nc\nd");
        assert_eq!(document.splice(1, 2, &[Line::new("X")]), Ok(true));
        check_flat(&document, &["a", "X", "d"]);
        assert_eq!(
            document.splice(1, 1, &[Line::new("X"), Line::new("Y"), Line::new("Z")]),
            Ok(true)
        );
        check_flat(&document, &["a", "X", "Y", "Z", "d"]);
    }

    #[test]
    fn an_edit_that_changes_nothing_is_not_a_change() {
        let mut document = Document::from_text("a\nb\nc");
        let before = document.revision();
        assert_eq!(document.splice(1, 1, &[Line::new("b")]), Ok(false));
        assert_eq!(document.splice(1, 0, &[]), Ok(false));
        assert_eq!(document.revision(), before);
    }

    #[test]
    fn a_real_edit_bumps_the_revision() {
        let mut document = Document::from_text("a\nb");
        let before = document.revision();
        assert_eq!(document.splice(0, 1, &[Line::new("X")]), Ok(true));
        assert_eq!(document.revision(), before + 1);
    }

    #[test]
    fn a_splice_that_does_not_fit_is_refused() {
        let mut document = Document::from_text("a\nb");
        let before = document.clone();
        assert_eq!(
            document.splice(1, 2, &[Line::new("X")]),
            Err(SpliceError {
                start: 1,
                removed: 2,
                line_count: 2
            })
        );
        assert_eq!(
            document.splice(3, 0, &[Line::new("X")]),
            Err(SpliceError {
                start: 3,
                removed: 0,
                line_count: 2
            })
        );
        assert_eq!(
            document, before,
            "a refused splice must leave the document alone"
        );
    }

    #[test]
    fn a_sequence_of_edits_stays_consistent() {
        // The shape a typing session actually has: the Dart side sends one small
        // span per keystroke, walking forward through the document.
        let mut document = Document::from_text("one\ntwo\nthree");
        check_flat(&document, &["one", "two", "three"]);

        assert_eq!(document.splice(0, 1, &[Line::new("ONE!")]), Ok(true));
        check_flat(&document, &["ONE!", "two", "three"]);

        assert_eq!(document.splice(1, 0, &[Line::new("inserted")]), Ok(true));
        check_flat(&document, &["ONE!", "inserted", "two", "three"]);

        assert_eq!(document.splice(1, 2, &[]), Ok(true));
        check_flat(&document, &["ONE!", "three"]);

        // Removing two lines left only two behind, so the last one is at 1.
        assert_eq!(document.splice(1, 1, &[Line::new("THREE")]), Ok(true));
        check_flat(&document, &["ONE!", "THREE"]);
    }

    #[test]
    fn multibyte_lines_survive_intact() {
        let document = Document::from_text("日\n😀\nab");
        check_flat(&document, &["日", "😀", "ab"]);
        assert_eq!(document.flattened(), "日\n😀\nab");
    }

    // --- Folded content -----------------------------------------------------

    #[test]
    fn a_folded_line_hides_its_content_from_one_view_and_not_the_other() {
        // The whole point of the two views: bracket analysis must not see the
        // hidden lines (or it would re-detect a region that is already folded),
        // and search must see them (or it could not find what is folded away).
        let mut document = Document::from_lines(&["{", "}"]);
        let folded = Line::with_hidden("{", vec!["middle".to_string(), "more".to_string()]);
        assert_eq!(document.splice(0, 1, &[folded]), Ok(true));

        check(&document, &["{", "}"], &["{", "middle", "more", "}"]);
    }

    #[test]
    fn unfolding_restores_the_flattened_view() {
        let mut document = Document::from_lines(&["{", "}"]);
        let folded = Line::with_hidden("{", vec!["middle".to_string()]);
        assert_eq!(document.splice(0, 1, &[folded]), Ok(true));
        check(&document, &["{", "}"], &["{", "middle", "}"]);

        // Unfolding replaces the folded line with itself plus its content, so
        // the collapsed view grows and the flattened view is unchanged.
        assert_eq!(
            document.splice(0, 1, &[Line::new("{"), Line::new("middle")]),
            Ok(true)
        );
        check_flat(&document, &["{", "middle", "}"]);
    }

    #[test]
    fn folded_line_count_matches_the_offsets() {
        let mut document = Document::from_lines(&["a", "b"]);
        let folded = Line::with_hidden("a", vec!["x".to_string(), "yy".to_string()]);
        assert_eq!(document.splice(0, 1, &[folded]), Ok(true));

        assert_eq!(document.line_count(), 2);
        assert_eq!(document.flattened_len(), 4);
        check(&document, &["a", "b"], &["a", "x", "yy", "b"]);
    }

    #[test]
    fn an_edit_that_only_changes_what_is_hidden_is_still_a_change() {
        // The collapsed view is identical either way, so a comparison that only
        // looked at it would decide nothing had happened and skip the edit.
        let mut document = Document::from_lines(&["{"]);
        let before = document.clone();
        assert_eq!(
            document.splice(0, 1, &[Line::with_hidden("{", vec!["x".to_string()])]),
            Ok(true)
        );
        assert_ne!(document, before);
        assert_eq!(document.flattened_len(), 2);
    }

    #[test]
    fn folded_content_survives_a_splice_around_it() {
        let mut document = Document::from_lines(&["head", "{", "tail"]);
        let folded = Line::with_hidden("{", vec!["hidden".to_string()]);
        assert_eq!(document.splice(1, 1, &[folded]), Ok(true));
        check(
            &document,
            &["head", "{", "tail"],
            &["head", "{", "hidden", "tail"],
        );

        // A line inserted before it must not disturb what it hides.
        assert_eq!(document.splice(0, 0, &[Line::new("new")]), Ok(true));
        check(
            &document,
            &["new", "head", "{", "tail"],
            &["new", "head", "{", "hidden", "tail"],
        );

        // Nor must one inserted after it.
        assert_eq!(document.splice(4, 0, &[Line::new("end")]), Ok(true));
        check(
            &document,
            &["new", "head", "{", "tail", "end"],
            &["new", "head", "{", "hidden", "tail", "end"],
        );
    }
}
