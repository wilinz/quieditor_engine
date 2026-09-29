//! Turning a grammar into something the engine can run.
//!
//! This is a port of highlight.js's `compileLanguage`, and it is the least
//! self-evident part of the port, because most of what it does is *rewrite* the
//! grammar a writer wrote into the shape the engine wants:
//!
//! * `className` becomes `scope` — highlight.js renamed the field and kept the
//!   old one working.
//! * `match` becomes `begin` with no `end`.
//! * `beginKeywords: "a b c"` becomes a pattern over those words, with a
//!   callback that stops it matching `obj.keyword`.
//! * `beforeMatch` becomes a begin that looks ahead at the real begin, wrapped
//!   in a `starts` mode so the lookahead is not consumed.
//! * a multi-part `begin` with a `beginScope` map has its groups remapped, so
//!   the scopes line up with what the engine actually captures.
//! * `endsWithParent` gives a mode its parent's terminator, appended to its own.
//!
//! These are not incidental: each one is a case where the grammar's author
//! writes what they mean and the engine needs something else.
//!
//! # Sharing
//!
//! highlight.js compiles a shared sub-mode once and reuses the object, so a
//! grammar's compiled form is a graph rather than a tree. That matters here,
//! because expanding the graph into a tree can grow without bound — a grammar
//! like Arduino's reuses sub-modes heavily.
//!
//! A compiled mode is reusable only if nothing about it depends on where it
//! sits. `endsWithParent` does: it inherits its parent's terminator. So those
//! are compiled fresh per placement and everything else is memoised, which is
//! the same split highlight.js draws with `dependencyOnParent`.
//!
//! highlight.js can key that memo on the spec object, which stays alive as long
//! as the grammar does. Here a spec is a value, and the compiler makes throwaway
//! ones for `variants` and for the `beforeMatch` rewrite, so identity has to be
//! earned: see [`SpecKey`].

use crate::highlight::keywords::{self, KeywordData};
use crate::highlight::mode::{
    Callback, IllegalSpec, LanguageSpec, ModeSpec, Patterns, RawKeywords, ScopeSpec, SubLanguage,
};
use crate::highlight::regex::{rewrite_backreferences, Dialect, MultiRegex};
use regress::{Flags, Regex};
use std::collections::HashMap;

/// A mode, by its index in the compiled grammar.
pub type ModeId = usize;

/// What one of a mode's rules does when it matches.
///
/// The engine needs this per rule, and the matcher only reports *which* rule
/// matched — so the two are kept in step by the compiler that built both.
/// Working it out from the rule's position would be guessing: a mode with no
/// end has no end rule, and one with no illegal rule has none of those either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// Enter the child mode with this id.
    Begin(ModeId),
    /// End the current mode.
    End,
    /// The text is not legal here.
    Illegal,
}

/// Why a grammar could not be compiled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A `ref` that points at nothing.
    UnknownRef(String),
    /// A rule that does not compile. The string is the offending pattern.
    BadPattern(String),
    /// A grammar that nests deeper than the engine will follow — which in
    /// practice means one that refers to itself.
    TooDeep,
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::UnknownRef(name) => write!(f, "no mode named {name:?}"),
            CompileError::BadPattern(pattern) => write!(f, "pattern does not compile: {pattern}"),
            CompileError::TooDeep => write!(f, "grammar nests too deeply"),
        }
    }
}

impl std::error::Error for CompileError {}

/// How deep the compiler will follow a grammar before giving up.
///
/// Real grammars nest a handful of levels: C++ needs nine, and the deepest of
/// the two hundred that ship with `re_highlight` needs eleven. Past this, either
/// the grammar refers to itself in a way the engine cannot express, or it is not
/// a grammar.
///
/// The limit is low because of what happens if it is reached too late. A stack
/// overflow is not a panic — nothing can catch it and the process dies, taking
/// the editor with it — and compiling a mode costs somewhere around ten
/// kilobytes of stack per level, so a limit in the hundreds runs the risk of
/// not being the thing that stops the recursion. Refusing is a fallback; this
/// has to arrive first.
const MAX_DEPTH: usize = 64;

