//! The highlighting loop.
//!
//! A port of `_highlight` from `re_highlight`, which is itself a port of
//! highlight.js. The algorithm is the same in all three, and deliberately so:
//! it walks the text, at each position asking the current mode's matcher for
//! the nearest match, and reacts to whichever rule it was — enter a sub-mode,
//! end the current one, colour a keyword, or reject an illegal lexeme.
//!
//! Where this reads oddly, the oddity is inherited from highlight.js, and the
//! comments say which grammar behaviour it is there for. Changing one changes
//! what the editor colours.

use crate::highlight::compiler::{CompiledScope, Grammar, Mode, ModeId, Rule};
use crate::highlight::mode::{Callback, SubLanguage};
use crate::highlight::regex::{Dialect, MultiRegex, RuleMatch};
use std::collections::HashMap;

/// How many times one keyword may count towards relevance.
///
/// A word repeated through a file says no more about its language than its
/// first occurrence, and counting every one would let a long file's score be
/// dominated by whichever word it uses most.
const MAX_KEYWORD_HITS: usize = 7;

/// A finished piece of highlighting.
#[derive(Debug, Clone)]
pub struct Highlighted {
    pub root: Node,
    /// How much this text looks like the grammar it was highlighted with.
    pub relevance: f64,
    /// Set when the grammar rejected the text outright.
    pub illegal: bool,
}

/// A run of text, possibly under a scope.
#[derive(Debug, Clone, Default)]
pub struct Node {
    /// The class name a theme keys on, such as `keyword` or `string`.
    pub scope: Option<String>,
    /// Set on a node produced by a sub-language, as `language:xml`.
    pub language: Option<String>,
    pub children: Vec<Element>,
}

/// A node's contents: text, or another node.
#[derive(Debug, Clone)]
pub enum Element {
    Text(String),
    Node(Node),
}

impl Node {
    /// The text this node covers, with scopes flattened away.
    ///
    /// Round-trips to the input whenever highlighting succeeded, which is what
    /// makes it worth asserting in tests: a grammar that loses or duplicates a
    /// character is a bug the eye would eventually find, but only eventually.
    pub fn text(&self) -> String {
        let mut out = String::new();
        self.write_text(&mut out);
        out
    }

    fn write_text(&self, into: &mut String) {
        for child in &self.children {
            match child {
                Element::Text(text) => into.push_str(text),
                Element::Node(node) => node.write_text(into),
            }
        }
    }
}

/// Collects the tree as the engine walks.
#[derive(Default)]
struct Emitter {
    /// The open nodes, outermost first. The last is what text goes into.
    stack: Vec<Node>,
}

impl Emitter {
    fn new() -> Self {
        Self {
            stack: vec![Node::default()],
        }
    }

    fn add_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.stack
            .last_mut()
            .expect("the root is always open")
            .children
            .push(Element::Text(text.to_string()));
    }

    fn open_node(&mut self, scope: String) {
        self.stack.push(Node {
            scope: Some(scope),
            ..Node::default()
        });
    }

    fn close_node(&mut self) {
        if self.stack.len() > 1 {
            let node = self.stack.pop().expect("checked");
            self.stack
                .last_mut()
                .expect("the root remains")
                .children
                .push(Element::Node(node));
        }
    }

    /// Closes everything still open and hands back the tree.
    ///
    /// The original clears its stack, which works because it attaches a node
    /// when it opens it. This attaches on close, so closing is the equivalent.
    fn finish(mut self) -> Node {
        while self.stack.len() > 1 {
            self.close_node();
        }
        self.stack.pop().unwrap_or_default()
    }

    fn add_sublanguage(&mut self, mut root: Node, language: &str) {
        root.language = Some(format!("language:{language}"));
        self.stack
            .last_mut()
            .expect("the root is always open")
            .children
            .push(Element::Node(root));
    }
}

/// A mode's rules, matched together, with the ability to skip a rule.
///
/// highlight.js needs this because a rule can match and then be *rejected* by a
/// callback — `obj.keyword` should not match a `beginKeywords` rule — and when
/// that happens the text at that position still has to be considered for the
/// rules that came later. One combined pattern cannot say "everything after
/// rule 3", so a matcher is built per starting rule, on demand.
///
/// The alternative — testing rules one at a time — would be correct and much
/// slower: scanning a long run of text in which nothing matches is the common
/// case, and doing it once per rule instead of once is the difference between
/// milliseconds and seconds.
struct Resumable {
    patterns: Vec<String>,
    matchers: HashMap<usize, MultiRegex>,
    dialect: Dialect,
    /// Which rule to start from. Non-zero only while a rejection is being
    /// resolved.
    index: usize,
    /// How many of the rules are `begin` rules. Reaching that count is how the
    /// original knows it has run out of rules to resume into.
    count: usize,
}

