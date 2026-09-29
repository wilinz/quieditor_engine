//! Highlighting a document in pieces, so an edit costs the lines it touched.
//!
//! The engine highlights from the top every time, because a grammar's state
//! carries from one line to the next: whether line 500 is inside a string
//! depends on what happened on line 3. That makes a keystroke cost the whole
//! document — 871 ms over the package's own sources at 181,000 lines, against
//! 54,000 ms for the Dart implementation this replaced. Sixty times faster, and
//! still a second per keystroke.
//!
//! What makes it cheap is noticing that an edit *usually* leaves the state alone
//! after a few lines: a new bracket, a word, a letter typed inside a comment all
//! end with the engine back where it was. So:
//!
//! * the state at a line boundary is recorded as the document is highlighted;
//! * after an edit, highlighting restarts from the last state recorded *before*
//!   it — those are still true, because the text they describe has not changed;
//! * and it stops as soon as a line boundary produces a state that was recorded
//!   before. Everything after that line would be highlighted the same way, so
//!   there is nothing left to compute.
//!
//! The state is small because the engine empties its buffer at every boundary a
//! state is recorded at. Without that, the state inside an unterminated comment
//! would carry the rest of the document with it — see [`State`].

use crate::highlight::compiler::Grammar;
use crate::highlight::engine::{Element, Engine, Node, State};
use std::collections::HashMap;

/// How many lines apart the states are recorded.
///
/// A state per line would be memory for nothing: what the interval buys is a
/// place to start from near an edit, and the lines between two recordings cost a
/// fraction of a millisecond. Bigger means less memory and more re-scanning.
const EVERY: usize = 32;

/// One scoped span, in byte offsets into the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub scope: String,
    pub start: usize,
    pub end: usize,
    /// How many reported spans enclose this one.
    pub depth: usize,
}

/// What an edit changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    /// The first line whose spans are new.
    pub from: usize,
    /// One past the last line whose spans are new, counting the document as it
    /// is now. From here on, every line has the spans it had before the edit.
    pub to: usize,
    /// One past the last line the client's spans used to cover, counting the
    /// document as it was. A client replaces `from..replaced` of what it holds
    /// with the lines `from..to` of this update — which is the same splice it
    /// made to the text, and leaves its lines numbered like the document's.
    pub replaced: usize,
    /// One past the last line the highlighter now has highlighted, which is the
    /// length the client's cache must have after making that splice. It is the
    /// new watermark: the lines below `to` are the ones the pass stopped on, and
    /// the splice is what brings the cache up to them.
    pub scanned_to: usize,
    /// The spans for `from..to`, in reading order and in document offsets.
    pub spans: Vec<Span>,
}

/// What one forward scan added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// One past the last line the caller already had: the watermark this scan
    /// carried on from. A caller whose own spans do not reach this far has lost
    /// step with the document and has to start again from nothing.
    pub from: usize,
    /// One past the last line this covers, counting the document as it is now —
    /// the new watermark. It is not the line the caller asked for: a rule match
    /// cannot be cut in two, so a scan reaches a line start at or past its
    /// target and no further. The caller takes this as the answer, never its own
    /// request.
    pub to: usize,
    /// The spans of `from..to`, in reading order and in document offsets.
    pub spans: Vec<Span>,
}

/// A state, and the line whose start it was taken at.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    line: usize,
    state: State,
}

/// A document being highlighted in pieces.
#[derive(Debug)]
pub struct Incremental {
    /// The compiled grammar, owned rather than borrowed: a caller that keeps a
    /// highlighter for the life of a document has nowhere to borrow it from, and
    /// the handle that carries this across the FFI cannot hold a reference to
    /// something outside itself.
    grammar: Grammar,
    sub_grammars: HashMap<String, Grammar>,
    /// The document, its lines joined with `\n`.
    text: String,
    /// Where each line begins, in bytes.
    line_starts: Vec<usize>,
    /// The states recorded, in increasing order of line.
    snapshots: Vec<Snapshot>,
    /// One past the last line that has been highlighted: `0..watermark` has been
    /// scanned and `watermark..` has not. `lines()` means all of it.
    ///
    /// A grammar's state carries from one line to the next, so the lines below
    /// this one cannot be highlighted without first highlighting everything
    /// above them. That is what makes this a single number rather than a set:
    /// whatever has been scanned is a prefix, and `snapshots.last().line` is the
    /// line to carry on from.
    watermark: usize,
}

impl Incremental {
    /// Builds a highlighter for `text`, without highlighting any of it.
    ///
    /// Nothing is scanned until [`scan_to`](Self::scan_to) asks for a line, and
    /// a caller that only ever wants the top of a document never pays for the
    /// rest of it. [`spans`](Self::spans) is there for a caller that does want
    /// all of it in one answer.
    ///
    /// A grammar that embeds another language is fine: the piece carrying that
    /// text simply runs to where the embedded mode ends, because a state can
    /// only be taken where the engine is not holding text for another grammar to
    /// read.
    pub fn new(
        grammar: Grammar,
        sub_grammars: HashMap<String, Grammar>,
        text: String,
    ) -> Option<Self> {
        let line_starts = line_starts(&text);
        // The state at the very start of the document, which the first scan has
        // no reason to record — it only records where it stops — but an edit in
        // the first `EVERY` lines has to have somewhere to resume from. It costs
        // the grammar, not the text.
        let first = Engine::start(&grammar, &sub_grammars, &text).state();
        Some(Self {
            grammar,
            sub_grammars,
            text,
            line_starts,
            snapshots: vec![Snapshot {
                line: 0,
                state: first,
            }],
            watermark: 0,
        })
    }