/// Scopes for a multi-part pattern, remapped to the engine's groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledScope {
    /// Scope name per match group number.
    pub positions: HashMap<usize, String>,
    /// Which groups are "top level" for this rule, so a scope is not emitted
    /// twice for a group nested inside another.
    pub emit: HashMap<usize, bool>,
    pub multi: bool,
    /// A single scope wrapping the whole match, instead of one per group.
    pub wrap: Option<String>,
}

/// A compiled mode.
#[derive(Debug)]
pub struct Mode {
    pub label: Option<String>,
    pub scope: Option<String>,
    pub begin_scope: Option<CompiledScope>,
    pub end_scope: Option<CompiledScope>,

    /// The pattern this mode begins with, joined from its parts.
    pub begin: Option<String>,
    pub end: Option<String>,
    /// The end pattern, compiled: the engine tests it against the text from the
    /// match onwards to decide whether a mode really ends here.
    pub end_regex: Option<Regex>,
    pub illegal: Option<String>,
    /// The pattern that ends this mode, and its parent's if it inherits them.
    pub terminator_end: Option<String>,
    /// The combined matcher over this mode's children, its end, and its
    /// illegal rules.
    pub matcher: Option<MultiRegex>,
    /// What each of the matcher's rules does, in the same order.
    pub rules: Vec<Rule>,
    /// The patterns those rules were built from, in the same order.
    ///
    /// Kept because the engine rebuilds a matcher per rule it resumes into, and
    /// deriving the patterns again would be a second source of truth for
    /// something that must stay in step with `matcher` exactly.
    pub rule_patterns: Vec<String>,

    pub contains: Vec<ModeId>,
    pub starts: Option<ModeId>,
    pub sub_language: Option<SubLanguage>,

    pub keywords: HashMap<String, KeywordData>,
    pub keyword_pattern: Option<Regex>,

    pub relevance: f64,
    pub ends_parent: bool,
    pub ends_with_parent: bool,
    pub exclude_begin: bool,
    pub exclude_end: bool,
    pub return_begin: bool,
    pub return_end: bool,
    pub skip: bool,

    pub on_begin: Option<Callback>,
    pub on_end: Option<Callback>,
    /// Runs before `on_begin`. Only `beginKeywords` uses it.
    pub before_begin: Option<Callback>,

    /// Where this mode was entered from. Filled in as the engine descends, and
    /// what it returns to when the mode ends.
    pub parent: Option<ModeId>,
}

impl Mode {
    fn empty() -> Self {
        Self {
            label: None,
            scope: None,
            begin_scope: None,
            end_scope: None,
            begin: None,
            end: None,
            end_regex: None,
            illegal: None,
            terminator_end: None,
            matcher: None,
            rules: Vec::new(),
            rule_patterns: Vec::new(),
            contains: Vec::new(),
            starts: None,
            sub_language: None,
            keywords: HashMap::new(),
            keyword_pattern: None,
            relevance: 1.0,
            ends_parent: false,
            ends_with_parent: false,
            exclude_begin: false,
            exclude_end: false,
            return_begin: false,
            return_end: false,
            skip: false,
            on_begin: None,
            on_end: None,
            before_begin: None,
            parent: None,
        }
    }
}

/// A grammar, compiled and ready to run.
#[derive(Debug)]
pub struct Grammar {
    pub name: Option<String>,
    /// The flags its patterns were read in, which the engine needs again for
    /// the patterns it builds as it runs.
    pub dialect: Dialect,
    pub modes: Vec<Mode>,
    /// The top-level mode, which is not itself matchable — it holds the
    /// grammar's own `contains`.
    pub root: ModeId,
    pub class_name_aliases: HashMap<String, String>,
    /// Sub-grammars, for modes with a `subLanguage`: compiled on demand.
    pub sub_languages: HashMap<String, Grammar>,
}

/// Compiles a grammar.
pub fn compile(spec: &LanguageSpec) -> Result<Grammar, CompileError> {
    Compiler::new(spec).compile()
}

struct Compiler<'a> {
    language: &'a LanguageSpec,
    modes: Vec<Mode>,
    /// Compiled modes that can be reused wherever they appear, by the identity
    /// of the spec they came from.
    shared: HashMap<SpecKey, ModeId>,
}