impl Resumable {
    fn new(patterns: Vec<String>, rules: &[Rule], dialect: Dialect) -> Option<Self> {
        let count = rules
            .iter()
            .filter(|rule| matches!(rule, Rule::Begin(_)))
            .count();
        let matchers = HashMap::new();
        Some(Self {
            patterns,
            matchers,
            dialect,
            index: 0,
            count,
        })
    }

    fn consider_all(&mut self) {
        self.index = 0;
    }

    fn matcher(&mut self, from: usize) -> Option<&MultiRegex> {
        if !self.matchers.contains_key(&from) {
            let suffix: Vec<String> = self.patterns.iter().skip(from).cloned().collect();
            let matcher = MultiRegex::new(&suffix, self.dialect)?;
            self.matchers.insert(from, matcher);
        }
        self.matchers.get(&from)
    }

    /// The nearest match, with a rejected rule stepped over.
    fn exec(&mut self, text: &str, from: usize) -> Option<(RuleMatch, usize)> {
        if self.patterns.is_empty() {
            return None;
        }
        let index = self.index;
        let mut found = self.matcher(index)?.find_from(text, from);

        // A rule was rejected at this exact position, so the rules that came
        // after it still have to be tried *here* — not by scanning forward,
        // which would miss a later rule matching sooner than an earlier one.
        if index != 0 {
            let at_position = found.as_ref().filter(|m| m.start == from);
            if at_position.is_none() {
                found = self.matcher(0)?.find_from(text, from + 1);
            }
        }

        let found = found?;
        let absolute = index + found.rule;
        // Resume after whichever rule matched, so an ignored one is not
        // considered again at this position.
        self.index = absolute + 1;
        if self.index >= self.count {
            self.consider_all();
        }
        Some((found, absolute))
    }

    /// Whether a rejected rule leaves anything to try at the same position.
    fn has_rules_left(&self) -> bool {
        self.index != 0 && self.index < self.patterns.len()
    }
}

/// State carried through one document.
///
/// Not `Debug`, which this crate asks of its public types: the twelve types it
/// holds — the emitter, the resumable matchers, the tree — would each have to
/// derive it, and what a reader would get is the whole engine's internals. The
/// crate's own warning is left standing rather than answered that way.
pub struct Engine<'a> {
    grammar: &'a Grammar,
    sub_grammars: &'a HashMap<String, Grammar>,
    code: &'a str,
    emitter: Emitter,
    keyword_hits: HashMap<String, usize>,
    /// The chain of modes we are inside, outermost first. The last is the mode
    /// being matched against.
    frames: Vec<ModeId>,
    /// The matcher for each mode currently on the stack.
    matchers: Vec<Resumable>,
    mode_buffer: String,
    relevance: f64,
    index: usize,
    resume_scan_at_same_position: bool,
    /// What a callback left behind on a mode, so a later one can read it —
    /// `endSameAsBegin` is a begin that writes and an end that checks.
    callback_data: HashMap<ModeId, HashMap<String, String>>,
    /// Whether the last match began a mode, and where it was. A zero-width end
    /// match right after a begin at the same place is a rule that cannot make
    /// progress, and has to be caught against that.
    last_match: Option<(bool, usize)>,
    /// The current match's groups, so an end callback can inspect them.
    end_groups: Vec<Option<String>>,
    /// How far to highlight, when this run is one piece of a document rather
    /// than the whole of it. `None` means to the end, which is what a
    /// whole-document highlight wants and what the Dart side is compared
    /// against, so nothing about that path changes.
    limit: Option<usize>,
}

/// Why highlighting stopped early.
enum Stop {
    Illegal,
}

/// Where the engine had got to at a line boundary.
///
/// Recorded at the start of a line and handed back to [`Engine::resume`], this
/// is what makes re-highlighting a document after an edit cost the lines that
/// changed rather than all of them.
///
/// The buffer is not in here, and that is the point: the engine empties it at
/// every boundary a state is taken at, so the text it holds — which for an
/// unterminated comment can be the rest of the document — never has to be
/// carried. What is left is the mode stack, where each mode has got to among its
/// own rules, and what the callbacks remember.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    frames: Vec<ModeId>,
    /// The rule each frame's matcher starts from. Non-zero only while a
    /// rejected rule is being stepped over, which lasts until the text it was
    /// rejected at has been passed.
    resume: Vec<usize>,
    callback_data: HashMap<ModeId, HashMap<String, String>>,
}

impl<'a> Engine<'a> {
    pub fn new(
        grammar: &'a Grammar,
        sub_grammars: &'a HashMap<String, Grammar>,
        code: &'a str,
    ) -> Self {
        Self {
            grammar,
            sub_grammars,
            code,
            emitter: Emitter::new(),
            keyword_hits: HashMap::new(),
            frames: Vec::new(),
            matchers: Vec::new(),
            mode_buffer: String::new(),
            relevance: 0.0,
            index: 0,
            resume_scan_at_same_position: false,
            callback_data: HashMap::new(),
            last_match: None,
            end_groups: Vec::new(),
            limit: None,
        }
    }