    /// One past the last line that has been highlighted, and the line the next
    /// scan carries on from.
    pub fn watermark(&self) -> usize {
        self.watermark
    }

    /// Highlights everything up to `to_line`, carrying on from where the last
    /// call stopped, and answers with the spans of the lines it added.
    ///
    /// The document is highlighted in one forward pass however many times this
    /// is called: a line's scopes depend on the state the line above it left, so
    /// there is no way to reach a line without having been through the ones
    /// before it. Asking for a line already covered costs nothing.
    pub fn scan_to(&mut self, to_line: usize) -> Scan {
        let from = self.watermark;
        let to_line = to_line.min(self.lines());
        if to_line <= from {
            return Scan {
                from,
                to: from,
                spans: Vec::new(),
            };
        }
        // A state every `EVERY` lines, to resume an edit from, and one at the
        // line being reached for, so that the watermark is always a line a later
        // call can carry on from.
        let mut record_at: Vec<usize> = (from..to_line)
            .filter(|line| line % EVERY == 0 && *line > from)
            .collect();
        record_at.push(to_line);
        let (spans, stopped, recorded) = self.scan(from, &record_at, &self.snapshots, to_line);
        self.snapshots.extend(recorded);
        self.watermark = stopped;
        Scan {
            from,
            to: stopped,
            spans,
        }
    }

    /// How many lines the document has.
    pub fn lines(&self) -> usize {
        self.line_starts.len()
    }

    /// The document as it stands.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Every span in the document, in reading order.
    ///
    /// Walks the whole document again rather than keeping the spans around:
    /// they are what the caller caches, and a second copy here would double what
    /// a large document costs in memory to save a walk that only happens when a
    /// caller has thrown its own cache away.
    ///
    /// Takes `&mut self` because this pass records as it goes, which is what a
    /// later edit resumes from. Nothing has been scanned yet at the point a
    /// caller usually wants everything — the highlighter has just been built —
    /// and then this answer *is* the first scan's, so the walk happens once
    /// rather than twice.
    pub fn spans(&mut self) -> Vec<Span> {
        let from = self.watermark;
        let scan = self.scan_to(self.lines());
        if from == 0 {
            return scan.spans;
        }
        // Partway down already: the spans above the watermark were handed out
        // and not kept, so the only way back to all of them is to walk it again.
        self.scan(0, &[], &[], self.lines()).0
    }

    /// The byte offset each line begins at.
    pub fn line_starts(&self) -> &[usize] {
        &self.line_starts
    }

    /// The first line start after `at`, or nothing when it is the last one.
    ///
    /// A carry-forward asks this once per line it runs on, so it is a binary
    /// search rather than a walk over every line start in the document.
    fn next_line_start(&self, at: usize) -> Option<usize> {
        let index = self.line_starts.partition_point(|start| *start <= at);
        self.line_starts.get(index).copied()
    }