/// What a compiled mode can be shared under, for the specs that have an
/// identity durable enough to name one.
///
/// A spec's address is an identity only while the spec is alive, and the
/// compiler makes throwaway specs: one per variant, one for each `beforeMatch`
/// rewrite. Memoising those under their own address let a later allocation land
/// on a freed one — which showed up as the second of two sub-modes silently
/// reusing the first's compiled form. So a throwaway is named after the spec it
/// came from, and a spec that is neither — one reached *through* a throwaway —
/// is not named at all, and so is never shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SpecKey {
    /// A spec that lives as long as the compilation: one from the language, or
    /// one reachable from the root without passing through a throwaway.
    Live(usize),
    /// The `index`th variant of the live spec at this address.
    Variant(usize, usize),
    /// The `beforeMatch` rewrite of the live spec at this address.
    Rewritten(usize),
}

impl SpecKey {
    /// The key for a spec that lives as long as the compilation.
    fn live(spec: &ModeSpec) -> Self {
        Self::Live(spec as *const ModeSpec as usize)
    }

    /// The address this key names, when what it names is a live spec.
    ///
    /// Only a live spec has an allocation for anything to be part of, so this
    /// is the one question the three constructors below turn on.
    fn live_address(self) -> Option<usize> {
        match self {
            Self::Live(address) => Some(address),
            Self::Variant(..) | Self::Rewritten(_) => None,
        }
    }

    /// The key for a spec that sits inside the one this key names.
    ///
    /// A child is part of its parent's allocation, so it is addressable exactly
    /// when its parent is.
    fn inside(self, child: &ModeSpec) -> Option<Self> {
        self.live_address().map(|_| Self::live(child))
    }

    /// The key for the `index`th variant of the spec this key names.
    ///
    /// A variant is built afresh each time it is used, so the spec it was
    /// overlaid on is what names it.
    fn variant(self, index: usize) -> Option<Self> {
        self.live_address()
            .map(|address| Self::Variant(address, index))
    }

    /// The key for the `beforeMatch` rewrite of the spec this key names.
    fn rewritten(self) -> Option<Self> {
        self.live_address().map(Self::Rewritten)
    }
}

impl<'a> Compiler<'a> {
    fn new(language: &'a LanguageSpec) -> Self {
        Self {
            language,
            modes: Vec::new(),
            shared: HashMap::new(),
        }
    }

    fn compile(mut self) -> Result<Grammar, CompileError> {
        let class_name_aliases = self.language.class_name_aliases.clone();
        // The root is a mode like any other, except that its `contains` are the
        // grammar's top-level rules rather than nested ones. Boxed, and held for
        // the whole compilation, so that the rules hanging off it have addresses
        // that mean something to the sharing memo.
        let root_spec = Box::new(ModeSpec {
            contains: self.language.contains.clone(),
            keywords: self.language.keywords.clone(),
            class_name_aliases: class_name_aliases.clone(),
            ..ModeSpec::default()
        });
        let root =
            self.compile_mode(&root_spec, Some(SpecKey::live(&root_spec)), false, None, 0)?;
        Ok(Grammar {
            name: self.language.name.clone(),
            dialect: self.language.dialect(),
            modes: self.modes,
            root,
            class_name_aliases,
            sub_languages: HashMap::new(),
        })
    }