    /// The state at the current position, for [`Engine::resume`] to carry on
    /// from.
    ///
    /// Only meaningful where the buffer is empty — at the end of
    /// [`Engine::scan_until`], which empties it. Anywhere else the text scanned
    /// since the last rule ended is part of what the state means, and the
    /// buffer holding it would have to be carried with it.
    pub fn state(&self) -> State {
        State {
            frames: self.frames.clone(),
            resume: self.matchers.iter().map(|matcher| matcher.index).collect(),
            callback_data: self.callback_data.clone(),
        }
    }

    /// Highlights `code` starting from `state`.
    ///
    /// The text is what follows the position the state was taken at, and the
    /// answer is only about that text: the lines behind it were highlighted by
    /// whichever run took the state, and re-opening the scopes it is inside is
    /// all that is needed for its output to sit inside the same nodes.
    pub fn resume(
        grammar: &'a Grammar,
        sub_grammars: &'a HashMap<String, Grammar>,
        code: &'a str,
        state: &State,
    ) -> Self {
        let mut engine = Engine::new(grammar, sub_grammars, code);
        engine.frames = state.frames.clone();
        engine.matchers = engine
            .frames
            .iter()
            .zip(state.resume.iter().chain(std::iter::repeat(&0)))
            .map(|(mode, index)| {
                let mut matcher = engine.resumable(*mode);
                matcher.index = *index;
                matcher
            })
            .collect();
        engine.callback_data = state.callback_data.clone();
        engine.process_continuations();
        engine
    }

    /// The matcher a mode starts with, before any text has been considered.
    fn resumable(&self, mode: ModeId) -> Resumable {
        let definition = self.mode(mode);
        Resumable::new(
            definition_matcher_patterns(definition),
            &definition.rules,
            self.grammar.dialect,
        )
        .unwrap_or_else(|| Resumable {
            patterns: Vec::new(),
            matchers: HashMap::new(),
            dialect: self.grammar.dialect,
            index: 0,
            count: 0,
        })
    }

    /// Highlights up to `position`, in bytes, and stops there.
    ///
    /// A rule that matches before `position` is followed even when its text runs
    /// past it: a match is a whole thing and cannot be cut in two, so the next
    /// piece starts after it rather than at `position`. `position` is therefore
    /// where the piece would like to end, and [`Engine::state`] says where it
    /// did.
    ///
    /// The buffer is emptied on the way out, which is what makes the state one
    /// that can be carried and what puts the text scanned so far behind us.
    pub fn scan_until(&mut self, position: usize) {
        self.limit = Some(position.min(self.code.len()));
        let _ = self.scan();
        self.limit = None;
    }

    /// Where the scan has got to, in bytes into the text this engine was given.
    pub fn position(&self) -> usize {
        self.index
    }

    /// Whether a mode that hands its text to another grammar is open.
    ///
    /// A caller taking a state has to wait for this to be false. Such a mode's
    /// text is buffered until the mode ends and only then given to the grammar
    /// it names, and that grammar's own state carries across the lines it spans
    /// — so emptying the buffer at a line boundary inside it would hand the
    /// embedded language a piece of the text it is meant to see whole.
    pub fn embeds_a_language(&self) -> bool {
        self.frames
            .iter()
            .any(|frame| self.mode(*frame).sub_language.is_some())
    }

    /// The tree built so far, and a fresh one to carry on with.
    ///
    /// Each piece of a piecewise highlight answers with a tree of its own, so a
    /// caller can read the spans for the lines it covered without knowing what
    /// came before. The scopes the engine is inside are re-opened in the new
    /// tree, which is what keeps those spans inside the same nodes.
    pub fn take_tree(&mut self) -> Node {
        let finished = std::mem::replace(&mut self.emitter, Emitter::new()).finish();
        self.process_continuations();
        finished
    }

    fn mode(&self, id: ModeId) -> &Mode {
        &self.grammar.modes[id]
    }

    fn top(&self) -> ModeId {
        *self.frames.last().unwrap_or(&self.grammar.root)
    }

    /// An engine about to highlight the whole of `code`.
    ///
    /// The root is pushed and its scopes opened here rather than in
    /// [`Engine::run`], because a caller that highlights in pieces drives
    /// [`Engine::scan_until`] itself.
    pub fn start(
        grammar: &'a Grammar,
        sub_grammars: &'a HashMap<String, Grammar>,
        code: &'a str,
    ) -> Self {
        let mut engine = Engine::new(grammar, sub_grammars, code);
        engine.begin();
        engine
    }

    /// Puts the root mode on the stack and opens the scopes it is inside — which
    /// at the top of a document is none of them.
    fn begin(&mut self) {
        self.push_mode(self.grammar.root);
        self.process_continuations();
    }

    /// Highlights the whole of `code`.
    pub fn run(self) -> Highlighted {
        let mut engine = self;
        engine.begin();
        let stopped = engine.scan();
        let root = engine.emitter.finish();
        Highlighted {
            root,
            relevance: if stopped { 0.0 } else { engine.relevance },
            illegal: stopped,
        }
    }