    /// The line a byte offset falls in.
    fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        }
    }

    /// Replaces `removed` lines at `start` with `added`, and highlights what that
    /// changed.
    ///
    /// The work is the lines between the last recorded state before the edit and
    /// the line where the state is what it was: one line for a keystroke inside a
    /// comment, and as many lines as the brace encloses for a keystroke that
    /// opens one. It never runs past what has been highlighted, so a document
    /// someone has only looked at the top of costs an edit in its top and
    /// nothing at all below it.
    pub fn update(&mut self, start: usize, removed: usize, added: &[&str]) -> Update {
        let old_lines = self.lines();
        // Read before the edit, which is the document it describes.
        let old_watermark = self.watermark;
        let start = start.min(old_lines);
        let removed = removed.min(old_lines - start);

        let from_byte = self.line_starts[start];
        let removed_to = self
            .line_starts
            .get(start + removed)
            .copied()
            .unwrap_or(self.text.len());
        // The replaced span runs to the start of the next line, so it takes the
        // line break that ended the last of the removed lines with it. Whatever
        // replaces it has to bring a line break back — otherwise the edit joins
        // two lines into one and every line after it moves up — but only when
        // something follows, and only when the replacement has a line to end.
        let removed_bytes = removed_to - from_byte;
        let added_text = added.join("\n");
        let separator = if start + removed < old_lines && !added.is_empty() {
            "\n"
        } else {
            ""
        };
        self.text.replace_range(
            from_byte..from_byte + removed_bytes,
            &format!("{added_text}{separator}"),
        );
        self.line_starts = line_starts(&self.text);

        let delta_lines = added.len() as isize - removed as isize;

        // An edit at or past everything the highlighter has highlighted changes
        // nothing it knows: both the caller's spans and the states stop at the
        // watermark. This is the shape of typing on past the end of a document,
        // which is worth not walking one for.
        if start >= old_watermark {
            return Update {
                from: old_watermark,
                to: old_watermark,
                replaced: old_watermark,
                scanned_to: old_watermark,
                spans: Vec::new(),
            };
        }

        // The furthest line of the new document this pass may claim to know.
        // Either the edit reached the end of what was known and took the rest
        // with it, or everything below shifted and is still known where it now
        // sits. Without this a scan runs to the end of the document — which is
        // right when the whole document is known and wrong, expensively so, when
        // only its top has been looked at.
        let cap = if start + removed >= old_watermark {
            start + added.len()
        } else {
            shift(old_watermark, delta_lines)
        };

        // Where to start from: the last state recorded at or before the edit.
        // Every state before it describes text that is still there, and the one
        // at it describes the line start the edit is on.
        //
        // Nothing recorded at or before the edit means the states have lost
        // their foothold at the top of the document. That happens: the merge
        // below keeps `shifted[..resume]`, which is empty when the last edit
        // resumed from line 0, and the states it records start at the first
        // target below that rather than at 0. So this is not dead code — an edit
        // near the top after one that resumed from the top lands here, and what
        // it costs is everything up to the cap.
        let resume = match self.snapshots.iter().rposition(|s| s.line <= start) {
            Some(index) => index,
            None => {
                let mut record_at: Vec<usize> = (0..cap).step_by(EVERY).collect();
                record_at.push(cap);
                let (spans, stopped, mut recorded) = self.scan(0, &record_at, &[], cap);
                // The state at the very start, which a pass from line 0 has no
                // reason to record — it only records where it stops. Without it
                // every later pass over the top of the document lands here too.
                recorded.insert(
                    0,
                    Snapshot {
                        line: 0,
                        state: Engine::start(&self.grammar, &self.sub_grammars, &self.text).state(),
                    },
                );
                self.snapshots = recorded;
                self.watermark = cap.max(stopped);
                return Update {
                    from: 0,
                    to: stopped,
                    replaced: old_watermark,
                    scanned_to: self.watermark,
                    spans,
                };
            }
        };
        let from_line = self.snapshots[resume].line;

        // The states after the edit describe text that has moved, which is the
        // only thing that has changed about them — except for the ones taken
        // inside the replaced span, which describe text that is gone. Shifting
        // one of those instead leaves a state sitting at a line it does not
        // belong to, and the stop test below compares line numbers alone, so it
        // can stop a pass early and report a document that changed all the way
        // down as unchanged from there. Dropping them costs nothing: the pass
        // records a state at every one of these lines anyway.
        let shifted: Vec<Snapshot> = self
            .snapshots
            .iter()
            .enumerate()
            .filter_map(|(index, snapshot)| {
                if index <= resume {
                    Some(snapshot.clone())
                } else if snapshot.line >= start + removed {
                    Some(Snapshot {
                        line: shift(snapshot.line, delta_lines),
                        state: snapshot.state.clone(),
                    })
                } else {
                    None
                }
            })
            .collect();

        // Where the state is looked for on the way down: the lines the previous
        // pass recorded after this one. Finding one there is what says the rest
        // of the document is unchanged.
        let record_at: Vec<usize> = shifted
            .iter()
            .skip(resume + 1)
            .map(|snapshot| snapshot.line)
            .collect();
        let (spans, stopped, recorded) = self.scan(from_line, &record_at, &shifted, cap);

        // The states before the re-highlighted range are still right. The ones
        // this pass took replace everything up to where it stopped, and the ones
        // after that are the shifted originals: they describe the same text with
        // the same state, further down the document.
        let mut merged: Vec<Snapshot> = shifted[..resume].to_vec();
        merged.extend(recorded);
        for snapshot in shifted.iter().skip(resume + 1) {
            if snapshot.line < stopped {
                continue;
            }
            // This pass ends with a state at exactly the line it stopped on, so
            // the shifted original of that line is the same state over again.
            // Keeping both would put two entries at one line, and every search
            // over these assumes there is at most one.
            if merged
                .last()
                .map_or(false, |last| last.line >= snapshot.line)
            {
                continue;
            }
            merged.push(snapshot.clone());
        }
        self.snapshots = merged;
        // Everything up to the cap is known again: the lines this pass computed,
        // and below it the ones it stopped on because their states say the rest
        // is what it was. Past the cap nothing was known before the edit either,
        // and nothing is invented now.
        self.watermark = cap.max(stopped);

        Update {
            from: from_line,
            to: stopped,
            // What it replaced, in the document as it was: the lines it
            // highlighted, less the ones the edit added and plus the ones it
            // took away.
            replaced: shift(stopped, -delta_lines),
            scanned_to: self.watermark,
            spans,
        }
    }

    /// Highlights from the start of `from_line` no further than the start of
    /// `limit_line`, recording a state at each line in `record_at`, and stopping
    /// early at the first line whose state `previous` also has.
    ///
    /// Returns the spans of everything it scanned, the line it stopped at, and
    /// the states it recorded. `stopped` is one past the last line whose spans
    /// were computed, so it is also where the caller's cached spans become valid
    /// again.
    fn scan(
        &self,
        from_line: usize,
        record_at: &[usize],
        previous: &[Snapshot],
        limit_line: usize,
    ) -> (Vec<Span>, usize, Vec<Snapshot>) {
        let from = self
            .line_starts
            .get(from_line)
            .copied()
            .unwrap_or(self.text.len());
        let limit = self
            .line_starts
            .get(limit_line)
            .copied()
            .unwrap_or(self.text.len());
        // The whole text from `from`, not the part of it up to `limit`. A rule
        // that matches before the limit is followed wherever it goes, so the
        // engine has to be able to read past it — and `Engine::limit` is what
        // stops it, at a match *starting* past the bound rather than at a byte.
        // Cutting the slice here would cut a match in two and leave the next
        // piece to re-decide text this one has already decided about.
        let code = &self.text[from..];
        let mut engine = match previous.iter().find(|snapshot| snapshot.line == from_line) {
            Some(snapshot) => {
                Engine::resume(&self.grammar, &self.sub_grammars, code, &snapshot.state)
            }
            None => Engine::start(&self.grammar, &self.sub_grammars, code),
        };

        let mut spans = Vec::new();
        let mut recorded = Vec::new();
        let mut piece_at = from;
        for target in record_at.iter().copied() {
            let Some(mut at) = self.line_starts.get(target).copied() else {
                break;
            };
            if at <= from {
                continue;
            }
            // A piece that had to carry on past its target leaves the engine
            // ahead of the targets between here and there. Walking them again
            // would scan nothing and record the same line over and over.
            if at <= engine.position() + from {
                continue;
            }
            if at > limit {
                break;
            }
            engine.scan_until(at - from);
            // A state can only be taken at a line start, and only where nothing
            // that embeds another language is open — see
            // `Engine::embeds_a_language`. Either way the piece carries on to
            // the next line start, and to the one after that, until it can stop
            // somewhere a state means what it says.
            while engine.position() + from > at || engine.embeds_a_language() {
                match self.next_line_start(at) {
                    Some(next) => {
                        at = next;
                        engine.scan_until(at - from);
                    }
                    None => break,
                }
            }
            let stopped = engine.position() + from;
            let tree = engine.take_tree();
            collect(&tree, piece_at, 0, &mut spans);
            piece_at = stopped;

            // Which line this state is at, which is not `target` when a rule
            // ran past the line start and the piece had to carry on to the next
            // one.
            let at_line = self.line_of(stopped);
            let state = engine.state();
            if previous
                .iter()
                .find(|snapshot| snapshot.line == at_line)
                .map_or(false, |snapshot| snapshot.state == state)
            {
                // The state here is the one the pass before recorded at the same
                // line, so every line from here on comes out the same and there
                // is nothing left to compute.
                return (spans, at_line, recorded);
            }
            // A carry-forward that overshot lands on a line a later target also
            // names. The states are kept in increasing line order and a second
            // entry for one line would break every search over them.
            if recorded.last().map_or(false, |last| last.line >= at_line) {
                continue;
            }
            recorded.push(Snapshot {
                line: at_line,
                state,
            });
        }

        // Whatever is left of the document, from wherever the last piece
        // stopped, up to where this scan was asked to reach. The end of the text
        // is the last line start only when the text ends with a newline: the
        // final line has no start of its own past it, so reading the last start
        // instead of the end left the text after it unscanned — one line's worth
        // of spans, or the whole document when there is only one line.
        if limit > engine.position() + from {
            engine.scan_until(limit - from);
            let tree = engine.take_tree();
            collect(&tree, piece_at, 0, &mut spans);
        }

        // Where this scan stopped, which is a line start at or past the line it
        // was asked to reach: a rule that ran past the bound, or a sub-language
        // still open at it, is followed to the next line start that can carry a
        // state. A state is recorded there whatever happened above, so that
        // `snapshots` always reaches exactly the watermark and the next scan can
        // carry on without re-reading anything.
        let stopped = engine.position() + from;
        let at_line = self.stop_line(stopped);
        let state = engine.state();
        if recorded.last().map_or(true, |last| last.line < at_line) {
            recorded.push(Snapshot {
                line: at_line,
                state,
            });
        }
        (spans, at_line, recorded)
    }

    /// The line a position stops at: the line past the last one when it is the
    /// end of the text, because a state taken there is the one a line appended
    /// later resumes from.
    fn stop_line(&self, position: usize) -> usize {
        if position >= self.text.len() {
            self.lines()
        } else {
            self.line_of(position)
        }
    }
}