    /// Compiles one mode and everything under it.
    ///
    /// `parent_terminator` is the enclosing mode's terminator pattern, which a
    /// mode with `endsWithParent` appends to its own — that is how a heredoc's
    /// content ends when the enclosing string does.
    fn compile_mode(
        &mut self,
        spec: &ModeSpec,
        key: Option<SpecKey>,
        has_parent: bool,
        parent_terminator: Option<&str>,
        depth: usize,
    ) -> Result<ModeId, CompileError> {
        if depth > MAX_DEPTH {
            return Err(CompileError::TooDeep);
        }

        // This mode's slot is claimed before anything below it is compiled, and
        // its key points at the slot straight away. A grammar that contains
        // itself then finds the mode it is already inside rather than compiling
        // it again and again: bash's `${` nests that way, and real grammars
        // reach the depth limit without this. highlight.js gets the same
        // behaviour from marking a mode compiled before it recurses into it.
        let id = self.modes.len();
        self.modes.push(Mode::empty());
        if let Some(key) = key.filter(|_| !depends_on_parent(spec)) {
            self.shared.insert(key, id);
        }

        let mut mode = Mode::empty();
        mode.label = spec.label.clone();
        mode.ends_parent = spec.ends_parent;
        mode.ends_with_parent = spec.ends_with_parent;
        mode.exclude_begin = spec.exclude_begin;
        mode.exclude_end = spec.exclude_end;
        mode.return_begin = spec.return_begin;
        mode.return_end = spec.return_end;
        mode.skip = spec.skip;
        mode.on_begin = spec.on_begin;
        mode.on_end = spec.on_end;
        mode.before_begin = spec.before_begin;
        mode.sub_language = spec.sub_language.clone();
        mode.relevance = spec.relevance.unwrap_or(1.0);

        // `className` is the old name for `scope`; a grammar may use either.
        mode.scope = match &spec.scope {
            Some(ScopeSpec::Single(name)) if !name.is_empty() => Some(name.clone()),
            _ => None,
        };
        // A map of scopes beside a match is shorthand for `beginScope`, and is
        // the only reason a multi-part `match` does not have to write one —
        // `{"match": ["class", "\\s+", "\\w+"], "scope": {"1": "keyword"}}` is
        // a shape C++ and Arduino both use. Resolved here, rather than written
        // over the compiled scope afterwards, so that the multi-part handling
        // below checks the map the mode actually addresses its groups through.
        let begin_scope = match (&spec.begin_scope, &spec.scope) {
            (Some(scope), _) => Some(scope.clone()),
            (None, Some(ScopeSpec::PerGroup(positions))) => {
                Some(ScopeSpec::PerGroup(positions.clone()))
            }
            _ => None,
        };
        mode.begin_scope = compile_scope(&begin_scope)?;
        mode.end_scope = compile_scope(&spec.end_scope)?;

        // `beforeMatch` is a lookahead: the mode begins where `beforeMatch` is
        // followed by `begin`, but only the lookahead is consumed, so the mode
        // that matches `begin` gets it.
        let (begin, before_match_starts) = rewrite_before_match(spec);

        mode.begin = join_patterns(begin.as_ref());
        mode.end = join_patterns(spec.end.as_ref());
        if mode.end.is_none() && mode.begin.is_some() && spec.ends_with_parent {
            // Nothing to end on its own; the parent's terminator does it.
            mode.end = None;
        }

        // A multi-part pattern addresses its groups through a scope map, and
        // the parts have to be welded into one pattern for the engine.
        if let Some(patterns) = begin.as_ref().filter(|p| p.is_multi()) {
            let scopes = per_group_scopes(&begin_scope).ok_or_else(|| {
                CompileError::BadPattern("multi-part begin without a beginScope map".into())
            })?;
            mode.begin_scope = Some(remap_scopes(patterns, &scopes));
            mode.begin = Some(join_rewritten(patterns));
        }
        if let Some(patterns) = spec.end.as_ref().filter(|p| p.is_multi()) {
            let scopes = per_group_scopes(&spec.end_scope).ok_or_else(|| {
                CompileError::BadPattern("multi-part end without an endScope map".into())
            })?;
            mode.end_scope = Some(remap_scopes(patterns, &scopes));
            mode.end = Some(join_rewritten(patterns));
        }

        // `beginKeywords` is sugar for a pattern over those words, plus a rule
        // that stops it matching after a dot — `obj.keyword` is not a keyword.
        let mut keywords = spec.keywords.clone();
        if let Some(begin_keywords) = &spec.begin_keywords {
            let alternatives = begin_keywords.split(' ').collect::<Vec<_>>().join("|");
            mode.begin = Some(format!(r"\b({alternatives})(?!\.)(?=\b|\s)"));
            mode.before_begin = Some(Callback::SkipIfPrecededByDot);
            // The words themselves become the keywords, so they colour.
            keywords = keywords.or(Some(RawKeywords::Text(begin_keywords.clone())));
        }

        match &spec.illegal {
            Some(IllegalSpec::Patterns(patterns)) => {
                mode.illegal = join_patterns(Some(patterns));
            }
            Some(IllegalSpec::Always(true)) => {
                // "Nothing here is legal" — a pattern that matches anything,
                // so every lexeme is rejected.
                mode.illegal = Some(".".to_string());
            }
            _ => {}
        }

        let dialect = self.language.dialect();
        if let Some(raw) = &keywords {
            mode.keywords = keywords::compile(raw, dialect.case_insensitive);
            let pattern = keyword_pattern_of(raw);
            mode.keyword_pattern = Some(compile_regex(&pattern, dialect, true)?);
        }

        // `endsWithParent` inherits the enclosing terminator, appended to this
        // mode's own end.
        mode.terminator_end = match (&mode.end, mode.ends_with_parent, parent_terminator) {
            (Some(end), true, Some(parent)) => Some(format!("{end}|{parent}")),
            (Some(end), _, _) => Some(end.clone()),
            (None, true, Some(parent)) => Some(parent.to_string()),
            (None, _, _) => None,
        };
        // A nested mode with no end of its own ends at a word boundary — which
        // is what stops a sub-mode swallowing the rest of the line. The
        // top-level mode has no parent to end back into, so it gets none.
        if has_parent && mode.end.is_none() && !mode.ends_with_parent {
            mode.end = Some(r"\B|\b".to_string());
            mode.terminator_end = Some(r"\B|\b".to_string());
        }
        // A mode with an end tests it against the text from the match onwards,
        // separately from the combined matcher.
        if let Some(end) = &mode.end {
            mode.end_regex = Some(compile_regex(end, self.language.dialect(), true)?);
        }

        // Children, with `self` standing for the enclosing mode and `variants`
        // exploded into one child each.
        let mut children = Vec::new();
        for child in &spec.contains {
            // `ref` is how a grammar writes a sub-mode once and uses it in many
            // places. Resolved here rather than by rewriting the tree, so the
            // original stays as the grammar author wrote it.
            let is_ref = child.r#ref.is_some();
            let child = self.resolve(child)?;
            let child_key = key_of_child(key, child, is_ref);
            if child.self_referential {
                // "Reuse the enclosing mode here", for grammars that nest
                // themselves — a heredoc's body inside a heredoc. The child *is*
                // this mode, which is what highlight.js compiles and what the
                // engine's mode stack is built to walk: the nesting it describes
                // is in the text, not in the grammar.
                children.push(id);
                continue;
            }
            match variants_of(child) {
                None => children.push(self.compile_child(
                    child,
                    child_key,
                    mode.terminator_end.as_deref(),
                    depth,
                )?),
                Some(variants) => {
                    for (index, variant) in variants.iter().enumerate() {
                        let variant_key = child_key.and_then(|key| key.variant(index));
                        children.push(self.compile_child(
                            variant,
                            variant_key,
                            mode.terminator_end.as_deref(),
                            depth,
                        )?);
                    }
                }
            }
        }
        let starts = match before_match_starts {
            // The rewrite is a throwaway too — one per spec, so the spec names it.
            // The parent's terminator, not this mode's: `starts` sits where
            // this mode sits, so it inherits what this mode inherited. Handing
            // it this mode's own terminator would give it the synthetic
            // word-boundary one a mode without an end gets, and a rule that
            // matches nothing where nothing should end.
            Some(starts) => Some(self.compile_child(
                &starts,
                key.and_then(SpecKey::rewritten),
                parent_terminator,
                depth,
            )?),
            None => match &spec.starts {
                Some(starts) => {
                    let is_ref = starts.r#ref.is_some();
                    let starts = self.resolve(starts)?;
                    let starts_key = key_of_child(key, starts, is_ref);
                    // As above: `starts` inherits from where this mode
                    // inherited, not from this mode.
                    Some(self.compile_child(starts, starts_key, parent_terminator, depth)?)
                }
                None => None,
            },
        };
        mode.contains = children;
        mode.starts = starts;

        // Into the slot claimed at the top, which anything that referred to this
        // mode while it was being built is already holding. Filled before the
        // matcher is built, because a mode that contains itself is one of the
        // children the matcher is made of: its own begin has to be in the slot
        // by the time that begin is read.
        self.modes[id] = mode;

        // The matcher: this mode's children's begins, its terminator, and its
        // illegal rules, all welded into one.
        let mut patterns = Vec::new();
        let mut rule_kinds = Vec::new();
        for child in self.modes[id].contains.clone() {
            if let Some(begin) = &self.modes[child].begin {
                patterns.push(begin.clone());
                rule_kinds.push(Rule::Begin(child));
            }
        }
        if let Some(terminator) = &self.modes[id].terminator_end {
            patterns.push(terminator.clone());
            rule_kinds.push(Rule::End);
        }
        if let Some(illegal) = &self.modes[id].illegal {
            patterns.push(illegal.clone());
            rule_kinds.push(Rule::Illegal);
        }
        if !patterns.is_empty() {
            let matcher = MultiRegex::new(&patterns, dialect)
                .ok_or_else(|| CompileError::BadPattern(patterns.join("|")))?;
            self.modes[id].matcher = Some(matcher);
        }
        self.modes[id].rules = rule_kinds;
        self.modes[id].rule_patterns = patterns;
        Ok(id)
    }