    /// Highlights the whole of `code` with a grammar given up front.
    pub fn highlight(
        grammar: &'a Grammar,
        sub_grammars: &'a HashMap<String, Grammar>,
        code: &'a str,
    ) -> Highlighted {
        Engine::new(grammar, sub_grammars, code).run()
    }

    fn scan(&mut self) -> bool {
        let mut iterations = 0usize;
        loop {
            iterations += 1;
            if self.resume_scan_at_same_position {
                self.resume_scan_at_same_position = false;
            } else if let Some(matcher) = self.matchers.last_mut() {
                matcher.consider_all();
            }

            let Some((found, absolute)) = self
                .matchers
                .last_mut()
                .and_then(|matcher| matcher.exec(self.code, self.index))
            else {
                break;
            };

            // A piece of a document ends here, rather than at the end of the
            // text. The check is on where the match *starts*: one that begins
            // before the limit is followed wherever it goes, because cutting it
            // would leave the next piece to re-scan text this one has already
            // decided about.
            if self.limit.map_or(false, |limit| found.start >= limit) {
                break;
            }

            // A rule that consumes nothing at a place where nothing else can
            // move the scan on is a grammar that cannot be highlighted, and
            // looping until the caller gives up is the worst way to find out —
            // nothing can interrupt it, because the work is in one call.
            // highlight.js gives up here too, on the same numbers: enough
            // iterations to be sure it is not merely slow, and an index far
            // enough behind them to be sure it is not advancing.
            if iterations > 100_000 && iterations > found.start * 3 {
                return true;
            }

            let before = self.code[self.index..found.start].to_string();
            match self.process_lexeme(&before, Some((&found, absolute))) {
                Ok(processed) => self.index = found.start + processed,
                Err(_) => return true,
            }
        }
        // Whatever is left of this piece, unscanned. With no limit that is the
        // rest of the document; with one it is the text between the last match
        // and where the piece was asked to end.
        let stop = self.limit.unwrap_or(self.code.len()).min(self.code.len());
        if self.index < stop {
            let rest = self.code[self.index..stop].to_string();
            let _ = self.process_lexeme(&rest, None);
        }
        self.index = self.index.max(stop);
        // Emptied, so the state taken now is one that can be carried: the text
        // behind it has been emitted, inside whichever node it belonged to.
        if !self.mode_buffer.is_empty() {
            self.process_buffer();
        }
        false
    }

    /// Reacts to one match, and says how far the scan moves on.
    fn process_lexeme(
        &mut self,
        before_match: &str,
        matched: Option<(&RuleMatch, usize)>,
    ) -> Result<usize, Stop> {
        self.mode_buffer.push_str(before_match);

        let Some((found, absolute)) = matched else {
            self.process_buffer();
            return Ok(0);
        };
        let lexeme = found.text.clone();
        let rule = *self
            .mode(self.top())
            .rules
            .get(absolute)
            .unwrap_or(&Rule::Illegal);

        // A rule that matches nothing, immediately after a rule that did the
        // same at the same place, means the grammar has a rule that cannot make
        // progress. highlight.js nudges the scan past a character rather than
        // spinning on it; the case is real, and comes from rules with optional
        // parts that can match the empty string.
        let is_begin = matches!(rule, Rule::Begin(_));
        let is_end = matches!(rule, Rule::End);
        if let Some((last_begin, last_index)) = self.last_match {
            if last_begin && is_end && last_index == found.start && lexeme.is_empty() {
                let step = self.char_len_at(found.start);
                self.mode_buffer
                    .push_str(&self.code[found.start..found.start + step]);
                return Ok(step);
            }
        }
        self.last_match = Some((is_begin, found.start));

        match rule {
            Rule::Begin(child) => self.do_begin_match(child, found, absolute),
            Rule::Illegal => {
                if lexeme.is_empty() {
                    // An `illegal` rule that matches nothing would not
                    // terminate, so step past a character instead.
                    return Ok(self.char_len_at(found.start));
                }
                Err(Stop::Illegal)
            }
            Rule::End => {
                if let Some(processed) = self.do_end_match(found) {
                    Ok(processed)
                } else {
                    // An end that did not stick — a callback rejected it, or it
                    // belonged to a parent. The text is kept and the scan moves
                    // past it.
                    self.mode_buffer.push_str(&lexeme);
                    Ok(lexeme.len())
                }
            }
        }
    }

    fn do_begin_match(
        &mut self,
        child: ModeId,
        found: &RuleMatch,
        _absolute: usize,
    ) -> Result<usize, Stop> {
        let lexeme = found.text.clone();

        // A callback can reject the match, and then the text is not this mode's
        // after all.
        if self.begin_callbacks(child, found) {
            return Ok(self.do_ignore(&lexeme));
        }

        let mode = self.mode(child);
        let skip = mode.skip;
        let exclude_begin = mode.exclude_begin;
        let return_begin = mode.return_begin;

        if skip {
            self.mode_buffer.push_str(&lexeme);
        } else {
            if exclude_begin {
                self.mode_buffer.push_str(&lexeme);
            }
            self.process_buffer();
            if !return_begin && !exclude_begin {
                self.mode_buffer = lexeme.clone();
            }
        }
        let scope = self.mode(child).scope.clone();
        let begin_scope = self.mode(child).begin_scope.clone();
        self.start_new_mode(child, found, scope, begin_scope);

        Ok(if return_begin { 0 } else { lexeme.len() })
    }