fn shift(line: usize, by: isize) -> usize {
    (line as isize + by).max(0) as usize
}

/// Walks a piece's tree, reporting every scope it found in document offsets.
///
/// `at` is where the node's text starts; the return value is where it ends, which
/// is what makes the spans nest — a node's text is its children's text and
/// nothing else.
///
/// A node with no scope is not reported and does not count towards the depth: it
/// colours nothing, so a client never sees it, and a depth that counted it would
/// not be a depth in the list a client has.
pub fn collect(node: &Node, at: usize, depth: usize, into: &mut Vec<Span>) -> usize {
    let mut cursor = at;
    for child in &node.children {
        match child {
            Element::Text(text) => cursor += text.len(),
            Element::Node(nested) => {
                let start = cursor;
                let scope = nested.scope.as_deref().filter(|scope| !scope.is_empty());
                // Recorded before the children are walked: the end is not known
                // until they are, so the placeholder is filled in below.
                let recorded = scope.map(|scope| {
                    into.push(Span {
                        scope: scope.to_string(),
                        start,
                        end: start,
                        depth,
                    });
                    into.len() - 1
                });
                cursor = collect(
                    nested,
                    cursor,
                    depth + usize::from(recorded.is_some()),
                    into,
                );
                if let Some(index) = recorded {
                    into[index].end = cursor;
                }
            }
        }
    }
    cursor
}