    /// Follows a `ref` to the mode it names.
    fn resolve<'b>(&self, spec: &'b ModeSpec) -> Result<&'b ModeSpec, CompileError>
    where
        // The referenced mode lives in the language, which outlives everything
        // borrowed from the spec being compiled — so a `ref` can be handed back
        // with the spec's own lifetime.
        'a: 'b,
    {
        match &spec.r#ref {
            None => Ok(spec),
            Some(name) => {
                let referenced = self
                    .language
                    .refs
                    .get(name)
                    .ok_or_else(|| CompileError::UnknownRef(name.clone()))?;
                // A ref pointing at another ref would need resolving twice;
                // highlight.js refuses that rather than following the chain.
                if referenced.r#ref.is_some() {
                    return Err(CompileError::UnknownRef(name.clone()));
                }
                Ok(referenced)
            }
        }
    }

    /// Compiles a child, reusing the result when the child does not care where
    /// it sits.
    fn compile_child(
        &mut self,
        spec: &ModeSpec,
        key: Option<SpecKey>,
        parent_terminator: Option<&str>,
        depth: usize,
    ) -> Result<ModeId, CompileError> {
        // A spec can be shared only if it can be named durably *and* nothing
        // about its compiled form depends on where it sits. Either way it is
        // compiled as a child, which a spec that is not shared still needs to
        // know.
        let memo = key.filter(|_| !depends_on_parent(spec));
        if let Some(key) = memo {
            if let Some(existing) = self.shared.get(&key) {
                return Ok(*existing);
            }
        }
        let id = self.compile_mode(spec, key, true, parent_terminator, depth + 1)?;
        if let Some(key) = memo {
            self.shared.insert(key, id);
        }
        Ok(id)
    }
}