    /// Leaves the current mode, if the match really ends it.
    ///
    /// `None` means it did not: the pattern belonged to an enclosing mode, or a
    /// callback refused it.
    fn do_end_match(&mut self, found: &RuleMatch) -> Option<usize> {
        let lexeme = found.text.clone();
        self.end_groups = found.groups.clone();
        let end_frame = self.end_of_mode(self.frames.len() - 1, found.start)?;
        if end_frame == 0 {
            // The root has no parent to return to, so nothing can end it.
            return None;
        }

        let origin = self.top();
        let end_mode = self.frames[end_frame];
        let end_scope = self.mode(end_mode).end_scope.clone();
        let skip = self.mode(origin).skip;
        let return_end = self.mode(origin).return_end;
        let exclude_end = self.mode(origin).exclude_end;

        if let Some(scope) = &end_scope {
            if let Some(wrap) = scope.wrap.as_ref().filter(|wrap| !wrap.is_empty()) {
                self.process_buffer();
                let aliased = self.aliased(wrap);
                self.emit_keyword(&lexeme, &aliased);
            } else if scope.multi {
                self.process_buffer();
                self.emit_multi_class(scope, found);
            } else {
                self.keep_end_text(&lexeme, skip, return_end, exclude_end);
            }
        } else {
            self.keep_end_text(&lexeme, skip, return_end, exclude_end);
        }

        // Unwind to the parent of the mode that ended, closing a scope and
        // collecting relevance for each mode left behind.
        let mut leaving = self.frames.len();
        while leaving > end_frame {
            leaving -= 1;
            let mode = self.mode(self.frames[leaving]);
            let (scoped, counts, relevance) = (
                mode.scope.is_some(),
                !mode.skip && mode.sub_language.is_none(),
                mode.relevance,
            );
            if scoped {
                self.emitter.close_node();
            }
            if counts {
                self.relevance += relevance;
            }
        }
        self.frames.truncate(end_frame);
        self.matchers.truncate(end_frame);

        // A mode whose end opens another one — how a heredoc's closing marker
        // starts the next rule.
        if let Some(starts) = self.mode(end_mode).starts {
            let scope = self.mode(starts).scope.clone();
            let begin_scope = self.mode(starts).begin_scope.clone();
            self.start_new_mode(starts, found, scope, begin_scope);
        }

        Some(if return_end { 0 } else { lexeme.len() })
    }

    /// Puts the ending text into the buffer, honouring the mode's flags.
    fn keep_end_text(&mut self, lexeme: &str, skip: bool, return_end: bool, exclude_end: bool) {
        if skip {
            self.mode_buffer.push_str(lexeme);
            return;
        }
        if !(return_end || exclude_end) {
            self.mode_buffer.push_str(lexeme);
        }
        self.process_buffer();
        if exclude_end {
            self.mode_buffer = lexeme.to_string();
        }
    }

    /// The frame whose end pattern matches at `from`, looking outward through
    /// modes that inherit their parent's terminator.
    ///
    /// `from` is where the matcher's terminator rule matched, which is the only
    /// place a mode can end: a mode whose own end pattern does not match there
    /// was ended by an ancestor's terminator instead.
    fn end_of_mode(&mut self, frame: usize, from: usize) -> Option<usize> {
        let mode = self.mode(self.frames[frame]);
        let remainder = &self.code[from..];
        let matches_end = mode.end_regex.as_ref().map_or(false, |regex| {
            regex
                .find(remainder)
                .map_or(false, |found| found.start() == 0)
        });
        if matches_end {
            if let Some(callback) = mode.on_end {
                let frame_mode = self.frames[frame];
                let found = RuleMatch {
                    text: remainder[..remainder.len().min(1)].to_string(),
                    groups: self.end_groups.clone(),
                    start: 0,
                    end: 0,
                    rule: 0,
                };
                if self.run_callback(callback, frame_mode, &found) {
                    return None;
                }
            }
            // `endsParent` says the enclosing mode ends too.
            let mut frame = frame;
            while frame > 0 {
                let mode = self.mode(self.frames[frame]);
                if !mode.ends_parent {
                    break;
                }
                frame -= 1;
            }
            return Some(frame);
        }

        if mode.ends_with_parent && frame > 0 {
            return self.end_of_mode(frame - 1, from);
        }
        None
    }