/// The byte offset each line of `text` begins at.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::highlight::{compile_json, highlight};

    /// The scopes of one line: the scope, and where it starts and ends within
    /// the line. Offsets inside a line are what makes two passes comparable —
    /// they do not move when lines above are added or removed.
    type Line = Vec<(String, usize, usize)>;

    /// Every line's scopes, from a flat list of spans in document offsets.
    fn per_line(spans: &[Span], line_starts: &[usize], text: &str) -> Vec<Line> {
        let lines = line_starts.len();
        let mut out = vec![Line::new(); lines];
        for span in spans {
            let mut line = line_of(line_starts, span.start);
            while line < lines && line_starts[line] < span.end {
                let from = line_starts[line].max(span.start);
                let to = line_starts
                    .get(line + 1)
                    .copied()
                    .unwrap_or(text.len())
                    .min(span.end);
                if to > from {
                    out[line].push((
                        span.scope.clone(),
                        from - line_starts[line],
                        to - line_starts[line],
                    ));
                }
                line += 1;
            }
        }
        out
    }

    fn line_of(line_starts: &[usize], offset: usize) -> usize {
        match line_starts.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        }
    }

    /// What highlighting the whole document from scratch says, line by line.
    fn from_scratch(grammar: &Grammar, text: &str) -> Vec<Line> {
        from_scratch_with(grammar, &HashMap::new(), text)
    }

    /// The same, for a grammar whose modes embed other languages.
    fn from_scratch_with(
        grammar: &Grammar,
        sub_grammars: &HashMap<String, Grammar>,
        text: &str,
    ) -> Vec<Line> {
        let out = highlight(grammar, sub_grammars, text);
        let mut spans = Vec::new();
        collect(&out.root, 0, 0, &mut spans);
        per_line(&spans, &line_starts(text), text)
    }

    fn driver(text: &str) -> (Incremental, Vec<Line>) {
        let grammar = compile_json(GRAMMAR).expect("the grammar compiles");
        let mut driver = Incremental::new(grammar, HashMap::new(), text.to_string())
            .expect("this grammar can be highlighted in pieces");
        let spans = driver.spans();
        let lines = per_line(&spans, driver.line_starts(), text);
        (driver, lines)
    }

    /// A grammar with strings that nest, comments that end at a line, keywords
    /// and a mode that contains itself — which is what a real one reaches for.
    const GRAMMAR: &str = r##"{
        "name": "Doc",
        "keywords": {"keyword": "def end if while"},
        "contains": [
            {
                "scope": "string",
                "begin": "\"",
                "end": "\"",
                "contains": [{"scope": "subst", "begin": "#\\{", "end": "\\}",
                              "contains": [{"selfReferential": true}]}]
            },
            {"scope": "comment", "begin": "//", "end": "$"},
            {"scope": "paren", "begin": "\\{", "end": "\\}",
             "contains": [{"selfReferential": true}]}
        ]
    }"##;

    /// A document of a few hundred lines, in the shape above.
    fn document(lines: usize) -> String {
        let mut out = String::new();
        for line in 0..lines {
            match line % 7 {
                0 => out.push_str("def name(arg)\n"),
                1 => out.push_str("  // a comment\n"),
                2 => out.push_str("  value = \"text\"\n"),
                3 => out.push_str("  {\n"),
                4 => out.push_str("    nested { inner }\n"),
                5 => out.push_str("  }\n"),
                _ => out.push_str("end\n"),
            }
        }
        out
    }

    #[test]
    fn a_new_highlighter_has_highlighted_nothing() {
        // Building one costs the grammar, not the text — which is the whole
        // point: opening a large document must not wait for the end of it. What
        // it costs is the state at line 0, which every scan carries on from.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let incremental =
            Incremental::new(grammar, HashMap::new(), document(4_000)).expect("a highlighter");
        assert_eq!(incremental.watermark(), 0);
        assert_eq!(incremental.snapshots.len(), 1);
        assert_eq!(incremental.snapshots[0].line, 0);
    }

    #[test]
    fn scanning_in_pieces_answers_the_lines_it_added() {
        // What a caller draws with: one call's spans cover the lines since the
        // last call, and no others. The lines are in document offsets, so the
        // answer for a chunk is comparable to the same lines of a whole scan.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let text = document(300);
        let starts = line_starts(&text);
        let mut incremental =
            Incremental::new(grammar, HashMap::new(), text.clone()).expect("a highlighter");

        let mut mine: Vec<Line> = Vec::new();
        let mut scanned = 0;
        for chunk in [EVERY, EVERY * 3, 1, EVERY * 2, 7, 1_000] {
            let scan = incremental.scan_to(scanned + chunk);
            assert_eq!(scan.from, scanned, "a scan carries on from the last one");
            assert!(scan.to > scanned, "asking for more lines scans some");
            // Every span lies within the lines this scan claims.
            let from_byte = starts[scan.from.min(starts.len() - 1)];
            let to_byte = starts.get(scan.to).copied().unwrap_or(text.len());
            for span in &scan.spans {
                assert!(
                    span.start >= from_byte && span.end <= to_byte,
                    "a span of {}..{} is outside the lines {}..{} it came from",
                    span.start,
                    span.end,
                    scan.from,
                    scan.to
                );
            }
            mine.extend(
                per_line(&scan.spans, &starts, &text)[scan.from..scan.to]
                    .iter()
                    .cloned(),
            );
            scanned = scan.to;
        }

        assert_eq!(
            scanned,
            incremental.lines(),
            "the whole document was reached"
        );
        assert_eq!(mine, from_scratch(&compile_json(GRAMMAR).unwrap(), &text));
    }

    #[test]
    fn a_pass_from_the_top_leaves_no_state_at_the_top_behind_it() {
        // Why `update` keeps a branch for finding no state at or before the edit,
        // rather than the edit being impossible. A pass keeps the states it did
        // not recompute by line range, and a pass resuming from line 0 recomputes
        // from there — so the states it leaves begin at the first target *below*
        // line 0, and the state at line 0 is gone. The next edit near the top has
        // nothing to resume from, and the branch is what answers it.
        //
        // Deleting the branch as unreachable leaves the second edit below with
        // nowhere to start. It was tried.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let text = document(600);
        let mut incremental =
            Incremental::new(grammar, HashMap::new(), text).expect("a highlighter");
        incremental.scan_to(incremental.lines());
        assert_eq!(incremental.snapshots.first().map(|s| s.line), Some(0));

        incremental.update(1, 1, &["end"]);
        assert_ne!(
            incremental.snapshots.first().map(|s| s.line),
            Some(0),
            "a pass from the top leaves no state at line 0 behind it"
        );

        // And the edit that lands here still answers correctly.
        let update = incremental.update(1, 1, &["end"]);
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let expected = from_scratch(&grammar, incremental.text());
        let cache = per_line(&update.spans, incremental.line_starts(), incremental.text());
        for (line, want) in expected.iter().enumerate().take(update.to) {
            assert_eq!(&cache[line], want, "line {line} after the second edit");
        }
    }

    #[test]
    fn an_edit_that_has_not_been_looked_at_costs_nothing() {
        // Highlighting lazily means the document past the watermark has not been
        // looked at, so an edit down there changes nothing that is known — and
        // this is the shape of typing on past the end of a file.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let mut incremental =
            Incremental::new(grammar, HashMap::new(), document(300)).expect("a highlighter");
        let looked_at = incremental.scan_to(64).to;

        let update = incremental.update(200, 1, &["end"]);
        assert_eq!(
            (update.from, update.to, update.replaced, update.scanned_to),
            (looked_at, looked_at, looked_at, looked_at),
            "an edit past the watermark answers with nothing to do"
        );
        assert!(update.spans.is_empty());
        assert_eq!(incremental.watermark(), looked_at);
    }

    #[test]
    fn an_edit_in_a_partly_scanned_document_does_not_scan_the_rest() {
        // The cap: a pass may not claim to know more than was known before it,
        // unless the edit is itself what added the lines. Without it, an edit
        // near the top of a document someone has only looked at the top of would
        // walk the whole thing — the laziness undone by the first keystroke.
        //
        // The edit has to be one the pass cannot stop early on, which is what
        // the cap is for: replacing a closing brace with an opening one leaves
        // the state at every line below different from the one the last pass
        // recorded, so there is no recorded state to stop at and only the cap
        // ends the walk.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let text = document(5_000);
        let mut incremental =
            Incremental::new(grammar, HashMap::new(), text).expect("a highlighter");
        let looked_at = incremental.scan_to(64).to;

        let update = incremental.update(5, 1, &["  {"]);
        assert!(
            update.to <= looked_at + EVERY,
            "an edit at line 5 of a document looked at to line {looked_at} \
             highlighted through line {}",
            update.to
        );
        assert_eq!(incremental.watermark(), update.scanned_to);
    }

    #[test]
    fn an_edit_after_a_partial_scan_leaves_the_cache_the_length_it_says() {
        // What the client does on the other side of the boundary: replace
        // `from..replaced` of its cache with `from..to` of the answer. The
        // length it ends up with is the length the highlighter now covers, and
        // the two have to agree or the lines past the shorter one are drawn
        // plain for the rest of the session.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let mut incremental =
            Incremental::new(grammar, HashMap::new(), document(600)).expect("a highlighter");

        let scan = incremental.scan_to(100);
        // `per_line` answers for every line of the document; the client only
        // holds the ones this scan covered.
        let mut cache = per_line(&scan.spans, incremental.line_starts(), incremental.text())
            [scan.from..scan.to]
            .to_vec();
        assert_eq!(cache.len(), incremental.watermark());
        let before = cache.len();

        let update = incremental.update(3, 1, &["  value = \"changed\""]);
        let new_lines = per_line(&update.spans, incremental.line_starts(), incremental.text());
        cache.splice(
            update.from..update.replaced,
            new_lines[update.from..update.to].iter().cloned(),
        );
        assert_eq!(
            cache.len(),
            update.scanned_to,
            "the cache came out {} long and the answer said {}",
            cache.len(),
            update.scanned_to
        );
        assert_eq!(cache.len(), incremental.watermark());
        assert_eq!(cache.len(), before, "the edit added no lines and took none");

        // And what it holds for the lines it covers is a fresh pass's answer.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        let expected = from_scratch(&grammar, incremental.text());
        for (line, (got, want)) in cache.iter().zip(expected.iter()).enumerate() {
            assert_eq!(got, want, "line {line} after the edit");
        }
    }

    #[test]
    fn highlighting_in_pieces_matches_highlighting_all_of_it() {
        // The pieces empty the engine's buffer at every boundary, which is what
        // a state can be taken at. That must not change what the scopes come out
        // as, or none of the rest of this means anything.
        let text = document(120);
        let (_, lines) = driver(&text);
        let grammar = compile_json(GRAMMAR).expect("compiles");
        assert_eq!(lines, from_scratch(&grammar, &text));
    }

    const OUTER_GRAMMAR: &str = r##"{
        "name": "Outer",
        "contains": [
            {"scope": "meta", "begin": "<%", "end": "%>", "subLanguage": "inner"},
            {"scope": "comment", "begin": "//", "end": "$"}
        ]
    }"##;

    fn sub_grammars_for() -> HashMap<String, Grammar> {
        let mut sub_grammars = HashMap::new();
        sub_grammars.insert(
            "inner".to_string(),
            compile_json(
                r#"{
                    "name": "Inner",
                    "contains": [{"scope": "string", "begin": "\"", "end": "\""}]
                }"#,
            )
            .expect("compiles"),
        );
        sub_grammars
    }

    #[test]
    fn a_grammar_that_embeds_another_language_is_highlighted_the_same() {
        // The embedded language's own state carries across the lines it spans,
        // so a piece cannot end inside it: emptying the buffer there would hand
        // the embedded grammar a block of its text in pieces. What the pieces
        // run to is where that mode ends — and the answer still has to be what
        // highlighting the whole document gives.
        // A block of the embedded language spanning lines, half way down a
        // document long enough to have states recorded either side of it.
        let mut text = String::new();
        for line in 0..200 {
            match line {
                37 => text.push_str("<%\n"),
                38..=45 => text.push_str("  \"still inside\"\n"),
                46 => text.push_str("%>\n"),
                _ if line % 5 == 0 => text.push_str("// a comment\n"),
                _ => text.push_str("plain text\n"),
            }
        }

        let mut incremental = Incremental::new(
            compile_json(OUTER_GRAMMAR).expect("compiles"),
            sub_grammars_for(),
            text.clone(),
        )
        .expect("a highlighter");
        let spans = incremental.spans();

        // The first pass, and then an edit inside the embedded block, worked
        // through a cache the way a client would.
        let mut cache = per_line(&spans, incremental.line_starts(), &text);
        let grammar = compile_json(OUTER_GRAMMAR).expect("compiles");
        let sub_grammars = sub_grammars_for();
        assert_eq!(cache, from_scratch_with(&grammar, &sub_grammars, &text));

        let edits: Vec<(usize, usize, Vec<&str>)> = vec![
            (40, 1, vec!["  \"changed\""]),
            (38, 1, vec!["  <% nested %>"]),
            (5, 1, vec!["// another comment"]),
        ];
        for (start, removed, added) in edits {
            let update = incremental.update(start, removed, &added);
            let new_lines = per_line(&update.spans, incremental.line_starts(), incremental.text());
            cache.splice(
                update.from..update.replaced,
                new_lines[update.from..update.to].iter().cloned(),
            );
            let expected = from_scratch_with(&grammar, &sub_grammars, incremental.text());
            for (line, (got, want)) in cache.iter().zip(expected.iter()).enumerate() {
                assert_eq!(got, want, "line {line} after editing {start}");
            }
        }
    }

    #[test]
    fn an_edit_in_a_large_document_stops_where_the_state_is_unchanged() {
        // The point of the whole exercise: a keystroke near the top of a long
        // document must not re-highlight the document. What bounds the work is
        // the next recorded state down, so the answer is tens of lines — and if
        // this ever stops being true, everything else here still passes while
        // nothing is gained.
        //
        // Each case gets its own document: an edit that leaves a brace
        // unbalanced really does change the state all the way to the end, and
        // that is the one case where re-highlighting everything is right.
        let text = document(5_000);

        let (mut incremental, _) = driver(&text);
        let update = incremental.update(3, 1, &["  value = \"changed\""]);
        assert!(
            update.to - update.from <= EVERY,
            "typing inside a line re-highlighted {} lines of 5000",
            update.to - update.from
        );

        let (mut incremental, _) = driver(&text);
        let update = incremental.update(3, 1, &["  {", "    extra { x }"]);
        assert!(
            update.to - update.from <= EVERY * 2,
            "a balanced brace re-highlighted {} lines",
            update.to - update.from
        );

        let (mut incremental, _) = driver(&text);
        let update = incremental.update(4990, 1, &["end"]);
        assert!(
            update.to - update.from <= EVERY,
            "an edit near the end re-highlighted {} lines",
            update.to - update.from
        );
    }

    #[test]
    fn an_edit_that_takes_recorded_states_with_it_stays_in_step() {
        // Deleting a run of lines longer than `EVERY` takes recorded states with
        // it: they describe text that is gone. Shifting them instead of dropping
        // them leaves a state at a line it does not belong to, and the stop test
        // compares line numbers alone — so a pass can stop on one of them and
        // report the rest of the document as unchanged when the edit changed all
        // of it, leaving both the cache and the states wrong from there down.
        //
        // Where a stale state lands depends on how the edit lines up with the
        // `EVERY` grid, and only some alignments put one past the resume point
        // where it can be reached. So this sweeps the alignments rather than
        // picking one: a single edit either misses it or is the whole test.
        let grammar = compile_json(GRAMMAR).expect("compiles");
        for start in (0..160).step_by(11) {
            for removed in [EVERY + 1, EVERY * 2, EVERY * 3 + 5] {
                let text = document(600);
                let (mut incremental, mut cache) = driver(&text);
                let update = incremental.update(start, removed, &["end"]);

                assert!(
                    incremental
                        .snapshots
                        .windows(2)
                        .all(|pair| pair[0].line < pair[1].line),
                    "deleting {removed} lines at {start} left the states out of \
                     order: {:?}",
                    incremental
                        .snapshots
                        .iter()
                        .map(|snapshot| snapshot.line)
                        .collect::<Vec<_>>()
                );

                let new_lines =
                    per_line(&update.spans, incremental.line_starts(), incremental.text());
                cache.splice(
                    update.from..update.replaced,
                    new_lines[update.from..update.to].iter().cloned(),
                );

                let expected = from_scratch(&grammar, incremental.text());
                assert_eq!(
                    cache.len(),
                    expected.len(),
                    "deleting {removed} lines at {start}: the cache is as long as \
                     the document"
                );
                for (line, (got, want)) in cache.iter().zip(expected.iter()).enumerate() {
                    assert_eq!(
                        got, want,
                        "line {line} after deleting {removed} lines at {start}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_edit_only_changes_the_lines_it_touches() {
        let text = document(120);
        let (mut incremental, mut cache) = driver(&text);
        let grammar = compile_json(GRAMMAR).expect("compiles");
        // The document as the edits below should leave it. Comparing the
        // highlighter's own text against this is what catches a splice that ate
        // a line break or left one behind: without it, a highlighter that
        // mangled the document it was editing would still agree with itself.
        let mut expected_text: Vec<String> = text.lines().map(str::to_string).collect();

        // Typing, deleting, folding a brace away, and reopening it: the cases a
        // keystroke actually produces.
        let edits: Vec<(usize, usize, Vec<&str>)> = vec![
            (2, 1, vec!["  value = \"text more\""]),
            (9, 1, vec![]),
            (20, 1, vec!["  // a comment", "  // and another"]),
            (
                33,
                1,
                vec!["  {", "    nested { deeper { deepest } }", "  }"],
            ),
            (40, 0, vec!["def added()"]),
            (41, 3, vec!["end"]),
        ];

        for (index, (start, removed, added)) in edits.iter().enumerate() {
            expected_text.splice(
                *start..start + removed,
                added.iter().map(|line| (*line).to_string()),
            );
            let update = incremental.update(*start, *removed, added);
            assert_eq!(
                incremental
                    .text()
                    .lines()
                    .map(str::to_string)
                    .collect::<Vec<_>>(),
                expected_text,
                "edit {index} left the document itself wrong"
            );
            // What a client does with it: replace the lines that changed, in a
            // cache indexed by line. The lines after them move down or up with
            // the splice, which is exactly what their line numbers do.
            let new_lines = per_line(&update.spans, incremental.line_starts(), incremental.text());
            // The update's own spans must say what a highlight from scratch says
            // about the lines it claims.
            let fresh = from_scratch(&grammar, incremental.text());
            for line in update.from..update.to {
                assert_eq!(
                    new_lines.get(line).unwrap_or(&Line::new()),
                    fresh.get(line).unwrap_or(&Line::new()),
                    "edit {index}, line {line} of the update"
                );
            }
            cache.splice(
                update.from..update.replaced,
                new_lines[update.from..update.to].iter().cloned(),
            );

            let expected = from_scratch(&grammar, incremental.text());
            // A document ending in a line break has one more line of text than
            // the editor has lines, and the extra one is empty and never asked
            // for. The cache carries it so that its lines line up with the
            // document's; the comparison is over the lines that exist.
            assert!(cache.len() >= expected.len(), "edit {index} lost lines");
            for (line, (got, want)) in cache.iter().zip(expected.iter()).enumerate() {
                assert_eq!(got, want, "edit {index}, line {line}");
            }
            for (line, extra) in cache.iter().enumerate().skip(expected.len()) {
                assert!(
                    extra.is_empty(),
                    "edit {index}, trailing line {line} is not empty"
                );
            }
        }
    }
}