/// The key for a child spec, which must already have had its `ref` followed.
///
/// A `ref` names a spec in the language, which outlives the compilation however
/// briefly the reference itself is written; every other child is written inside
/// its parent, and so is addressable exactly when the parent is.
fn key_of_child(parent: Option<SpecKey>, resolved: &ModeSpec, is_ref: bool) -> Option<SpecKey> {
    if is_ref {
        Some(SpecKey::live(resolved))
    } else {
        parent.and_then(|parent| parent.inside(resolved))
    }
}

/// Whether a mode's compiled form depends on where it sits in the grammar.
///
/// `endsWithParent` inherits the enclosing mode's terminator, so the same mode
/// under two parents compiles to two different things and cannot be shared.
fn depends_on_parent(spec: &ModeSpec) -> bool {
    spec.ends_with_parent || spec.starts.as_ref().map_or(false, |s| depends_on_parent(s))
}

/// A mode's variants, if it has any.
///
/// `None` means the mode stands for itself, and the caller must compile *that*
/// spec rather than a copy. A copy would be a throwaway with no name to share
/// under, and before the memo was keyed on names rather than addresses, it was
/// worse than that: two copies made in turn landed on the same address, so the
/// second of two sub-modes silently recompiled as the first.
fn variants_of(spec: &ModeSpec) -> Option<Vec<ModeSpec>> {
    if spec.variants.is_empty() {
        return None;
    }
    Some(
        spec.variants
            .iter()
            .map(|variant| overlay(spec, variant))
            .collect(),
    )
}