    /// Handles a rule the grammar wants skipped.
    fn do_ignore(&mut self, lexeme: &str) -> usize {
        if self
            .matchers
            .last()
            .map_or(true, |matcher| !matcher.has_rules_left())
        {
            // Nothing left to try here, so step past a character.
            let step = self.char_len_at(self.index);
            if step > 0 {
                if let Some(ch) = lexeme.chars().next() {
                    self.mode_buffer.push(ch);
                    return ch.len_utf8();
                }
            }
            return 0;
        }
        // Try the remaining rules at the same position.
        self.resume_scan_at_same_position = true;
        0
    }

    fn start_new_mode(
        &mut self,
        mode: ModeId,
        found: &RuleMatch,
        scope: Option<String>,
        begin_scope: Option<CompiledScope>,
    ) {
        if let Some(scope) = scope.as_ref().filter(|scope| !scope.is_empty()) {
            let aliased = self.aliased(scope);
            self.emitter.open_node(aliased);
        }
        if let Some(scope) = &begin_scope {
            if let Some(wrap) = scope.wrap.as_ref().filter(|wrap| !wrap.is_empty()) {
                let aliased = self.aliased(wrap);
                let buffer = std::mem::take(&mut self.mode_buffer);
                self.emit_keyword(&buffer, &aliased);
            } else if scope.multi {
                self.emit_multi_class(scope, found);
                self.mode_buffer.clear();
            }
        }
        self.push_mode(mode);
    }

    /// Puts a mode on the stack, with a matcher of its own.
    fn push_mode(&mut self, mode: ModeId) {
        self.frames.push(mode);
        let definition = self.mode(mode);
        let matcher = definition.matcher.as_ref().map(|_| {
            Resumable::new(
                definition_matcher_patterns(definition),
                &definition.rules,
                self.grammar.dialect,
            )
        });
        self.matchers
            .push(matcher.flatten().unwrap_or_else(|| Resumable {
                patterns: Vec::new(),
                matchers: HashMap::new(),
                dialect: self.grammar.dialect,
                index: 0,
                count: 0,
            }));
    }

    /// Colours the match groups a multi-part rule asked to colour.
    ///
    /// Every part is emitted, not only the ones with a scope: the parts a rule
    /// colours are wrapped in it, and the parts between them are the plain text
    /// of the match — the space in `def hello`, whose rule colours `def` and
    /// `hello` and says nothing about the space. Leaving those out loses text,
    /// which is the one thing highlighting may not do.
    fn emit_multi_class(&mut self, scope: &CompiledScope, found: &RuleMatch) {
        for index in 1..=found.groups.len() {
            if scope.emit.get(&index).copied() != Some(true) {
                continue;
            }
            let text = found
                .groups
                .get(index)
                .cloned()
                .flatten()
                .unwrap_or_default();
            match scope.positions.get(&index) {
                Some(name) => {
                    let aliased = self.aliased(name);
                    self.emit_keyword(&text, &aliased);
                }
                // A part with no scope of its own is still text the match
                // consumed, and it belongs to the mode that is being entered.
                // The buffer is replaced rather than added to: it holds the
                // whole lexeme, and the parts *are* that lexeme, split up.
                None => {
                    self.mode_buffer = text;
                    self.process_keywords();
                    self.mode_buffer.clear();
                }
            }
        }
    }

    fn emit_keyword(&mut self, text: &str, scope: &str) {
        if text.is_empty() {
            return;
        }
        self.emitter.open_node(scope.to_string());
        self.emitter.add_text(text);
        self.emitter.close_node();
    }

    /// A grammar may rename the scopes it uses, so a theme written for another
    /// grammar still applies.
    fn aliased(&self, scope: &str) -> String {
        self.grammar
            .class_name_aliases
            .get(scope)
            .cloned()
            .unwrap_or_else(|| scope.to_string())
    }

    fn process_buffer(&mut self) {
        if self.mode(self.top()).sub_language.is_some() {
            self.process_sub_language();
        } else {
            self.process_keywords();
        }
        self.mode_buffer.clear();
    }