/// `base` with `variant`'s fields laid over it, for the fields a variant sets.
///
/// highlight.js does this with a `copyWith` that takes every non-null field, so
/// the question is what "set" means for a field that is not nullable here. A
/// `false` cannot be told from an absent one, so booleans count as set when
/// they are true — which is the only direction a variant uses them, and a
/// variant that wanted to clear a flag would be written as a separate mode.
fn overlay(base: &ModeSpec, variant: &ModeSpec) -> ModeSpec {
    let pick = |over: bool, under: bool| over || under;

    ModeSpec {
        name: variant.name.clone().or_else(|| base.name.clone()),
        case_insensitive: pick(variant.case_insensitive, base.case_insensitive),
        disable_autodetect: pick(variant.disable_autodetect, base.disable_autodetect),
        superset_of: variant
            .superset_of
            .clone()
            .or_else(|| base.superset_of.clone()),
        aliases: if variant.aliases.is_empty() {
            base.aliases.clone()
        } else {
            variant.aliases.clone()
        },
        class_name_aliases: if variant.class_name_aliases.is_empty() {
            base.class_name_aliases.clone()
        } else {
            variant.class_name_aliases.clone()
        },
        begin: variant.begin.clone().or_else(|| base.begin.clone()),
        end: variant.end.clone().or_else(|| base.end.clone()),
        r#match: variant.r#match.clone().or_else(|| base.r#match.clone()),
        illegal: variant.illegal.clone().or_else(|| base.illegal.clone()),
        before_match: variant
            .before_match
            .clone()
            .or_else(|| base.before_match.clone()),
        scope: variant.scope.clone().or_else(|| base.scope.clone()),
        begin_scope: variant
            .begin_scope
            .clone()
            .or_else(|| base.begin_scope.clone()),
        end_scope: variant.end_scope.clone().or_else(|| base.end_scope.clone()),
        contains: if variant.contains.is_empty() {
            base.contains.clone()
        } else {
            variant.contains.clone()
        },
        variants: Vec::new(),
        starts: variant.starts.clone().or_else(|| base.starts.clone()),
        sub_language: variant
            .sub_language
            .clone()
            .or_else(|| base.sub_language.clone()),
        keywords: variant.keywords.clone().or_else(|| base.keywords.clone()),
        begin_keywords: variant
            .begin_keywords
            .clone()
            .or_else(|| base.begin_keywords.clone()),
        lexemes: variant.lexemes.clone().or_else(|| base.lexemes.clone()),
        relevance: variant.relevance.or(base.relevance),
        label: variant.label.clone().or_else(|| base.label.clone()),
        ends_parent: pick(variant.ends_parent, base.ends_parent),
        ends_with_parent: pick(variant.ends_with_parent, base.ends_with_parent),
        end_same_as_begin: pick(variant.end_same_as_begin, base.end_same_as_begin),
        exclude_begin: pick(variant.exclude_begin, base.exclude_begin),
        exclude_end: pick(variant.exclude_end, base.exclude_end),
        return_begin: pick(variant.return_begin, base.return_begin),
        return_end: pick(variant.return_end, base.return_end),
        skip: pick(variant.skip, base.skip),
        self_referential: pick(variant.self_referential, base.self_referential),
        on_begin: variant.on_begin.or(base.on_begin),
        on_end: variant.on_end.or(base.on_end),
        before_begin: variant.before_begin.or(base.before_begin),
        r#ref: variant.r#ref.clone().or_else(|| base.r#ref.clone()),
    }
}

/// `match` is `begin` with no end; `beforeMatch` becomes a lookahead.
fn rewrite_before_match(spec: &ModeSpec) -> (Option<Patterns>, Option<ModeSpec>) {
    let begin = spec.r#match.clone().or_else(|| spec.begin.clone());
    let Some(before_match) = &spec.before_match else {
        return (begin, None);
    };
    let Some(begin) = begin else {
        return (None, None);
    };
    // The lookahead is where the mode really begins; the `starts` mode then
    // matches the real begin, so it is not consumed by the lookahead.
    let looked_ahead = match &begin {
        Patterns::Single(one) => Patterns::Single(format!("{before_match}(?={one})")),
        Patterns::Many(parts) => Patterns::Many(
            parts
                .iter()
                .map(|part| format!("{before_match}(?={part})"))
                .collect(),
        ),
    };
    let inner = ModeSpec {
        begin: Some(begin),
        relevance: Some(0.0),
        ends_parent: true,
        before_match: None,
        ..spec.clone()
    };
    let starts = ModeSpec {
        relevance: Some(0.0),
        contains: vec![inner],
        ..ModeSpec::default()
    };
    (Some(looked_ahead), Some(starts))
}