    /// Colours the buffered text by looking its words up in the keyword table.
    fn process_keywords(&mut self) {
        let mode = self.mode(self.top());
        let Some(pattern) = mode.keyword_pattern.clone() else {
            let buffer = std::mem::take(&mut self.mode_buffer);
            self.emitter.add_text(&buffer);
            return;
        };
        let keywords_empty = mode.keywords.is_empty();
        if keywords_empty {
            let buffer = std::mem::take(&mut self.mode_buffer);
            self.emitter.add_text(&buffer);
            return;
        }

        let buffer = std::mem::take(&mut self.mode_buffer);
        let case_insensitive = self.grammar.dialect.case_insensitive;

        // Text is emitted as it is passed: everything up to a keyword goes out
        // plain, the keyword goes out scoped, and the walk continues. The
        // order matters — this is what puts the colours in the right places.
        let mut pending = String::new();
        let mut last = 0usize;
        let mut search_from = 0usize;

        while search_from <= buffer.len() {
            let Some(found) = pattern.find_from(&buffer, search_from).next() else {
                break;
            };
            let word = buffer[found.range()].to_string();
            let (start, end) = (found.start(), found.end());
            // A pattern that can match nothing would loop; step a character.
            search_from = if end == start {
                start + self.char_len_in(&buffer, start)
            } else {
                end
            };

            pending.push_str(&buffer[last..start]);
            let lookup = if case_insensitive {
                word.to_lowercase()
            } else {
                word.clone()
            };
            let data = self.mode(self.top()).keywords.get(&lookup).cloned();
            match data {
                Some(data) => {
                    let text = std::mem::take(&mut pending);
                    self.emitter.add_text(&text);

                    let entry = self.keyword_hits.entry(lookup).or_insert(0);
                    *entry += 1;
                    if *entry <= MAX_KEYWORD_HITS {
                        self.relevance += data.relevance;
                    }

                    if data.scope.starts_with('_') {
                        // A leading underscore means the word counts towards
                        // relevance but is not coloured.
                        pending.push_str(&word);
                    } else {
                        let aliased = self.aliased(&data.scope);
                        self.emit_keyword(&word, &aliased);
                    }
                }
                None => pending.push_str(&word),
            }
            last = end;
        }
        pending.push_str(&buffer[last..]);
        self.emitter.add_text(&pending);
    }

    /// Highlights a run in another language and drops the result in.
    fn process_sub_language(&mut self) {
        if self.mode_buffer.is_empty() {
            return;
        }
        let sub_language = self.mode(self.top()).sub_language.clone();
        let buffer = std::mem::take(&mut self.mode_buffer);
        let names: Vec<String> = match sub_language {
            Some(SubLanguage::Named(name)) => vec![name],
            Some(SubLanguage::Candidate(names)) => names,
            None => {
                self.emitter.add_text(&buffer);
                return;
            }
        };

        // With one candidate there is nothing to detect. With several, the one
        // that finds the most evidence wins — what `highlightAuto` does.
        let mut best: Option<(f64, Node, String)> = None;
        for name in &names {
            let Some(grammar) = self.sub_grammars.get(name) else {
                continue;
            };
            let result = Engine::highlight(grammar, self.sub_grammars, &buffer);
            let better = best
                .as_ref()
                .map_or(true, |(relevance, _, _)| result.relevance > *relevance);
            if better {
                best = Some((result.relevance, result.root, name.clone()));
            }
        }

        match best {
            Some((_, root, name)) => self.emitter.add_sublanguage(root, &name),
            // A sub-language we were not given: shown plain rather than
            // dropped, since the text is still the document's.
            None => self.emitter.add_text(&buffer),
        }
    }

    /// Re-opens the scopes we are inside.
    ///
    /// A document resumed mid-way starts coloured rather than plain until the
    /// next mode boundary, which is how a file scrolled into view looks right
    /// from the first line.
    fn process_continuations(&mut self) {
        let scopes: Vec<String> = self
            .frames
            .iter()
            .skip(1)
            .filter_map(|frame| self.mode(*frame).scope.clone())
            .filter(|scope| !scope.is_empty())
            .collect();
        for scope in scopes {
            self.emitter.open_node(scope);
        }
    }

    /// Runs the callbacks that fire as a mode begins. Returns whether the
    /// match was rejected.
    fn begin_callbacks(&mut self, mode: ModeId, found: &RuleMatch) -> bool {
        let before = self.mode(mode).before_begin;
        let on_begin = self.mode(mode).on_begin;
        for callback in [before, on_begin].into_iter().flatten() {
            if self.run_callback(callback, mode, found) {
                return true;
            }
        }
        false
    }

    /// Runs one callback. Returns whether it asked for the match to be ignored.
    fn run_callback(&mut self, callback: Callback, mode: ModeId, found: &RuleMatch) -> bool {
        match callback {
            Callback::SkipIfPrecededByDot => {
                // `beginKeywords` matches a bare word; `obj.keyword` is a
                // property access and not a keyword.
                found.start > 0 && self.code.as_bytes()[found.start - 1] == b'.'
            }
            Callback::Shebang => found.start != 0,
            Callback::JsxOrGeneric => {
                let after = self.code.get(found.end..).unwrap_or("");
                looks_like_a_type(&found.text, after)
            }
            Callback::SameAsBegin => {
                let begin = found.groups.get(1).cloned().flatten().unwrap_or_default();
                self.callback_data
                    .entry(mode)
                    .or_default()
                    .insert("_beginMatch".to_string(), begin);
                false
            }
            Callback::SameAsBeginSecond => {
                let group = |index: usize| found.groups.get(index).cloned().flatten();
                let begin = group(1).or_else(|| group(2)).unwrap_or_default();
                self.callback_data
                    .entry(mode)
                    .or_default()
                    .insert("_beginMatch".to_string(), begin);
                false
            }
            Callback::SameAsEnd => {
                let begin = self
                    .callback_data
                    .get(&mode)
                    .and_then(|data| data.get("_beginMatch"))
                    .cloned();
                let this = found.groups.get(1).cloned().flatten();
                begin.is_some() && begin != this
            }
            Callback::MathematicaSymbol => !is_mathematica_symbol(&found.text),
        }
    }

    fn char_len_at(&self, at: usize) -> usize {
        self.char_len_in(self.code, at)
    }

    fn char_len_in(&self, text: &str, at: usize) -> usize {
        text.get(at..)
            .and_then(|rest| rest.chars().next())
            .map(char::len_utf8)
            .unwrap_or(1)
    }
}

/// Mathematica's system symbols, one per line.
///
/// Generated from the list in `re_highlight`'s `mathematica.dart` — the grammar
/// this is a port of — and kept here rather than asked for at run time, because
/// the Dart side holds its copy in a private constant and the engine has no way
/// to reach it. A grammar's data belongs with the grammar; this is the one case
/// where the grammar's data lives in a file of its own rather than in the JSON.
static MATHEMATICA_SYMBOLS: &str = include_str!("mathematica_symbols.txt");

/// The list above, split into lines and sorted, built the first time it is
/// needed.
static MATHEMATICA_SYMBOLS_SORTED: std::sync::OnceLock<Vec<&'static str>> =
    std::sync::OnceLock::new();

/// Whether `text` is one of Mathematica's system symbols.
///
/// The grammar's `onBegin` rejects everything else, which is what keeps a
/// variable named `List` from being coloured as the built-in one.
fn is_mathematica_symbol(text: &str) -> bool {
    let symbols = MATHEMATICA_SYMBOLS_SORTED.get_or_init(|| {
        let mut symbols: Vec<&'static str> = MATHEMATICA_SYMBOLS
            .lines()
            .filter(|line| !line.is_empty())
            .collect();
        symbols.sort_unstable();
        symbols
    });
    symbols.binary_search(&text).is_ok()
}

/// Whether `<something` begins a type parameter list rather than a tag.
///
/// Four questions, in the order highlight.js asks them, all of them about
/// what follows the `<`:
///
/// * another `<` or a `,` — `<Array<Array<T>>` and `<T, A>` are types, and
///   neither can appear in a tag;
/// * a `>` — then only a matching closing tag makes it an element, which is
///   a search through the rest of the document;
/// * `=` after whitespace — `<T = any>` is a default, and a tag attribute
///   would need a name before the `=`;
/// * `extends` after whitespace — `<T extends string>` is a constraint.
///
/// The reference implementation is these same four, written against the
/// whole document it is highlighting; this one is given the same text, so
/// the answer is the same rather than an approximation of it.
fn looks_like_a_type(text: &str, after: &str) -> bool {
    match after.chars().next() {
        Some('<') | Some(',') => return true,
        Some('>') if !has_closing_tag(text, after) => return true,
        _ => {}
    }
    let rest = after.trim_start();
    if rest.starts_with('=') {
        return true;
    }
    // `extends` needs whitespace before *and* after it, which is what tells
    // `<T extends X>` from a tag called `extends`.
    rest.starts_with("extends")
        && rest.len() > "extends".len()
        && rest["extends".len()..].starts_with(char::is_whitespace)
}

/// Whether an opening tag has a closing tag after it.
///
/// `<div` closes with `</div`, so the `<` comes off and a `/` goes on.
fn has_closing_tag(text: &str, after: &str) -> bool {
    let Some(name) = text.strip_prefix('<') else {
        return true;
    };
    after.contains(&format!("</{name}"))
}

/// The patterns a compiled mode's matcher was built from, by rule order.
///
/// Rebuilt here rather than kept alongside, because the matcher already holds
/// them and a second copy could drift. The order is the one the compiler built
/// them in: children, then the terminator, then the illegal rule.
fn definition_matcher_patterns(mode: &Mode) -> Vec<String> {
    mode.rule_patterns.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_node_reports_the_text_it_covers() {
        let node = Node {
            scope: None,
            language: None,
            children: vec![
                Element::Text("a".to_string()),
                Element::Node(Node {
                    scope: Some("keyword".to_string()),
                    language: None,
                    children: vec![Element::Text("b".to_string())],
                }),
            ],
        };
        assert_eq!(node.text(), "ab");
    }

    #[test]
    fn closing_one_too_many_nodes_is_harmless() {
        // The engine can close a node that was never opened when a grammar ends
        // a mode it is not in; the root must survive it.
        let mut emitter = Emitter::new();
        emitter.close_node();
        emitter.close_node();
        let node = emitter.finish();
        assert!(node.children.is_empty());
    }

    #[test]
    fn a_finished_emitter_attaches_what_is_still_open() {
        let mut emitter = Emitter::new();
        emitter.add_text("a");
        emitter.open_node("string".to_string());
        emitter.add_text("b");
        let node = emitter.finish();
        // "b" is still inside the scope, not dropped for being unclosed — the
        // original attaches nodes as it opens them, and this closes them
        // instead, so the result has to be the same.
        assert_eq!(node.text(), "ab");
        assert_eq!(node.children.len(), 2);
    }
}