/// The `$pattern` a grammar may put in its keyword table, or the default.
fn keyword_pattern_of(raw: &RawKeywords) -> String {
    match raw {
        RawKeywords::Scoped(map) => map
            .get("$pattern")
            .and_then(|pattern| match pattern {
                RawKeywords::Text(text) => Some(text.clone()),
                RawKeywords::List(list) => Some(list.join("|")),
                RawKeywords::Scoped(_) => None,
            })
            .unwrap_or_else(|| r"\w+".to_string()),
        _ => r"\w+".to_string(),
    }
}

/// The per-group scopes of a spec, if it has them.
fn per_group_scopes(spec: &Option<ScopeSpec>) -> Option<HashMap<String, String>> {
    match spec {
        Some(ScopeSpec::PerGroup(positions)) => Some(positions.clone()),
        _ => None,
    }
}

/// One scope for the whole match, or one per group.
fn compile_scope(spec: &Option<ScopeSpec>) -> Result<Option<CompiledScope>, CompileError> {
    Ok(match spec {
        None => None,
        Some(ScopeSpec::Single(name)) => Some(CompiledScope {
            positions: HashMap::new(),
            emit: HashMap::new(),
            multi: false,
            wrap: Some(name.clone()),
        }),
        Some(ScopeSpec::PerGroup(positions)) => Some(scope_from_positions(positions.clone())),
    })
}

fn scope_from_positions(positions: HashMap<String, String>) -> CompiledScope {
    let mut mapped = HashMap::new();
    for (group, name) in positions {
        if let Ok(group) = group.parse::<usize>() {
            mapped.insert(group, name);
        }
    }
    CompiledScope {
        positions: mapped,
        emit: HashMap::new(),
        multi: false,
        wrap: None,
    }
}

/// Lines the scopes up with the groups the engine actually captures.
///
/// A multi-part pattern `(a)(((b)))(c)` captures five groups where the grammar
/// named three scopes, so the numbers have to be remapped and the groups that
/// are merely nested inside others marked so they are not emitted twice.
fn remap_scopes(patterns: &Patterns, scopes: &HashMap<String, String>) -> CompiledScope {
    let mut positions = HashMap::new();
    let mut emit = HashMap::new();
    let mut offset = 0usize;
    for (index, part) in patterns.parts().iter().enumerate() {
        let group = index + 1 + offset;
        if let Some(name) = scopes.get(&(index + 1).to_string()) {
            positions.insert(group, name.clone());
        }
        emit.insert(group, true);
        offset += count_groups_for_remap(part);
    }
    CompiledScope {
        positions,
        emit,
        multi: true,
        wrap: None,
    }
}

fn count_groups_for_remap(pattern: &str) -> usize {
    // Reuses the walker the multi-regex needs, so the two cannot disagree about
    // what counts as a group.
    crate::highlight::regex::count_capture_groups(pattern)
}

/// Joins a pattern's parts for the engine.
///
/// A single pattern is itself; several are alternatives, which is how
/// highlight.js reads a list.
fn join_patterns(patterns: Option<&Patterns>) -> Option<String> {
    let joined = patterns?.parts().join("|");
    if joined.is_empty() {
        None
    } else {
        Some(joined)
    }
}

/// Joins a multi-part pattern into one, renumbering its backreferences — the
/// parts become one pattern, so their groups move.
fn join_rewritten(patterns: &Patterns) -> String {
    let parts: Vec<String> = patterns
        .parts()
        .iter()
        .enumerate()
        .map(|(index, part)| {
            let already = patterns.parts()[..index]
                .iter()
                .map(|previous| count_groups_for_remap(previous))
                .sum::<usize>()
                + index;
            // Each part is wrapped in a group of its own, because that is what
            // the scope map addresses: the map's keys are part numbers, and the
            // engine reads the coloured text out of the group each part landed
            // in. Joining the parts without wrapping them puts the groups in
            // the wrong places — or, for a rule that writes its scope map, in
            // no place at all.
            format!("({})", rewrite_backreferences(part, already))
        })
        .collect();
    parts.join("")
}

fn compile_regex(source: &str, dialect: Dialect, multiline: bool) -> Result<Regex, CompileError> {
    Regex::with_flags(
        source,
        Flags {
            icase: dialect.case_insensitive,
            unicode: dialect.unicode,
            multiline,
            ..Flags::default()
        },
    )
    .map_err(|_| CompileError::BadPattern(source.to_string()))
}
