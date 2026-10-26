//! A regular expression the user typed into the search box wrapped in `/…/`.
//!
//! [`parse`] folds the pattern like the index folds text (no nikud, ASCII quotes, maqaf as a
//! space), drops anchors at its edges and sorts it into one of three kinds:
//!
//! - [`RegexKind::Word`] — no explicit whitespace: the pattern is matched against whole index
//!   terms, like the advanced search's regex word. `.` and negated classes stay inside a word.
//! - [`RegexKind::Sequence`] — words separated at the top level by a literal space or `\s+`,
//!   no part containing whitespace of its own: an adjacent phrase of term patterns, which
//!   may continue onto the next line like any phrase.
//! - [`RegexKind::Line`] — anything else that can span a space (`שמע .* ישראל`,
//!   `(\S+\s){2}ישראל`, an inner `\b`): matched against each line's text, normalized as the
//!   app's `cleanLineForSearch` does, with a word boundary implied at both edges. Candidates
//!   come from the fixed words of the pattern when they are selective; otherwise the books
//!   in scope are read whole, in parallel, and the search can be cancelled.
//!
//! Term patterns never reach the dictionary as typed: bounded repetitions above
//! [`MAX_BOUNDED_REPEAT`] become unbounded and classes are narrowed to the term alphabet
//! before [`compile_user_term_automaton`] builds a size-limited DFA, and when that relaxed
//! a pattern every term it accepts is checked against the exact one. No input compiles for
//! long: the limits fail fast and the pattern is reported invalid.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use regex::{Regex, RegexBuilder};
use regex_syntax::hir::{Class, ClassUnicode, ClassUnicodeRange, Hir, HirKind, Look, Repetition};
use tantivy::collector::{Collector, Count, SegmentCollector};
use tantivy::query::{BooleanQuery, EnableScoring, Occur, Query, TermQuery, TermSetQuery};
use tantivy::query::{EmptyScorer, Explanation, Scorer, Weight};
use tantivy::schema::{Field, IndexRecordOption, Schema, Value};
use tantivy::{
    DocAddress, DocId, DocSet, Score, Searcher, SegmentOrdinal, SegmentReader, TantivyDocument,
    Term, TERMINATED,
};

use crate::api::search_engine::{
    indexed_texts, line_source_book, HighlightRange, SearchResult, TextStatus, UserRegexKind,
    UserRegexLineMatches, UserRegexStreamUpdate, LINE_CHECK_FIELD,
};
use crate::book_order::{book_number_of, OrderKind, OrderTables};
use crate::doc_store::DocumentLoader;
use crate::gap_phrase::Proximity;
use crate::hebrew_query::{
    fold_presentation_form, is_invisible_char, is_word_mark, tag_breaks_word,
};
use crate::highlight_matcher::{Match, PreparedHighlightMatcher, Range};
use crate::line_source::{self, FrameDecoder, RawRow, Source};
use crate::quote_expansion::{is_quote, QuoteTransparent};
use crate::term_automaton::{self, TermDfa};

/// Longest pattern accepted, in characters.
const MAX_PATTERN_CHARS: usize = 300;
/// A bounded repetition above this becomes unbounded in a term automaton (and is verified).
const MAX_BOUNDED_REPEAT: u32 = 7;
const NEST_LIMIT: u32 = 40;
const LINE_REGEX_SIZE_LIMIT: usize = 8 << 20;
/// [`cost`] above which a term automaton is relaxed further, and a verifier or line regex
/// refused: compiling past it takes long before any size limit is reached.
const TERM_COST_LIMIT: u64 = 2_000;
const REGEX_COST_LIMIT: u64 = 20_000;
/// Dictionary keys a term scan may visit when verifying a relaxed pattern, per segment.
const MAX_SCANNED_KEYS_PER_SEGMENT: u64 = 20_000_000;
/// Candidate lines verified by reading them one by one; more read whole books instead.
pub(crate) const CANDIDATE_FETCH_LIMIT: u32 = 5_000;
/// A fixed word whose terms cover more postings than this narrows nothing worth reading.
const CLAUSE_POSTINGS_CAP: u64 = 3_000_000;
const CLAUSE_MAX_TERMS: u32 = 50_000;
/// Line hits kept by one search; more end it with `truncated`.
pub(crate) const MAX_LINE_HITS: usize = 100_000;
/// Lines read and verified per transaction on the candidate path.
const VERIFY_BATCH: usize = 256;
/// Books read before results and progress are reported.
const SCAN_BATCH_BOOKS: usize = 256;
const SCAN_BATCH_LINES: u64 = 400_000;
/// Candidates fewer than a book's rows over this are read row by row; more, with the book.
const SPARSE_ROW_FACTOR: u64 = 200;
/// Connections reading books at once: past four, reading gains little (the measured
/// throughput: 1 → 9.4 s, 4 → 4.1 s, 12 → 3.6 s for the whole library).
const SCAN_READERS: usize = 4;
/// Books read ahead of the threads that match them.
const FETCH_AHEAD: usize = 16;
const CANCEL_CHECK_ROWS: usize = 512;

// ── Parsing ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RegexKind {
    Word,
    Sequence,
    Line,
}

impl RegexKind {
    pub(crate) fn api(self) -> UserRegexKind {
        match self {
            RegexKind::Word => UserRegexKind::SingleWord,
            RegexKind::Sequence => UserRegexKind::WordSequence,
            RegexKind::Line => UserRegexKind::CrossSpace,
        }
    }
}

/// One word of a term pattern.
pub(crate) struct TermPart {
    /// The exact pattern, as the regex crate reads it (highlighting, verification).
    pub(crate) pattern: String,
    /// The relaxed pattern the automaton was compiled from (the cache key of its terms).
    relaxed: String,
    automaton: Arc<UserTermAutomaton>,
    /// `\A(?:pattern)\z`, when the automaton accepts more than the pattern.
    verify: Option<Regex>,
}

/// A fixed word every match of a line pattern contains, as the tokenizer makes it a term.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fragment {
    pub(crate) text: String,
    /// Bounded on both sides in the pattern: a whole term, or one glued to others by
    /// [`GLUE`]; otherwise only contained in a term.
    pub(crate) whole: bool,
}

pub(crate) struct LinePlan {
    regex: Regex,
    /// The pattern without word-boundary assertions: a superset that the regex crate runs as a
    /// DFA on Hebrew text, where a Unicode `\b` forces its slow engines.
    prefilter: Regex,
    /// Fixed words: every clause (an OR of fragments) holds in every matching line.
    pub(crate) clauses: Vec<Vec<Fragment>>,
}

pub(crate) struct UserRegex {
    pub(crate) kind: RegexKind,
    /// The folded pattern without its `/…/` and edge anchors.
    pub(crate) pattern: String,
    pub(crate) parts: Vec<TermPart>,
    pub(crate) line: Option<LinePlan>,
}

/// Parses `raw` (`/pattern/`). `Err` holds why it is not a usable regex; the caller then
/// searches the text as ordinary words.
pub(crate) fn parse(raw: &str) -> Result<UserRegex, String> {
    let trimmed = raw.trim();
    let inner = trimmed
        .strip_prefix('/')
        .and_then(|rest| rest.strip_suffix('/'))
        .ok_or_else(|| "not wrapped in /…/".to_string())?;
    if inner.chars().count() > MAX_PATTERN_CHARS {
        return Err(format!("longer than {MAX_PATTERN_CHARS} characters"));
    }
    let folded = fold_pattern(inner);
    if folded.trim().is_empty() {
        return Err("empty pattern".to_string());
    }
    let hir = regex_syntax::ParserBuilder::new()
        .case_insensitive(true)
        .unicode(true)
        .utf8(true)
        .nest_limit(NEST_LIMIT)
        .build()
        .parse(&folded)
        .map_err(|e| match e {
            regex_syntax::Error::Parse(e) => format!("syntax: {}", e.kind()),
            regex_syntax::Error::Translate(e) => format!("syntax: {}", e.kind()),
            other => format!("syntax: {other}"),
        })?;
    let mut elements = top_elements(strip_captures(hir));
    loop {
        let edge =
            |e: &Hir| matches!(e.kind(), HirKind::Look(_) | HirKind::Empty) || is_separator(e);
        if elements.first().is_some_and(edge) {
            elements.remove(0);
        } else if elements.last().is_some_and(edge) {
            elements.pop();
        } else {
            break;
        }
    }
    if elements.is_empty() {
        return Err("matches only anchors or spaces".to_string());
    }
    let inner_looks = elements
        .iter()
        .any(|e| !e.properties().look_set().is_empty());
    let mut parts: Vec<Vec<Hir>> = vec![Vec::new()];
    for element in &elements {
        if is_separator(element) {
            if parts.last().is_some_and(|p| !p.is_empty()) {
                parts.push(Vec::new());
            }
        } else {
            parts
                .last_mut()
                .expect("one part at least")
                .push(element.clone());
        }
    }
    let parts: Vec<Hir> = parts.into_iter().map(Hir::concat).collect();
    let whole = Hir::concat(elements.clone());
    let pattern = whole.to_string();
    let spans_space = inner_looks
        || parts.iter().any(has_explicit_whitespace)
        || (parts.len() > 1 && parts.iter().any(is_wildcard_word));
    if spans_space {
        let line = line_plan(&whole, &elements)?;
        return Ok(UserRegex {
            kind: RegexKind::Line,
            pattern,
            parts: Vec::new(),
            line: Some(line),
        });
    }
    let kind = if parts.len() == 1 {
        RegexKind::Word
    } else {
        RegexKind::Sequence
    };
    let parts = parts
        .iter()
        .map(prepare_term_part)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(UserRegex {
        kind,
        pattern,
        parts,
        line: None,
    })
}

/// The pattern with the text's foldings applied to it: marks dropped, Hebrew and typographic
/// quotes as ASCII, maqaf and paseq as a space, presentation forms decomposed.
fn fold_pattern(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.chars() {
        if is_dropped_mark(c) || is_invisible_char(c) {
            continue;
        }
        match c {
            '\u{05F3}' | '\u{2018}' | '\u{2019}' => out.push('\''),
            '\u{05F4}' | '\u{201C}' | '\u{201D}' => out.push('"'),
            '\u{05BE}' | '\u{05C0}' => out.push(' '),
            _ => match fold_presentation_form(c) {
                Some(folded) => out.extend(folded.chars().filter(|c| !is_word_mark(*c))),
                None => out.push(c),
            },
        }
    }
    out
}

/// What `cleanLineForSearch` deletes: the whole nikud/cantillation block (sof pasuq too),
/// except maqaf and paseq, which it turns into a space.
fn is_dropped_mark(c: char) -> bool {
    (matches!(c, '\u{0591}'..='\u{05C7}') && !matches!(c, '\u{05BE}' | '\u{05C0}'))
        || is_word_mark(c)
}

fn strip_captures(hir: Hir) -> Hir {
    match hir.into_kind() {
        HirKind::Capture(capture) => strip_captures(*capture.sub),
        HirKind::Concat(subs) => Hir::concat(subs.into_iter().map(strip_captures).collect()),
        HirKind::Alternation(subs) => {
            Hir::alternation(subs.into_iter().map(strip_captures).collect())
        }
        HirKind::Repetition(rep) => Hir::repetition(Repetition {
            sub: Box::new(strip_captures(*rep.sub)),
            ..rep
        }),
        kind => rebuild(kind),
    }
}

fn rebuild(kind: HirKind) -> Hir {
    match kind {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(lit) => Hir::literal(lit.0),
        HirKind::Class(class) => Hir::class(class),
        HirKind::Look(look) => Hir::look(look),
        HirKind::Repetition(rep) => Hir::repetition(rep),
        HirKind::Capture(capture) => Hir::capture(capture),
        HirKind::Concat(subs) => Hir::concat(subs),
        HirKind::Alternation(subs) => Hir::alternation(subs),
    }
}

/// The top-level concatenation, every literal split into its characters.
fn top_elements(hir: Hir) -> Vec<Hir> {
    let subs = match hir.into_kind() {
        HirKind::Concat(subs) => subs,
        kind => vec![rebuild(kind)],
    };
    let mut out = Vec::with_capacity(subs.len());
    for sub in subs {
        match sub.kind() {
            HirKind::Literal(lit) => match std::str::from_utf8(&lit.0) {
                Ok(text) => {
                    for c in text.chars() {
                        let mut buf = [0u8; 4];
                        out.push(Hir::literal(c.encode_utf8(&mut buf).as_bytes().to_vec()));
                    }
                }
                Err(_) => out.push(sub),
            },
            _ => out.push(sub),
        }
    }
    out
}

fn literal_text(hir: &Hir) -> Option<&str> {
    match hir.kind() {
        HirKind::Literal(lit) => std::str::from_utf8(&lit.0).ok(),
        _ => None,
    }
}

/// Whitespace and nothing else, at least once: what separates the words of a sequence.
fn is_separator(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Literal(_) => {
            literal_text(hir).is_some_and(|t| t.chars().all(char::is_whitespace))
        }
        HirKind::Class(Class::Unicode(class)) => {
            !class.ranges().is_empty()
                && class
                    .ranges()
                    .iter()
                    .all(|r| (r.start()..=r.end()).all(char::is_whitespace))
        }
        HirKind::Repetition(rep) => rep.min >= 1 && is_separator(&rep.sub),
        _ => false,
    }
}

fn class_contains(class: &ClassUnicode, c: char) -> bool {
    class
        .ranges()
        .iter()
        .any(|r| r.start() <= c && c <= r.end())
}

/// Whitespace the pattern asks for: a literal space, or a class that has a space and no
/// letter (`\s`, `\W`, `[ ,]`). `.` and `[^א]` have letters too, so they stay in a word.
fn has_explicit_whitespace(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Literal(_) => {
            literal_text(hir).is_some_and(|t| t.chars().any(char::is_whitespace))
        }
        HirKind::Class(Class::Unicode(class)) => {
            class_contains(class, ' ') && !class_contains(class, 'א') && !class_contains(class, 'a')
        }
        HirKind::Class(Class::Bytes(_)) | HirKind::Empty | HirKind::Look(_) => false,
        HirKind::Repetition(rep) => has_explicit_whitespace(&rep.sub),
        HirKind::Capture(capture) => has_explicit_whitespace(&capture.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => {
            subs.iter().any(has_explicit_whitespace)
        }
    }
}

/// A word of a sequence that names no letter of its own (`.*`, `\S+`): the user means
/// "anything here", which only a line pattern can honour.
fn is_wildcard_word(hir: &Hir) -> bool {
    fn literal_free(hir: &Hir) -> bool {
        match hir.kind() {
            HirKind::Literal(_) => false,
            HirKind::Class(Class::Unicode(class)) => class_contains(class, 'א'),
            HirKind::Class(Class::Bytes(_)) => true,
            HirKind::Empty | HirKind::Look(_) => true,
            HirKind::Repetition(rep) => literal_free(&rep.sub),
            HirKind::Capture(capture) => literal_free(&capture.sub),
            HirKind::Concat(subs) | HirKind::Alternation(subs) => subs.iter().all(literal_free),
        }
    }
    literal_free(hir)
}

// ── Term patterns ──────────────────────────────────────────────────────────────

/// Letters, digits and quotes that index terms are made of. Wildcards are narrowed to it.
static TERM_ALPHABET: Lazy<ClassUnicode> = Lazy::new(|| {
    ClassUnicode::new([
        ClassUnicodeRange::new('"', '"'),
        ClassUnicodeRange::new('\'', '\''),
        ClassUnicodeRange::new('0', '9'),
        ClassUnicodeRange::new('a', 'z'),
        ClassUnicodeRange::new('\u{05D0}', '\u{05EA}'),
        ClassUnicodeRange::new('\u{05F0}', '\u{05F2}'),
    ])
});

static WORD_CLASS: Lazy<ClassUnicode> =
    Lazy::new(|| match regex_syntax::parse(r"\w").map(Hir::into_kind) {
        Ok(HirKind::Class(Class::Unicode(class))) => class,
        _ => ClassUnicode::empty(),
    });

/// How much compiling `hir` costs, roughly in automaton states: every class weighs its
/// ranges, and a repetition multiplies its body by its count.
fn cost(hir: &Hir) -> u64 {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => 1,
        HirKind::Literal(lit) => lit.0.len() as u64,
        HirKind::Class(Class::Unicode(class)) => 1 + class.ranges().len() as u64,
        HirKind::Class(Class::Bytes(class)) => 1 + class.ranges().len() as u64,
        HirKind::Repetition(rep) => {
            let copies = u64::from(rep.max.unwrap_or(rep.min).max(rep.min)).max(1) + 1;
            cost(&rep.sub).saturating_mul(copies)
        }
        HirKind::Capture(capture) => cost(&capture.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => subs
            .iter()
            .fold(0u64, |acc, sub| acc.saturating_add(cost(sub))),
    }
}

fn prepare_term_part(exact: &Hir) -> Result<TermPart, String> {
    let pattern = exact.to_string();
    let mut last_error = String::from("too large");
    for aggressive in [false, true] {
        let (relaxed, changed) = relax(exact, aggressive);
        if cost(&relaxed) > TERM_COST_LIMIT {
            continue;
        }
        match compile_user_term_automaton(&relaxed) {
            Ok(automaton) => {
                let verify = if changed {
                    // Terms are made of the narrowed alphabet: the narrowed exact pattern
                    // decides them alike, and compiles far smaller than full Unicode classes.
                    let narrowed = narrow(exact);
                    if cost(&narrowed) > REGEX_COST_LIMIT {
                        return Err("too complex: bounded repetitions too large".to_string());
                    }
                    Some(
                        RegexBuilder::new(&format!(r"\A(?:{narrowed})\z"))
                            .case_insensitive(true)
                            .size_limit(LINE_REGEX_SIZE_LIMIT)
                            .dfa_size_limit(LINE_REGEX_SIZE_LIMIT)
                            .build()
                            .map_err(|e| format!("too complex: {e}"))?,
                    )
                } else {
                    None
                };
                return Ok(TermPart {
                    pattern,
                    relaxed: relaxed.to_string(),
                    automaton: Arc::new(automaton),
                    verify,
                });
            }
            Err(e) => last_error = e,
        }
    }
    Err(format!("too complex: {last_error}"))
}

fn has_bounded_repetition(hir: &Hir) -> bool {
    match hir.kind() {
        HirKind::Repetition(rep) => {
            rep.max.is_some_and(|max| max > 1) || has_bounded_repetition(&rep.sub)
        }
        HirKind::Capture(capture) => has_bounded_repetition(&capture.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => {
            subs.iter().any(has_bounded_repetition)
        }
        _ => false,
    }
}

/// `hir` with every class narrowed to [`TERM_ALPHABET`].
fn narrow(hir: &Hir) -> Hir {
    match hir.kind() {
        HirKind::Class(Class::Unicode(class)) => {
            let mut narrowed = class.clone();
            narrowed.intersect(&TERM_ALPHABET);
            Hir::class(Class::Unicode(narrowed))
        }
        HirKind::Repetition(rep) => Hir::repetition(Repetition {
            sub: Box::new(narrow(&rep.sub)),
            ..rep.clone()
        }),
        HirKind::Capture(capture) => narrow(&capture.sub),
        HirKind::Concat(subs) => Hir::concat(subs.iter().map(narrow).collect()),
        HirKind::Alternation(subs) => Hir::alternation(subs.iter().map(narrow).collect()),
        _ => hir.clone(),
    }
}

/// The pattern as a term automaton accepts it: classes narrowed to [`TERM_ALPHABET`] (no
/// change for any term), lazy quantifiers made greedy (a whole term has one match), and
/// long bounded repetitions unbounded — or, `aggressive`, every bounded repetition. The
/// flag says the result accepts terms the pattern does not.
fn relax(hir: &Hir, aggressive: bool) -> (Hir, bool) {
    match hir.kind() {
        HirKind::Class(Class::Unicode(class)) => {
            let mut narrowed = class.clone();
            narrowed.intersect(&TERM_ALPHABET);
            (Hir::class(Class::Unicode(narrowed)), false)
        }
        HirKind::Repetition(rep) => {
            let (sub, changed) = relax(&rep.sub, aggressive);
            let limit = if aggressive { 1 } else { MAX_BOUNDED_REPEAT };
            // A bounded count of bounded counts multiplies the DFA's states.
            let nested = rep.max.is_some_and(|max| max > 1) && has_bounded_repetition(&sub);
            let long = nested || rep.min > limit || rep.max.is_some_and(|max| max > limit);
            let (min, max) = if long {
                (rep.min.min(limit), None)
            } else {
                (rep.min, rep.max)
            };
            (
                Hir::repetition(Repetition {
                    min,
                    max,
                    greedy: true,
                    sub: Box::new(sub),
                }),
                changed || long,
            )
        }
        HirKind::Capture(capture) => relax(&capture.sub, aggressive),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => {
            let mut changed = false;
            let relaxed: Vec<Hir> = subs
                .iter()
                .map(|sub| {
                    let (r, c) = relax(sub, aggressive);
                    changed |= c;
                    r
                })
                .collect();
            let hir = if matches!(hir.kind(), HirKind::Concat(_)) {
                Hir::concat(relaxed)
            } else {
                Hir::alternation(relaxed)
            };
            (hir, changed)
        }
        _ => (hir.clone(), false),
    }
}

/// A DFA over index-term bytes that accepts the terms `hir` matches as a whole, written or
/// without their quotes (schema 6 indexes no quote-free twin).
pub(crate) struct UserTermAutomaton(QuoteTransparent<TermDfa>);

/// The one place a user term pattern becomes an automaton. Callers bound its input with
/// [`cost`] first: the compiler's own state limit is reached only after a long build.
pub(crate) fn compile_user_term_automaton(hir: &Hir) -> Result<UserTermAutomaton, String> {
    term_automaton::compile_pattern(&hir.to_string())
        .map(|dfa| UserTermAutomaton(QuoteTransparent::from_arc(dfa)))
}

impl tantivy_fst::Automaton for UserTermAutomaton {
    type State = <QuoteTransparent<TermDfa> as tantivy_fst::Automaton>::State;

    fn start(&self) -> Self::State {
        self.0.start()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        self.0.is_match(state)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        self.0.can_match(state)
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        self.0.accept(state, byte)
    }
}

/// Whether `verify` accepts `term` as the quote-transparent automaton does: written, or
/// without its quotes.
fn verifies(verify: &Regex, term: &str) -> bool {
    if verify.is_match(term) {
        return true;
    }
    let bare: String = term
        .chars()
        .filter(|&c| !c.is_ascii() || !is_quote(c as u8))
        .collect();
    !bare.is_empty() && bare.len() != term.len() && verify.is_match(&bare)
}

impl UserTermAutomaton {
    pub(crate) fn matches(&self, term: &str) -> bool {
        use tantivy_fst::Automaton;
        let mut state = self.start();
        for &b in term.as_bytes() {
            state = self.accept(&state, b);
        }
        self.is_match(&state)
    }
}

impl TermPart {
    #[cfg(test)]
    pub(crate) fn accepts(&self, term: &str) -> bool {
        self.automaton.matches(term) && self.verify.as_ref().is_none_or(|v| verifies(v, term))
    }

    #[cfg(test)]
    pub(crate) fn relaxed(&self) -> bool {
        self.verify.is_some()
    }
}

/// The terms of `field` one part matches, within the budgets: `truncated` when collection
/// stopped at either (the rest of the dictionary was not looked at).
pub(crate) struct PartTerms {
    pub(crate) terms: Arc<Vec<String>>,
    /// The summed document frequencies of `terms`.
    pub(crate) postings: u64,
    pub(crate) truncated: bool,
}

/// Which index and which of its states a cached answer belongs to: the snapshot the page
/// cache keys by, whose segment order is the one cached [`DocAddress`]es use.
pub(crate) fn index_identity(searcher: &Searcher) -> u64 {
    crate::page_cache::snapshot_of(searcher)
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct PartTermsKey {
    index: u64,
    field: Field,
    relaxed: String,
    exact: String,
    max_terms: u32,
    postings_budget: u64,
}

impl PartTermsKey {
    fn new(
        searcher: &Searcher,
        field: Field,
        part: &TermPart,
        max_terms: u32,
        postings_budget: u64,
    ) -> Self {
        Self {
            index: index_identity(searcher),
            field,
            relaxed: part.relaxed.clone(),
            exact: part.pattern.clone(),
            max_terms,
            postings_budget,
        }
    }
}

/// A part's terms, their postings and whether they were truncated.
type CachedTerms = (Arc<Vec<String>>, u64, bool);

static TERM_CACHE: Lazy<Mutex<lru::LruCache<PartTermsKey, CachedTerms>>> =
    Lazy::new(|| Mutex::new(lru::LruCache::new(std::num::NonZeroUsize::new(16).unwrap())));

/// The terms of `field` that `part` matches as a whole, collected over every segment in
/// parallel and verified against the exact pattern when the automaton was relaxed.
pub(crate) fn collect_part_terms(
    searcher: &Searcher,
    field: Field,
    part: &TermPart,
    max_terms: u32,
    postings_budget: u64,
    cancel: &AtomicBool,
) -> anyhow::Result<PartTerms> {
    // A cached set must not let a cancelled call through.
    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!(CANCELLED);
    }
    let key = PartTermsKey::new(searcher, field, part, max_terms, postings_budget);
    if let Some((terms, postings, truncated)) = TERM_CACHE.lock().unwrap().get(&key) {
        return Ok(PartTerms {
            terms: terms.clone(),
            postings: *postings,
            truncated: *truncated,
        });
    }
    use rayon::prelude::*;
    // Each segment collects up to the budgets on its own; merging in segment order then
    // truncates at the same terms on every run.
    let per_segment = searcher
        .segment_readers()
        .par_iter()
        .map(|reader| -> anyhow::Result<(Vec<(String, u64)>, bool)> {
            let inverted = reader.inverted_index(field)?;
            let mut stream = inverted
                .terms()
                .search(part.automaton.as_ref())
                .into_stream()?;
            let mut found = Vec::new();
            let (mut scanned, mut postings) = (0u64, 0u64);
            while stream.advance() {
                scanned += 1;
                if scanned % 4096 == 0 && cancel.load(Ordering::Relaxed) {
                    break;
                }
                if scanned > MAX_SCANNED_KEYS_PER_SEGMENT {
                    return Ok((found, true));
                }
                let Ok(term) = std::str::from_utf8(stream.key()) else {
                    continue;
                };
                if part.verify.as_ref().is_some_and(|v| !verifies(v, term)) {
                    continue;
                }
                let freq = u64::from(stream.value().doc_freq);
                found.push((term.to_string(), freq));
                postings += freq;
                if found.len() >= max_terms as usize || postings >= postings_budget {
                    return Ok((found, true));
                }
            }
            Ok((found, false))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!(CANCELLED);
    }
    let mut set: HashSet<String> = HashSet::new();
    let mut postings = 0u64;
    let mut truncated = false;
    'segments: for (found, stopped) in per_segment {
        for (term, freq) in found {
            if set.len() >= max_terms as usize || postings >= postings_budget {
                truncated = true;
                break 'segments;
            }
            postings += freq;
            set.insert(term);
        }
        truncated |= stopped;
    }
    let mut terms: Vec<String> = set.into_iter().collect();
    terms.sort_unstable();
    let terms = Arc::new(terms);
    TERM_CACHE
        .lock()
        .unwrap()
        .put(key, (terms.clone(), postings, truncated));
    Ok(PartTerms {
        terms,
        postings,
        truncated,
    })
}

/// Uncached fixed words share each dictionary walk; their budgets and cache entries stay separate.
const LINE_TERM_BATCH_PARTS: usize = 16;
const LINE_TERM_BATCH_LITERAL_BYTES: usize = 512;

fn line_literal_prefilter(literals: &[&str]) -> Option<Arc<TermDfa>> {
    if literals.is_empty()
        || literals.len() > LINE_TERM_BATCH_PARTS
        || literals.iter().map(|literal| literal.len()).sum::<usize>()
            > LINE_TERM_BATCH_LITERAL_BYTES
    {
        return None;
    }
    let alternatives: Vec<_> = literals
        .iter()
        .map(|literal| regex::escape(literal))
        .collect();
    term_automaton::compile_pattern(&format!("(?s:.*(?:{}).*)", alternatives.join("|"))).ok()
}

fn collect_line_terms(
    searcher: &Searcher,
    field: Field,
    parts: &[TermPart],
    literals: &[&str],
    max_terms: u32,
    postings_budget: u64,
    cancel: &AtomicBool,
) -> anyhow::Result<Vec<PartTerms>> {
    debug_assert_eq!(parts.len(), literals.len());
    if cancel.load(Ordering::Relaxed) {
        anyhow::bail!(CANCELLED);
    }
    let keys: Vec<_> = parts
        .iter()
        .map(|p| PartTermsKey::new(searcher, field, p, max_terms, postings_budget))
        .collect();
    let mut collected: Vec<Option<PartTerms>> = keys
        .iter()
        .map(|key| {
            TERM_CACHE
                .lock()
                .unwrap()
                .get(key)
                .map(|(terms, postings, truncated)| PartTerms {
                    terms: terms.clone(),
                    postings: *postings,
                    truncated: *truncated,
                })
        })
        .collect();
    let missing: Vec<_> = collected
        .iter()
        .enumerate()
        .filter_map(|(i, value)| value.is_none().then_some(i))
        .collect();
    if missing.len() == 1 {
        let i = missing[0];
        collected[i] = Some(collect_part_terms(
            searcher,
            field,
            &parts[i],
            max_terms,
            postings_budget,
            cancel,
        )?);
    } else {
        // Bound the simultaneous per-segment term buffers, as the cache bounds retained parts.
        for missing in missing.chunks(LINE_TERM_BATCH_PARTS) {
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!(CANCELLED);
            }
            // Literal containment is a cheap superset; the original automata classify each hit.
            let literals: Vec<_> = missing.iter().map(|&i| literals[i]).collect();
            let Some(dfa) = line_literal_prefilter(&literals) else {
                for &i in missing {
                    collected[i] = Some(collect_part_terms(
                        searcher,
                        field,
                        &parts[i],
                        max_terms,
                        postings_budget,
                        cancel,
                    )?);
                }
                continue;
            };
            use rayon::prelude::*;
            let segments = searcher
                .segment_readers()
                .par_iter()
                .map(|reader| -> anyhow::Result<_> {
                    let inverted = reader.inverted_index(field)?;
                    let mut found = vec![Vec::new(); missing.len()];
                    let mut stopped = vec![false; missing.len()];
                    let mut scanned = vec![0u64; missing.len()];
                    let mut postings = vec![0u64; missing.len()];
                    if cancel.load(Ordering::Relaxed) {
                        anyhow::bail!(CANCELLED);
                    }
                    let mut stream = inverted
                        .terms()
                        .search(QuoteTransparent::from_arc(dfa.clone()))
                        .into_stream()?;
                    let mut visited = 0usize;
                    while stream.advance() {
                        visited += 1;
                        if visited.is_multiple_of(4096) && cancel.load(Ordering::Relaxed) {
                            anyhow::bail!(CANCELLED);
                        }
                        let Ok(term) = std::str::from_utf8(stream.key()) else {
                            continue;
                        };
                        for local in 0..missing.len() {
                            let part = &parts[missing[local]];
                            if stopped[local] || !part.automaton.matches(term) {
                                continue;
                            }
                            scanned[local] += 1;
                            if scanned[local] > MAX_SCANNED_KEYS_PER_SEGMENT {
                                stopped[local] = true;
                                continue;
                            }
                            if part.verify.as_ref().is_some_and(|v| !verifies(v, term)) {
                                continue;
                            }
                            let freq = u64::from(stream.value().doc_freq);
                            found[local].push((term.to_string(), freq));
                            postings[local] += freq;
                            stopped[local] = found[local].len() >= max_terms as usize
                                || postings[local] >= postings_budget;
                        }
                        if stopped.iter().all(|&done| done) {
                            break;
                        }
                    }
                    Ok((found, stopped))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            if cancel.load(Ordering::Relaxed) {
                anyhow::bail!(CANCELLED);
            }
            for (local, &i) in missing.iter().enumerate() {
                let mut set = HashSet::new();
                let mut postings = 0;
                let mut truncated = false;
                'segments: for (found, stopped) in &segments {
                    for (term, freq) in &found[local] {
                        if set.len() >= max_terms as usize || postings >= postings_budget {
                            truncated = true;
                            break 'segments;
                        }
                        postings += freq;
                        set.insert(term.clone());
                    }
                    truncated |= stopped[local];
                }
                let mut terms: Vec<_> = set.into_iter().collect();
                terms.sort_unstable();
                let terms = Arc::new(terms);
                TERM_CACHE
                    .lock()
                    .unwrap()
                    .put(keys[i].clone(), (terms.clone(), postings, truncated));
                collected[i] = Some(PartTerms {
                    terms,
                    postings,
                    truncated,
                });
            }
        }
    }
    Ok(collected
        .into_iter()
        .map(|value| value.expect("all uncached parts collected"))
        .collect())
}

/// The error text of a cancelled search.
pub(crate) const CANCELLED: &str = "user regex search cancelled";

// ── Line patterns ──────────────────────────────────────────────────────────────

fn line_plan(whole: &Hir, elements: &[Hir]) -> Result<LinePlan, String> {
    if cost(whole) > REGEX_COST_LIMIT {
        return Err("too complex: bounded repetitions too large".to_string());
    }
    let pattern = whole.to_string();
    let (first_word, _) = edge_chars(whole, true);
    let (last_word, _) = edge_chars(whole, false);
    let source = format!(
        "{}(?:{pattern}){}",
        if first_word { r"\b{start-half}" } else { "" },
        if last_word { r"\b{end-half}" } else { "" },
    );
    let build = |source: &str| {
        RegexBuilder::new(source)
            .case_insensitive(true)
            .size_limit(LINE_REGEX_SIZE_LIMIT)
            .dfa_size_limit(LINE_REGEX_SIZE_LIMIT)
            .build()
            .map_err(|e| format!("too complex: {e}"))
    };
    Ok(LinePlan {
        regex: build(&source)?,
        prefilter: build(&without_word_looks(whole).to_string())?,
        clauses: required_clauses(elements),
    })
}

fn without_word_looks(hir: &Hir) -> Hir {
    match hir.kind() {
        HirKind::Look(look) if !matches!(look, Look::Start | Look::End) => Hir::empty(),
        HirKind::Repetition(rep) => Hir::repetition(Repetition {
            sub: Box::new(without_word_looks(&rep.sub)),
            ..rep.clone()
        }),
        HirKind::Capture(capture) => without_word_looks(&capture.sub),
        HirKind::Concat(subs) => Hir::concat(subs.iter().map(without_word_looks).collect()),
        HirKind::Alternation(subs) => {
            Hir::alternation(subs.iter().map(without_word_looks).collect())
        }
        _ => hir.clone(),
    }
}

/// Whether the first (or last) character of a match can be a word character, and whether
/// a match can be empty there. A boundary is implied only on an edge that can be a letter.
fn edge_chars(hir: &Hir, first: bool) -> (bool, bool) {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => (false, true),
        HirKind::Literal(_) => {
            let text = literal_text(hir).unwrap_or_default();
            let c = if first {
                text.chars().next()
            } else {
                text.chars().next_back()
            };
            (c.is_some_and(is_word_char), false)
        }
        HirKind::Class(Class::Unicode(class)) => {
            let mut word = class.clone();
            word.intersect(&WORD_CLASS);
            (!word.ranges().is_empty(), false)
        }
        HirKind::Class(Class::Bytes(_)) => (true, false),
        HirKind::Repetition(rep) => {
            let (word, empty) = edge_chars(&rep.sub, first);
            (word, empty || rep.min == 0)
        }
        HirKind::Capture(capture) => edge_chars(&capture.sub, first),
        HirKind::Alternation(subs) => subs.iter().fold((false, false), |acc, sub| {
            let (w, e) = edge_chars(sub, first);
            (acc.0 || w, acc.1 || e)
        }),
        HirKind::Concat(subs) => {
            let mut word = false;
            let ordered: Box<dyn Iterator<Item = &Hir>> = if first {
                Box::new(subs.iter())
            } else {
                Box::new(subs.iter().rev())
            };
            for sub in ordered {
                let (w, e) = edge_chars(sub, first);
                word |= w;
                if !e {
                    return (word, false);
                }
            }
            (word, true)
        }
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || is_word_mark(c)
}

/// The fixed words of a line pattern's top level, as AND-ed clauses: each term of a run of
/// literal characters, whole when the run lies between two characters that end a term (or
/// a pattern edge), and the OR of a top-level group whose every branch is a fixed word.
fn required_clauses(elements: &[Hir]) -> Vec<Vec<Fragment>> {
    fn flush(run: &mut String, left: bool, right: bool, clauses: &mut Vec<Vec<Fragment>>) {
        for fragment in fragments(run, left, right) {
            if !clauses.iter().any(|c| c.len() == 1 && c[0] == fragment) {
                clauses.push(vec![fragment]);
            }
        }
        run.clear();
    }
    let mut clauses: Vec<Vec<Fragment>> = Vec::new();
    let mut run = String::new();
    // A pattern edge bounds a run only where `line_plan` implies a word boundary.
    let edge = |c: Option<char>| c.is_some_and(is_word_char);
    let mut left_bounded = edge(
        elements
            .first()
            .and_then(literal_text)
            .and_then(|t| t.chars().next()),
    );
    for element in elements {
        let bounding = match literal_text(element) {
            Some(text) => {
                let c = text.chars().next().unwrap_or(' ');
                if crate::hebrew_tokenizer::continues_token(c) {
                    run.extend(c.to_lowercase());
                    continue;
                }
                true
            }
            None => is_separator(element),
        };
        flush(&mut run, left_bounded, bounding, &mut clauses);
        left_bounded = bounding;
        if !bounding {
            if let Some(any) = alternatives(element) {
                clauses.push(any);
            }
        }
    }
    let right_bounded = edge(run.chars().next_back());
    flush(&mut run, left_bounded, right_bounded, &mut clauses);
    clauses
}

/// The literal branches of a group required at least once: the longest term of each.
fn alternatives(hir: &Hir) -> Option<Vec<Fragment>> {
    match hir.kind() {
        HirKind::Repetition(rep) if rep.min >= 1 => alternatives(&rep.sub),
        HirKind::Capture(capture) => alternatives(&capture.sub),
        HirKind::Alternation(subs) => subs
            .iter()
            .map(|sub| {
                fragments(&literal_text(sub)?.to_lowercase(), false, false)
                    .into_iter()
                    .max_by_key(|f| f.text.chars().count())
            })
            .collect(),
        _ => None,
    }
}

/// The terms the index makes of `run`, without edge quotes; `left`/`right` say the run is
/// bounded there. The tokenizer folds `''` to `"`, and a run like `""` splits it.
fn fragments(run: &str, left: bool, right: bool) -> Vec<Fragment> {
    use tantivy::tokenizer::{TokenStream, Tokenizer};
    let mut tokenizer = crate::hebrew_tokenizer::HebrewTokenizer::default();
    let mut stream = tokenizer.token_stream(run);
    let mut tokens = Vec::new();
    while stream.advance() {
        tokens.push(stream.token().text.clone());
    }
    let last = tokens.len().saturating_sub(1);
    tokens
        .iter()
        .enumerate()
        .filter_map(|(i, token)| {
            let text = token.trim_matches(['"', '\'']);
            // A longer term is not indexed (`RemoveLongFilter`): no term could contain it.
            let indexed = text.len() < crate::api::search_engine::MAX_TOKEN_BYTES;
            (text.chars().count() >= 2 && indexed).then(|| Fragment {
                text: text.to_string(),
                whole: (left || i > 0) && (right || i < last),
            })
        })
        .collect()
}

/// What can glue a word to another in a term while the line pattern still sees a word
/// boundary between them: a quote, or a character the tokenizer keeps in a word but `\w`
/// does not match (a numeral like `²`).
static GLUE: Lazy<ClassUnicode> = Lazy::new(|| {
    let mut glue = ClassUnicode::new(
        ('\0'..=char::MAX)
            .filter(|&c| crate::hebrew_tokenizer::is_word_char(c))
            .map(|c| ClassUnicodeRange::new(c, c)),
    );
    glue.difference(&WORD_CLASS);
    glue.union(&ClassUnicode::new([
        ClassUnicodeRange::new('"', '"'),
        ClassUnicodeRange::new('\'', '\''),
    ]));
    glue
});

impl LinePlan {
    pub(crate) fn is_match(&self, view: &str) -> bool {
        self.prefilter.is_match(view) && self.regex.find_iter(view).any(|m| !m.is_empty())
    }

    /// Byte ranges of the non-empty matches in `view`.
    pub(crate) fn find(&self, view: &str) -> Vec<std::ops::Range<usize>> {
        self.regex
            .find_iter(view)
            .filter(|m| !m.is_empty())
            .map(|m| m.range())
            .collect()
    }
}

// ── The searched form of a line ────────────────────────────────────────────────

/// A line as a line pattern sees it — the app's `cleanLineForSearch`: markup removed (a
/// breaking tag and a whitespace entity become a space), nikud and cantillation removed,
/// maqaf, paseq and `|` as a space, whitespace collapsed and trimmed — plus the index's own
/// foldings: quotes as ASCII, presentation forms decomposed, invisible characters dropped,
/// lowercase. With a map, every byte of `text` knows the UTF-16 range of the source it came
/// from.
#[derive(Default)]
pub(crate) struct LineView {
    pub(crate) text: String,
    starts: Vec<u32>,
    ends: Vec<u32>,
}

impl LineView {
    pub(crate) fn of(source: &str, map: bool) -> Self {
        let mut view = LineView::default();
        view.fill(source, map);
        view
    }

    pub(crate) fn fill(&mut self, source: &str, map: bool) {
        self.text.clear();
        self.starts.clear();
        self.ends.clear();
        let bytes = source.as_bytes();
        let mut pending: Option<(u32, u32)> = None;
        let mut has_tag_end = true;
        let mut has_entity_end = true;
        let mut byte = 0usize;
        let mut utf16 = 0u32;
        let units = |text: &str| {
            if map {
                text.encode_utf16().count() as u32
            } else {
                0
            }
        };
        while byte < bytes.len() {
            let b = bytes[byte];
            if b == b'<' && has_tag_end {
                let rest = &source[byte..];
                if let Some(close) = rest.find('>') {
                    let end16 = utf16 + units(&rest[..=close]);
                    if tag_breaks_word(source, byte, byte + close) {
                        pending = Some(pending.map_or((utf16, end16), |(s, _)| (s, end16)));
                    }
                    byte += close + 1;
                    utf16 = end16;
                    continue;
                }
                has_tag_end = false;
            } else if b == b'&' && has_entity_end {
                let rest = &source[byte..];
                match rest[1..].find(';') {
                    Some(semi) if semi >= 1 => {
                        let end16 = utf16 + units(&rest[..semi + 2]);
                        if matches!(&rest[1..1 + semi], "nbsp" | "thinsp" | "ensp" | "emsp") {
                            pending = Some(pending.map_or((utf16, end16), |(s, _)| (s, end16)));
                        }
                        byte += semi + 2;
                        utf16 = end16;
                        continue;
                    }
                    Some(_) => {}
                    None => has_entity_end = false,
                }
            }
            if b < 0x80 {
                byte += 1;
                if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C | b'|') {
                    pending = Some(pending.map_or((utf16, utf16 + 1), |(s, _)| (s, utf16 + 1)));
                } else {
                    self.flush_space(&mut pending, map);
                    self.push(char::from(b.to_ascii_lowercase()), utf16, utf16 + 1, map);
                }
                utf16 += 1;
                continue;
            }
            let c = source[byte..].chars().next().expect("not at the end");
            byte += c.len_utf8();
            let next16 = utf16 + c.len_utf16() as u32;
            if ('\u{05D0}'..='\u{05EA}').contains(&c) {
                self.flush_space(&mut pending, map);
                self.push(c, utf16, next16, map);
            } else if is_dropped_mark(c) {
                // A mark is painted with the letter it sits on.
                if map && pending.is_none() {
                    if let Some(last) = self.text.chars().next_back() {
                        let n = self.ends.len();
                        self.ends[n - last.len_utf8()..].fill(next16);
                    }
                }
            } else if c.is_whitespace() || matches!(c, '\u{05BE}' | '\u{05C0}') {
                pending = Some(pending.map_or((utf16, next16), |(s, _)| (s, next16)));
            } else if !is_invisible_char(c) {
                self.flush_space(&mut pending, map);
                match c {
                    '\u{05F3}' | '\u{2018}' | '\u{2019}' => self.push('\'', utf16, next16, map),
                    '\u{05F4}' | '\u{201C}' | '\u{201D}' => self.push('"', utf16, next16, map),
                    _ => match fold_presentation_form(c) {
                        Some(folded) => {
                            for f in folded.chars().filter(|f| !is_word_mark(*f)) {
                                self.push(f, utf16, next16, map);
                            }
                        }
                        None if c.is_uppercase() => {
                            for l in c.to_lowercase() {
                                self.push(l, utf16, next16, map);
                            }
                        }
                        None => self.push(c, utf16, next16, map),
                    },
                }
            }
            utf16 = next16;
        }
    }

    /// Emits a pending whitespace run as one space, unless the view is still empty.
    fn flush_space(&mut self, pending: &mut Option<(u32, u32)>, map: bool) {
        if let Some((s, e)) = pending.take() {
            if !self.text.is_empty() {
                self.push(' ', s, e, map);
            }
        }
    }

    fn push(&mut self, c: char, start: u32, end: u32, map: bool) {
        self.text.push(c);
        if map {
            self.starts.extend(std::iter::repeat_n(start, c.len_utf8()));
            self.ends.extend(std::iter::repeat_n(end, c.len_utf8()));
        }
    }

    /// The UTF-16 source range of the view's byte range (non-empty).
    pub(crate) fn source_range(&self, range: &std::ops::Range<usize>) -> (u32, u32) {
        (self.starts[range.start], self.ends[range.end - 1])
    }
}

// ── Display matching (the book view) ───────────────────────────────────────────

/// Finds a pattern's matches in displayed text (markup included), in UTF-16 units.
pub(crate) enum DisplayMatcher {
    Terms(PreparedHighlightMatcher),
    Line(Regex),
}

impl DisplayMatcher {
    pub(crate) fn new(regex: &UserRegex) -> Option<Self> {
        match regex.kind {
            RegexKind::Line => Some(DisplayMatcher::Line(regex.line.as_ref()?.regex.clone())),
            _ => {
                let branches = regex
                    .parts
                    .iter()
                    .map(|p| vec![(String::new(), p.pattern.clone(), String::new())])
                    .collect();
                let n = regex.parts.len();
                PreparedHighlightMatcher::new(branches, Proximity::adjacent(n), vec![true; n])
                    .ok()
                    .map(DisplayMatcher::Terms)
            }
        }
    }

    pub(crate) fn find(&self, data: &str) -> Vec<Match> {
        match self {
            DisplayMatcher::Terms(matcher) => matcher.find_matches(data, &[]),
            DisplayMatcher::Line(regex) => {
                let view = LineView::of(data, true);
                regex
                    .find_iter(&view.text)
                    .filter(|m| !m.is_empty())
                    .map(|m| {
                        let (start, end) = view.source_range(&m.range());
                        let (start, end) = (start as usize, end as usize);
                        Match {
                            start,
                            end,
                            word_ranges: vec![Range {
                                start: 0,
                                end: end - start,
                            }],
                        }
                    })
                    .collect()
            }
        }
    }
}

// ── Line search ────────────────────────────────────────────────────────────────

/// Everything a line search reads from, on one searcher.
pub(crate) struct LineSearch<'a> {
    pub(crate) searcher: &'a Searcher,
    pub(crate) schema: &'a Schema,
    /// The facet filter (`None`: the whole index).
    pub(crate) scope: Option<Box<dyn Query>>,
    /// The filter's facets, for the result cache.
    pub(crate) facets: Vec<String>,
    pub(crate) cancel: &'a AtomicBool,
    /// Line hits kept; more end the search with `truncated` ([`MAX_LINE_HITS`]).
    pub(crate) max_hits: usize,
    /// The catalogue order hits are reported and kept in, and its epoch for the cache.
    pub(crate) order: Arc<OrderTables>,
    pub(crate) order_epoch: u64,
}

impl LineSearch<'_> {
    fn catalogue_key(&self, id: u64) -> u64 {
        self.order.key(OrderKind::Catalogue, id)
    }
}

/// What a page of the stream shows.
pub(crate) struct Page {
    pub(crate) offset: u32,
    pub(crate) limit: u32,
    pub(crate) chunk_size: u32,
    pub(crate) highlight_prefix: String,
    pub(crate) highlight_postfix: String,
    pub(crate) max_chars: u32,
}

#[derive(Clone, Copy)]
struct LineHit {
    id: u64,
    address: DocAddress,
}

/// A finished line search: its hits in catalogue order, and per book.
pub(crate) struct LineOutcome {
    hits: Vec<LineHit>,
    pub(crate) book_counts: HashMap<String, u32>,
    pub(crate) truncated: bool,
}

impl LineOutcome {
    pub(crate) fn count(&self) -> u32 {
        self.hits.len() as u32
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct LineCacheKey {
    index: u64,
    order: u64,
    sources: Vec<(Source, line_source::SourceGeneration)>,
    facets: Vec<String>,
    pattern: String,
    max_hits: usize,
}

impl LineCacheKey {
    /// The same search, whatever its scope.
    fn same_search(&self, other: &Self) -> bool {
        self.index == other.index
            && self.order == other.order
            && self.sources == other.sources
            && self.pattern == other.pattern
            && self.max_hits == other.max_hits
    }
}

pub(crate) fn release_memory() {
    TERM_CACHE.lock().unwrap().clear();
    LINE_SCANS.lock().unwrap().clear();
}

/// A line search as far as it got. A later call of the same search (another page, a count,
/// or after a cancel) resumes it, and a concurrent one waits for it instead of reading the
/// scope again.
struct LineScan {
    /// The facet filter (`None`: the whole index).
    scope: Option<Box<dyn Query>>,
    state: Progress,
    /// What the search reads, in catalogue order; set by the first call.
    work: Option<LineWork>,
    outcome: Option<Arc<LineOutcome>>,
}

enum LineWork {
    /// Few candidate lines, read one by one.
    Verify(Vec<(u64, DocAddress)>),
    Scan(Vec<ScopeBook>),
}

/// Searches kept. Tests run in parallel on many indexes and must not evict each other's.
const LINE_SCANS_KEPT: usize = if cfg!(test) { 256 } else { 8 };

static LINE_SCANS: Lazy<Mutex<lru::LruCache<LineCacheKey, Arc<ScanSlot>>>> = Lazy::new(|| {
    Mutex::new(lru::LruCache::new(
        std::num::NonZeroUsize::new(LINE_SCANS_KEPT).unwrap(),
    ))
});

/// A book of the scope: how it is stored, and when only some of its lines can match, those
/// (`(id, address)` of each candidate).
struct ScopeBook {
    file_path: String,
    min_id: u64,
    docs: u64,
    library: bool,
    candidates: Option<Vec<(u64, DocAddress)>>,
}

impl ScopeBook {
    /// Lines the scan reads.
    fn lines(&self) -> u64 {
        self.candidates
            .as_ref()
            .map_or(self.docs, |c| c.len() as u64)
    }
}

/// Runs a line pattern over the scope, reporting through `emit` (which returns `false` once
/// nobody listens). A search is kept per index generation, filter and source generation
/// ([`LineScan`]), so the pages, counts and facets that follow a stream cost nothing, and a
/// complete search of a wider scope answers a narrower one.
pub(crate) fn run_line_search(
    ctx: &LineSearch<'_>,
    regex: &UserRegex,
    page: Option<&Page>,
    emit: &mut dyn FnMut(UserRegexStreamUpdate) -> bool,
) -> anyhow::Result<Option<Arc<LineOutcome>>> {
    let plan = regex
        .line
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("not a line pattern"))?;
    if ctx.cancel.load(Ordering::Relaxed) {
        emit(final_update(0, &HashMap::new(), false, true, 0, 0));
        return Ok(None);
    }
    let mut facets = ctx.facets.clone();
    facets.sort();
    let key = LineCacheKey {
        index: index_identity(ctx.searcher),
        order: ctx.order_epoch,
        sources: Source::all()
            .into_iter()
            .map(|source| (source, line_source::scan_cache_generation(source)))
            .collect(),
        facets,
        pattern: regex.pattern.clone(),
        max_hits: ctx.max_hits,
    };
    let scan = LINE_SCANS
        .lock()
        .unwrap()
        .get_or_insert(key.clone(), || {
            Arc::new(ScanSlot::new(LineScan {
                scope: ctx.scope.as_ref().map(|scope| scope.box_clone()),
                state: Progress::new(ctx.max_hits),
                work: None,
                outcome: None,
            }))
        })
        .clone();
    let Some(mut scan) = lock_unless_cancelled(&scan, ctx.cancel) else {
        emit(final_update(0, &HashMap::new(), false, true, 0, 0));
        return Ok(None);
    };
    if scan.outcome.is_none() {
        if let Some(outcome) = narrowed_outcome(ctx, &key)? {
            scan.work = None;
            scan.state = Progress::new(ctx.max_hits);
            scan.outcome = Some(outcome);
        }
    }
    if let Some(outcome) = scan.outcome.clone() {
        if let Some(page) = page {
            let mut reporter = Reporter::new(ctx, plan, page, emit);
            reporter.offer(&outcome.hits)?;
        }
        let cancelled = ctx.cancel.load(Ordering::Relaxed);
        emit(final_update(
            outcome.count(),
            &outcome.book_counts,
            outcome.truncated,
            cancelled,
            0,
            0,
        ));
        return Ok((!cancelled).then_some(outcome));
    }

    let finished = {
        let mut reporter = page.map(|page| Reporter::new(ctx, plan, page, &mut *emit));
        if let Some(reporter) = reporter.as_mut() {
            // What the calls this one resumes found.
            reporter.offer(&scan.state.hits)?;
        }
        search_lines(ctx, plan, &mut scan, &mut reporter)?
    };
    let state = &mut scan.state;
    if !finished {
        emit(final_update(
            state.hits.len() as u32,
            &state.book_counts,
            state.truncated,
            true,
            state.done,
            state.total,
        ));
        return Ok(None);
    }
    let (done, total) = (state.done, state.total);
    let mut hits = std::mem::take(&mut state.hits);
    hits.sort_unstable_by_key(|h| ctx.catalogue_key(h.id));
    let outcome = Arc::new(LineOutcome {
        hits,
        book_counts: std::mem::take(&mut state.book_counts),
        truncated: state.truncated,
    });
    scan.work = None;
    if scan.state.unavailable {
        scan.state = Progress::new(scan.state.max_hits);
    } else {
        scan.outcome = Some(outcome.clone());
    }
    let cancelled = ctx.cancel.load(Ordering::Relaxed);
    emit(final_update(
        outcome.count(),
        &outcome.book_counts,
        outcome.truncated,
        cancelled,
        done,
        total,
    ));
    Ok((!cancelled).then_some(outcome))
}

/// One search's scan, run by one call at a time; the others wait on `freed`.
struct ScanSlot {
    scan: Mutex<LineScan>,
    busy: Mutex<bool>,
    freed: std::sync::Condvar,
}

impl ScanSlot {
    fn new(scan: LineScan) -> Self {
        Self {
            scan: Mutex::new(scan),
            busy: Mutex::new(false),
            freed: std::sync::Condvar::new(),
        }
    }
}

/// The scan of a [`ScanSlot`], held by one call; dropping it wakes the next.
struct ScanGuard<'a> {
    slot: &'a ScanSlot,
    scan: Option<std::sync::MutexGuard<'a, LineScan>>,
}

impl std::ops::Deref for ScanGuard<'_> {
    type Target = LineScan;
    fn deref(&self) -> &LineScan {
        self.scan.as_ref().expect("held until dropped")
    }
}

impl std::ops::DerefMut for ScanGuard<'_> {
    fn deref_mut(&mut self) -> &mut LineScan {
        self.scan.as_mut().expect("held until dropped")
    }
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        self.scan = None;
        *self.slot.busy.lock().unwrap_or_else(|p| p.into_inner()) = false;
        self.slot.freed.notify_one();
    }
}

/// How often a waiting call looks at its cancellation token.
const CANCEL_CHECK: std::time::Duration = std::time::Duration::from_millis(50);

#[cfg(test)]
thread_local! {
    /// Times this thread woke while waiting for another call's scan.
    pub(crate) static WAKEUPS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Waits for another call's scan of the same search, unless this call is cancelled first.
fn lock_unless_cancelled<'a>(slot: &'a ScanSlot, cancel: &AtomicBool) -> Option<ScanGuard<'a>> {
    let mut busy = slot.busy.lock().unwrap_or_else(|p| p.into_inner());
    while *busy {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        #[cfg(test)]
        WAKEUPS.with(|w| w.set(w.get() + 1));
        busy = slot
            .freed
            .wait_timeout(busy, CANCEL_CHECK)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
    *busy = true;
    drop(busy);
    let scan = slot.scan.lock().unwrap_or_else(|poisoned| {
        // A scan that panicked may have stopped between its updates: start over,
        // once — a mutex left poisoned would drop every later call's answer too.
        slot.scan.clear_poison();
        let mut scan = poisoned.into_inner();
        scan.state = Progress::new(scan.state.max_hits);
        scan.work = None;
        scan.outcome = None;
        scan
    });
    Some(ScanGuard {
        slot,
        scan: Some(scan),
    })
}

/// A complete search of a wider scope answers a narrower one without reading a line.
fn narrowed_outcome(
    ctx: &LineSearch<'_>,
    key: &LineCacheKey,
) -> anyhow::Result<Option<Arc<LineOutcome>>> {
    let Some(narrow) = &ctx.scope else {
        return Ok(None);
    };
    let others: Vec<Arc<ScanSlot>> = LINE_SCANS
        .lock()
        .unwrap()
        .iter()
        .filter(|(other, _)| other.facets != key.facets && other.same_search(key))
        .map(|(_, scan)| scan.clone())
        .collect();
    for other in others {
        // A scan still running is not waited for.
        let Ok(other) = other.scan.try_lock() else {
            continue;
        };
        let Some(outcome) = other.outcome.as_ref().filter(|o| !o.truncated) else {
            continue;
        };
        if let Some(wide) = &other.scope {
            let outside = BooleanQuery::new(vec![
                (Occur::Must, narrow.box_clone()),
                (Occur::MustNot, wide.box_clone()),
            ]);
            if ctx.searcher.search(&outside, &Count)? > 0 {
                continue;
            }
        }
        let within = BooleanQuery::new(vec![
            (
                Occur::Must,
                Box::new(HitSetQuery::new(ctx.searcher, outcome)) as Box<dyn Query>,
            ),
            (Occur::Must, narrow.box_clone()),
        ]);
        let keep = ScopeBooksCollector {
            keep_documents: true,
        };
        let mut hits = Vec::new();
        let mut book_counts = HashMap::new();
        for book in ctx.searcher.search(&within, &keep)? {
            let documents = book.candidates.unwrap_or_default();
            book_counts.insert(book.file_path, documents.len() as u32);
            hits.extend(
                documents
                    .into_iter()
                    .map(|(id, address)| LineHit { id, address }),
            );
        }
        hits.sort_unstable_by_key(|h| ctx.catalogue_key(h.id));
        return Ok(Some(Arc::new(LineOutcome {
            hits,
            book_counts,
            truncated: false,
        })));
    }
    Ok(None)
}

fn final_update(
    count: u32,
    book_counts: &HashMap<String, u32>,
    truncated: bool,
    cancelled: bool,
    done: u32,
    total: u32,
) -> UserRegexStreamUpdate {
    UserRegexStreamUpdate {
        kind: UserRegexKind::CrossSpace,
        results: Vec::new(),
        match_ranges: Vec::new(),
        total_count: Some(count),
        book_counts: Some(book_counts.clone()),
        found_so_far: count,
        progress_done: done,
        progress_total: total,
        truncated,
        done: true,
        cancelled,
    }
}

struct Progress {
    max_hits: usize,
    hits: Vec<LineHit>,
    book_counts: HashMap<String, u32>,
    truncated: bool,
    /// Some lines could not be read: the answer is not kept for the next call.
    unavailable: bool,
    /// Candidate lines or books read; a resumed search continues from here.
    done: u32,
    total: u32,
}

impl Progress {
    fn new(max_hits: usize) -> Self {
        Progress {
            max_hits,
            hits: Vec::new(),
            book_counts: HashMap::new(),
            truncated: false,
            unavailable: false,
            done: 0,
            total: 0,
        }
    }

    /// Adds the hits of finished work (in catalogue order); `false` once the cap is reached.
    fn add(&mut self, ctx: &LineSearch<'_>, mut hits: Vec<(String, LineHit)>) -> bool {
        hits.sort_unstable_by_key(|(_, h)| ctx.catalogue_key(h.id));
        for (book, hit) in hits {
            if self.hits.len() >= self.max_hits {
                self.truncated = true;
                return false;
            }
            *self.book_counts.entry(book).or_insert(0) += 1;
            self.hits.push(hit);
        }
        true
    }
}

/// Sends the hits that fall on the requested page, hydrated, as they are found.
struct Reporter<'a, 'e> {
    ctx: &'a LineSearch<'a>,
    plan: &'a LinePlan,
    page: &'a Page,
    emit: &'e mut dyn FnMut(UserRegexStreamUpdate) -> bool,
    seen: usize,
    listening: bool,
}

impl<'a, 'e> Reporter<'a, 'e> {
    fn new(
        ctx: &'a LineSearch<'a>,
        plan: &'a LinePlan,
        page: &'a Page,
        emit: &'e mut dyn FnMut(UserRegexStreamUpdate) -> bool,
    ) -> Self {
        Reporter {
            ctx,
            plan,
            page,
            emit,
            seen: 0,
            listening: true,
        }
    }

    /// `hits` follow every hit offered before, in catalogue order.
    fn offer(&mut self, hits: &[LineHit]) -> anyhow::Result<()> {
        let start = self.page.offset as usize;
        let end = start.saturating_add(self.page.limit as usize);
        let from = self.seen.max(start);
        let to = (self.seen + hits.len()).min(end);
        let wanted = if from < to {
            &hits[from - self.seen..to - self.seen]
        } else {
            &[][..]
        };
        self.seen += hits.len();
        for chunk in wanted.chunks(self.page.chunk_size.max(1) as usize) {
            if !self.listening || self.ctx.cancel.load(Ordering::Relaxed) {
                break;
            }
            let hydrated = hydrate(self.ctx, self.plan, chunk, self.page)?;
            if self.ctx.cancel.load(Ordering::Relaxed) {
                break;
            }
            let (results, match_ranges) = hydrated
                .into_iter()
                .map(|(result, ranges)| (result, UserRegexLineMatches { ranges }))
                .unzip();
            self.listening = (self.emit)(UserRegexStreamUpdate {
                kind: UserRegexKind::CrossSpace,
                results,
                match_ranges,
                total_count: None,
                book_counts: None,
                found_so_far: self.seen as u32,
                progress_done: 0,
                progress_total: 0,
                truncated: false,
                done: false,
                cancelled: false,
            });
        }
        Ok(())
    }

    fn progress(&mut self, done: u32, total: u32) {
        if self.listening && !self.ctx.cancel.load(Ordering::Relaxed) {
            self.listening = (self.emit)(UserRegexStreamUpdate {
                kind: UserRegexKind::CrossSpace,
                results: Vec::new(),
                match_ranges: Vec::new(),
                total_count: None,
                book_counts: None,
                found_so_far: self.seen as u32,
                progress_done: done,
                progress_total: total,
                truncated: false,
                done: false,
                cancelled: false,
            });
        }
    }
}

/// `Ok(false)` when cancelled; what was read stays in `scan` for the next call.
fn search_lines(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
    scan: &mut LineScan,
    reporter: &mut Option<Reporter<'_, '_>>,
) -> anyhow::Result<bool> {
    if scan.work.is_none() {
        let started = std::time::Instant::now();
        let candidates = candidate_query(ctx, plan)?;
        if ctx.cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        scan.work = Some(match &candidates {
            Some(query) => {
                let count = ctx.searcher.search(query.as_ref(), &Count)? as u32;
                log::info!(
                    "user regex: {count} candidate lines in {:?}",
                    started.elapsed()
                );
                if count <= CANDIDATE_FETCH_LIMIT {
                    let mut docs = ctx.searcher.search(query.as_ref(), &IdAddressCollector)?;
                    docs.sort_unstable_by_key(|(id, _)| ctx.catalogue_key(*id));
                    LineWork::Verify(docs)
                } else {
                    // Too many to read one by one: read their books, but decode only their rows.
                    let keep = ScopeBooksCollector {
                        keep_documents: true,
                    };
                    LineWork::Scan(ctx.searcher.search(query.as_ref(), &keep)?)
                }
            }
            None => {
                let all = ScopeBooksCollector {
                    keep_documents: false,
                };
                LineWork::Scan(match &ctx.scope {
                    Some(scope) => ctx.searcher.search(scope.as_ref(), &all)?,
                    None => ctx.searcher.search(&tantivy::query::AllQuery, &all)?,
                })
            }
        });
        if let Some(LineWork::Scan(books)) = scan.work.as_mut() {
            books.sort_unstable_by_key(|b| ctx.order.catalogue_rank(book_number_of(b.min_id)));
        }
    }
    let LineScan { state, work, .. } = scan;
    let books = match work.as_ref().expect("set above") {
        LineWork::Verify(docs) => return verify_candidates(ctx, plan, docs, state, reporter),
        LineWork::Scan(books) => books,
    };
    let lines: u64 = books.iter().map(ScopeBook::lines).sum();
    let scan_started = std::time::Instant::now();
    FETCH_NANOS.store(0, Ordering::Relaxed);
    let finished = scan_books(ctx, plan, books, state, reporter)?;
    log::info!(
        "user regex: read {} books ({lines} lines) in {:?}; readers spent {:?} in the database",
        books.len(),
        scan_started.elapsed(),
        std::time::Duration::from_nanos(FETCH_NANOS.load(Ordering::Relaxed)),
    );
    Ok(finished)
}

/// Time the reading threads spent in the database, summed, for the scan's log line.
static FETCH_NANOS: AtomicU64 = AtomicU64::new(0);

/// The lines that hold every selective fixed word, in scope; `None` when no fixed word
/// narrows the search.
fn candidate_query(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
) -> anyhow::Result<Option<Box<dyn Query>>> {
    let text_field = ctx.schema.get_field("text")?;
    let parts = plan
        .clauses
        .iter()
        .flatten()
        .map(|fragment| {
            // A word glued by a character the index drops (`יב[ע]ר` is `יבער`) is not found.
            if fragment.whole {
                glued_part(&fragment.text)
            } else {
                contains_part(&fragment.text)
            }
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if ctx.cancel.load(Ordering::Relaxed) {
        anyhow::bail!(CANCELLED);
    }
    let mut first = 0;
    for clause in &plan.clauses {
        let end = first + clause.len();
        let keys: Vec<_> = parts[first..end]
            .iter()
            .map(|part| {
                PartTermsKey::new(
                    ctx.searcher,
                    text_field,
                    part,
                    CLAUSE_MAX_TERMS,
                    CLAUSE_POSTINGS_CAP,
                )
            })
            .collect();
        let known_empty = {
            let cache = TERM_CACHE.lock().unwrap();
            keys.iter().all(|key| {
                cache
                    .peek(key)
                    .is_some_and(|(terms, _, truncated)| terms.is_empty() && !truncated)
            })
        };
        if known_empty {
            return Ok(Some(Box::new(tantivy::query::EmptyQuery)));
        }
        first = end;
    }
    let mut found = collect_line_terms(
        ctx.searcher,
        text_field,
        &parts,
        &plan
            .clauses
            .iter()
            .flatten()
            .map(|fragment| fragment.text.as_str())
            .collect::<Vec<_>>(),
        CLAUSE_MAX_TERMS,
        CLAUSE_POSTINGS_CAP,
        ctx.cancel,
    )?
    .into_iter();
    let mut clauses: Vec<(u64, Box<dyn Query>)> = Vec::new();
    for clause in &plan.clauses {
        let mut terms: HashSet<String> = HashSet::new();
        let mut postings = 0u64;
        let mut selective = true;
        for _ in clause {
            let found = found.next().expect("one term collection per fragment");
            if found.truncated {
                selective = false;
                continue;
            }
            postings += found.postings;
            terms.extend(found.terms.iter().cloned());
            if postings > CLAUSE_POSTINGS_CAP {
                selective = false;
            }
        }
        if !selective {
            continue;
        }
        if terms.is_empty() {
            return Ok(Some(Box::new(tantivy::query::EmptyQuery)));
        }
        let query: Box<dyn Query> = Box::new(TermSetQuery::new(
            terms.iter().map(|t| Term::from_field_text(text_field, t)),
        ));
        clauses.push((postings, query));
    }
    if clauses.is_empty() {
        return Ok(None);
    }
    clauses.sort_by_key(|(postings, _)| *postings);
    let mut must: Vec<(Occur, Box<dyn Query>)> =
        clauses.into_iter().map(|(_, q)| (Occur::Must, q)).collect();
    if let Some(scope) = &ctx.scope {
        must.push((Occur::Must, scope.box_clone()));
    }
    Ok(Some(Box::new(BooleanQuery::new(must))))
}

fn any_text() -> Hir {
    Hir::repetition(Repetition {
        min: 0,
        max: None,
        greedy: true,
        sub: Box::new(Hir::class(Class::Unicode(ClassUnicode::new([
            ClassUnicodeRange::new('\0', char::MAX),
        ])))),
    })
}

/// `.*fragment.*` as a term part.
fn contains_part(fragment: &str) -> anyhow::Result<TermPart> {
    literal_part(Hir::concat(vec![
        any_text(),
        Hir::literal(fragment.as_bytes().to_vec()),
        any_text(),
    ]))
}

/// `(?:.*[glue])?fragment(?:[glue].*)?` as a term part: the term `fragment`, or one that
/// [`GLUE`] joins it to.
fn glued_part(fragment: &str) -> anyhow::Result<TermPart> {
    let glue = || Hir::class(Class::Unicode(GLUE.clone()));
    let optional = |subs| {
        Hir::repetition(Repetition {
            min: 0,
            max: Some(1),
            greedy: true,
            sub: Box::new(Hir::concat(subs)),
        })
    };
    literal_part(Hir::concat(vec![
        optional(vec![any_text(), glue()]),
        Hir::literal(fragment.as_bytes().to_vec()),
        optional(vec![glue(), any_text()]),
    ]))
}

fn literal_part(hir: Hir) -> anyhow::Result<TermPart> {
    let automaton = compile_user_term_automaton(&hir).map_err(|e| anyhow::anyhow!(e))?;
    Ok(TermPart {
        pattern: hir.to_string(),
        relaxed: hir.to_string(),
        automaton: Arc::new(automaton),
        verify: None,
    })
}

fn verify_candidates(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
    docs: &[(u64, DocAddress)],
    state: &mut Progress,
    reporter: &mut Option<Reporter<'_, '_>>,
) -> anyhow::Result<bool> {
    state.total = docs.len() as u32;
    let file_path_f = ctx.schema.get_field("filePath")?;
    let mut view = LineView::default();
    let mut loader = DocumentLoader::new(ctx.searcher);
    for batch in docs[state.done as usize..].chunks(VERIFY_BATCH) {
        if ctx.cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let documents = batch
            .iter()
            .map(|&(_, address)| Ok((address, loader.load(address)?)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let texts = indexed_texts(ctx.searcher, &documents)?;
        let mut hits = Vec::new();
        for ((&(id, address), (_, document)), text) in batch.iter().zip(&documents).zip(texts) {
            if text.status == TextStatus::Unavailable {
                state.truncated = true;
                state.unavailable = true;
                continue;
            }
            view.fill(&text.text, false);
            if plan.is_match(&view.text) {
                let book = str_field(document, file_path_f);
                hits.push((book, LineHit { id, address }));
            }
        }
        let before = state.hits.len();
        let keep_going = state.add(ctx, hits);
        state.done += batch.len() as u32;
        if let Some(reporter) = reporter.as_mut() {
            reporter.offer(&state.hits[before..])?;
            reporter.progress(state.done, state.total);
        }
        if !keep_going {
            break;
        }
    }
    Ok(true)
}

fn scan_books(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
    books: &[ScopeBook],
    state: &mut Progress,
    reporter: &mut Option<Reporter<'_, '_>>,
) -> anyhow::Result<bool> {
    state.total = books.len() as u32;
    let mut start = state.done as usize;
    while start < books.len() {
        if ctx.cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let mut end = start;
        let mut lines = 0u64;
        while end < books.len() && end - start < SCAN_BATCH_BOOKS && lines < SCAN_BATCH_LINES {
            lines += books[end].lines();
            end += 1;
        }
        let scanned = scan_batch(ctx, plan, &books[start..end])?;
        if ctx.cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        let mut hits = Vec::new();
        for (book, scan) in books[start..end].iter().zip(scanned) {
            state.truncated |= scan.unavailable;
            state.unavailable |= scan.unavailable;
            hits.extend(
                scan.hits
                    .into_iter()
                    .map(|hit| (book.file_path.clone(), hit)),
            );
        }
        let before = state.hits.len();
        let keep_going = state.add(ctx, hits);
        state.done = end as u32;
        if let Some(reporter) = reporter.as_mut() {
            reporter.offer(&state.hits[before..])?;
            reporter.progress(state.done, state.total);
        }
        if !keep_going {
            break;
        }
        start = end;
    }
    Ok(true)
}

struct BookScan {
    hits: Vec<LineHit>,
    unavailable: bool,
}

impl BookScan {
    fn empty(unavailable: bool) -> Self {
        BookScan {
            hits: Vec::new(),
            unavailable,
        }
    }
}

const FETCH_AHEAD_BYTES: usize = 32 << 20;

struct ReadAheadBudget {
    used: Mutex<usize>,
    changed: std::sync::Condvar,
    limit: usize,
}

impl Default for ReadAheadBudget {
    fn default() -> Self {
        Self::new(FETCH_AHEAD_BYTES)
    }
}

impl ReadAheadBudget {
    fn new(limit: usize) -> Self {
        Self {
            used: Mutex::new(0),
            changed: std::sync::Condvar::new(),
            limit,
        }
    }

    fn admit(&self, bytes: usize, cancel: &AtomicBool) -> Option<ReadAheadPermit<'_>> {
        let mut used = self.used.lock().unwrap();
        while *used > 0 && used.saturating_add(bytes) > self.limit {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            used = self
                .changed
                .wait_timeout(used, std::time::Duration::from_millis(10))
                .unwrap()
                .0;
        }
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        *used += bytes;
        Some(ReadAheadPermit {
            budget: self,
            bytes,
        })
    }
}

struct ReadAheadPermit<'a> {
    budget: &'a ReadAheadBudget,
    bytes: usize,
}

impl Drop for ReadAheadPermit<'_> {
    fn drop(&mut self) {
        *self.budget.used.lock().unwrap() -= self.bytes;
        self.budget.changed.notify_all();
    }
}

/// What a reading thread hands a book's scan.
enum Fetched {
    /// The index stores the book's text.
    Stored,
    /// The rows to match, undecoded, by ordinal; `Err` when the database could not be read.
    Library(Result<Option<BookRows>, ()>),
}

impl Fetched {
    fn bytes(&self) -> usize {
        match self {
            Self::Library(Ok(Some((rows, _)))) => {
                rows.capacity() * std::mem::size_of::<(u64, RawRow)>()
                    + rows
                        .iter()
                        .map(|(_, row)| match row {
                            RawRow::Text(bytes) | RawRow::Frame(bytes) => bytes.capacity(),
                        })
                        .sum::<usize>()
            }
            _ => 0,
        }
    }
}

/// Rows of a book by ordinal, with the decoder of their frames.
type BookRows = (Vec<(u64, RawRow)>, Option<FrameDecoder>);

/// A candidate's ordinal is its id's low half, less one (see `add_text_book`).
fn ordinal_of(id: u64) -> Option<u64> {
    (id & 0xFFFF_FFFF).checked_sub(1)
}

/// The rows of `book` a scan matches: its candidates' rows, or all of them.
fn fetch_rows(
    store: &mut line_source::LineStore,
    book: &ScopeBook,
    book_id: i64,
    stop: &dyn Fn() -> bool,
    known_row_count: Option<u64>,
) -> anyhow::Result<Option<BookRows>> {
    match &book.candidates {
        Some(candidates) => {
            let mut ordinals: Vec<u64> = candidates
                .iter()
                .filter_map(|&(id, _)| ordinal_of(id))
                .collect();
            ordinals.sort_unstable();
            ordinals.dedup();
            // A row read alone is a random read (~0.3 ms on a cold file); reading the whole
            // book runs sequentially (~1.5 µs a row). Only sparse candidates read alone.
            let book_rows = match known_row_count {
                Some(rows) => rows,
                None => store.book_row_count(book_id)?,
            };
            if (ordinals.len() as u64).saturating_mul(SPARSE_ROW_FACTOR) >= book_rows {
                let wanted: HashSet<u64> = ordinals.into_iter().collect();
                return Ok(store
                    .fetch_book_raw_checked(book_id, stop)?
                    .map(|(rows, decoder)| {
                        let rows = (0u64..)
                            .zip(rows)
                            .filter(|(ordinal, _)| wanted.contains(ordinal))
                            .collect();
                        (rows, decoder)
                    }));
            }
            let (rows, decoder) = store.fetch_rows_raw_checked(book_id, &ordinals, stop)?;
            let rows = ordinals
                .into_iter()
                .zip(rows)
                .filter_map(|(ordinal, row)| row.map(|row| (ordinal, row)))
                .collect();
            Ok(Some((rows, decoder)))
        }
        None => Ok(store
            .fetch_book_raw_checked(book_id, stop)?
            .map(|(rows, decoder)| ((0u64..).zip(rows).collect(), decoder))),
    }
}

/// The books of one batch, in order. [`SCAN_READERS`] threads read the library rows, each on a
/// connection of a [`line_source::ScanSession`], while the pool decodes and matches the books
/// already read.
fn scan_batch(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
    books: &[ScopeBook],
) -> anyhow::Result<Vec<BookScan>> {
    use rayon::prelude::*;
    let source_of = |book: &ScopeBook| line_source_book(&book.file_path).filter(|_| book.library);
    let sessions: Vec<(Source, line_source::ScanSession)> = books
        .iter()
        .filter_map(|book| source_of(book).map(|(source, _)| source))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|source| Some((source, line_source::begin_scan(source).ok()?)))
        .collect();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let budget = ReadAheadBudget::default();
    let large_books = ReadAheadBudget::new(1);
    let (sender, receiver) = std::sync::mpsc::sync_channel(FETCH_AHEAD);
    std::thread::scope(|scope| {
        for _ in 0..SCAN_READERS {
            let sender = sender.clone();
            let (sessions, next, budget, large_books) = (&sessions, &next, &budget, &large_books);
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= books.len() || ctx.cancel.load(Ordering::Relaxed) {
                    break;
                }
                let book = &books[i];
                let row_count = if book.candidates.is_some() {
                    source_of(book).and_then(|(source, id)| {
                        sessions.iter().find(|(kept, _)| *kept == source).and_then(
                            |(_, session)| session.read(|store| store.book_row_count(id)).ok(),
                        )
                    })
                } else {
                    Some(book.docs)
                };
                let large_permit = if book.library && row_count.is_none_or(|rows| rows >= 4_096) {
                    let Some(permit) = large_books.admit(1, ctx.cancel) else {
                        break;
                    };
                    Some(permit)
                } else {
                    None
                };
                let fetched = match source_of(book) {
                    Some((source, book_id)) => {
                        let session = sessions.iter().find(|(s, _)| *s == source).map(|(_, s)| s);
                        let started = std::time::Instant::now();
                        let rows = match session {
                            Some(session) => session
                                .read(|store| {
                                    fetch_rows(
                                        store,
                                        book,
                                        book_id,
                                        &|| {
                                            ctx.cancel.load(Ordering::Relaxed)
                                                || !session.is_current()
                                        },
                                        row_count,
                                    )
                                })
                                .map_err(|_| ()),
                            None => Err(()),
                        };
                        FETCH_NANOS
                            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        Fetched::Library(rows)
                    }
                    None => Fetched::Stored,
                };
                let Some(permit) = budget.admit(fetched.bytes(), ctx.cancel) else {
                    break;
                };
                if sender.send((i, fetched, permit, large_permit)).is_err() {
                    break;
                }
            });
        }
        drop(sender);
        let mut scanned = receiver
            .into_iter()
            .par_bridge()
            .map(|(i, fetched, permit, large_permit)| {
                let scan = scan_fetched(ctx, plan, &books[i], fetched).map(|scan| (i, scan));
                drop(permit);
                drop(large_permit);
                scan
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        // A book no reader got to (cancelled) counts as unread.
        let mut out: Vec<BookScan> = books.iter().map(|_| BookScan::empty(true)).collect();
        for (i, scan) in scanned.drain(..) {
            out[i] = scan;
        }
        Ok(out)
    })
}

fn scan_fetched(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
    book: &ScopeBook,
    fetched: Fetched,
) -> anyhow::Result<BookScan> {
    let (rows, decoder) = match fetched {
        Fetched::Stored => return scan_stored_book(ctx, plan, book),
        Fetched::Library(Err(())) => return Ok(BookScan::empty(true)),
        Fetched::Library(Ok(None)) => return Ok(BookScan::empty(false)),
        Fetched::Library(Ok(Some(found))) => found,
    };
    use rayon::prelude::*;
    let unavailable = AtomicBool::new(false);
    let matched: HashSet<u64> = rows
        .par_chunks(CANCEL_CHECK_ROWS)
        .map(|chunk| -> anyhow::Result<Vec<u64>> {
            let mut found = Vec::new();
            if ctx.cancel.load(Ordering::Relaxed) {
                return Ok(found);
            }
            let mut dctx = None;
            let mut view = LineView::default();
            for (ordinal, row) in chunk {
                let decoded;
                let bytes: &[u8] = match row {
                    RawRow::Text(bytes) => bytes,
                    RawRow::Frame(frame) => {
                        let Some(decoder) = decoder.as_ref() else {
                            unavailable.store(true, Ordering::Relaxed);
                            continue;
                        };
                        if dctx.is_none() {
                            dctx = Some(FrameDecoder::context()?);
                        }
                        match decoder.decode(dctx.as_mut().expect("created above"), frame) {
                            Ok(bytes) => {
                                decoded = bytes;
                                &decoded
                            }
                            Err(_) => {
                                unavailable.store(true, Ordering::Relaxed);
                                continue;
                            }
                        }
                    }
                };
                // `prepare_row` without its copy: most rows hold no `data:` URI.
                let text = String::from_utf8_lossy(bytes);
                if text.contains("data:") {
                    view.fill(&line_source::strip_data_uris_for_index(&text), false);
                } else {
                    view.fill(&text, false);
                }
                if plan.is_match(&view.text) {
                    found.push(*ordinal);
                }
            }
            Ok(found)
        })
        .collect::<anyhow::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let unavailable = unavailable.load(Ordering::Relaxed);
    if matched.is_empty() {
        return Ok(BookScan::empty(unavailable));
    }
    let documents = match &book.candidates {
        Some(candidates) => candidates.clone(),
        None => book_documents(ctx, &book.file_path)?,
    };
    let hits = documents
        .into_iter()
        .filter(|&(id, _)| ordinal_of(id).is_some_and(|o| matched.contains(&o)))
        .map(|(id, address)| LineHit { id, address })
        .collect();
    Ok(BookScan { hits, unavailable })
}

/// A book whose text the index stores: its lines (or candidates) read from the doc store.
fn scan_stored_book(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
    book: &ScopeBook,
) -> anyhow::Result<BookScan> {
    use rayon::prelude::*;
    let docs = match &book.candidates {
        Some(candidates) => candidates.clone(),
        None => book_documents(ctx, &book.file_path)?,
    };
    let batches = docs
        .par_chunks(VERIFY_BATCH)
        .map(|batch| -> anyhow::Result<BookScan> {
            let mut scan = BookScan::empty(false);
            if ctx.cancel.load(Ordering::Relaxed) {
                return Ok(scan);
            }
            let mut loader = DocumentLoader::new(ctx.searcher);
            let documents = batch
                .iter()
                .map(|&(_, address)| Ok((address, loader.load(address)?)))
                .collect::<anyhow::Result<Vec<_>>>()?;
            let mut view = LineView::default();
            for (&(id, address), text) in batch.iter().zip(indexed_texts(ctx.searcher, &documents)?)
            {
                if text.status == TextStatus::Unavailable {
                    scan.unavailable = true;
                    continue;
                }
                view.fill(&text.text, false);
                if plan.is_match(&view.text) {
                    scan.hits.push(LineHit { id, address });
                }
            }
            Ok(scan)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(batches
        .into_iter()
        .fold(BookScan::empty(false), |mut acc, scan| {
            acc.hits.extend(scan.hits);
            acc.unavailable |= scan.unavailable;
            acc
        }))
}

/// Every live document of one book, with its id.
fn book_documents(ctx: &LineSearch<'_>, file_path: &str) -> anyhow::Result<Vec<(u64, DocAddress)>> {
    let query = TermQuery::new(
        Term::from_field_text(ctx.schema.get_field("filePath")?, file_path),
        IndexRecordOption::Basic,
    );
    Ok(ctx.searcher.search(&query, &IdAddressCollector)?)
}

fn str_field(document: &TantivyDocument, field: Field) -> String {
    document
        .get_first(field)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// The results of `hits`, their snippets painted, with every match's UTF-16 range in the
/// line's text (what [`crate::api::search_engine::SearchEngine::get_document_by_id`] returns).
fn hydrate(
    ctx: &LineSearch<'_>,
    plan: &LinePlan,
    hits: &[LineHit],
    page: &Page,
) -> anyhow::Result<Vec<(SearchResult, Vec<HighlightRange>)>> {
    let schema = ctx.schema;
    let title_f = schema.get_field("title")?;
    let reference_f = schema.get_field("reference")?;
    let segment_f = schema.get_field("segment")?;
    let is_pdf_f = schema.get_field("isPdf")?;
    let file_path_f = schema.get_field("filePath")?;
    let mut loader = DocumentLoader::new(ctx.searcher);
    let documents = hits
        .iter()
        .map(|hit| Ok((hit.address, loader.load(hit.address)?)))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let texts = indexed_texts(ctx.searcher, &documents)?;
    let mut out = Vec::with_capacity(hits.len());
    for ((hit, (_, document)), text) in hits.iter().zip(&documents).zip(texts) {
        let view = LineView::of(&text.text, true);
        let ranges: Vec<(u32, u32)> = plan
            .find(&view.text)
            .iter()
            .map(|r| view.source_range(r))
            .collect();
        let snippet = paint_snippet(&text.text, &ranges, page);
        out.push((
            SearchResult {
                title: str_field(document, title_f),
                reference: str_field(document, reference_f),
                text: snippet,
                id: hit.id,
                segment: document
                    .get_first(segment_f)
                    .and_then(|v| v.as_u64())
                    .unwrap_or_default(),
                is_pdf: document
                    .get_first(is_pdf_f)
                    .and_then(|v| v.as_bool())
                    .unwrap_or_default(),
                file_path: str_field(document, file_path_f),
                merged_count: 1,
                merged: Vec::new(),
                text_status: text.status,
                continues_to_next_line: false,
            },
            ranges
                .into_iter()
                .map(|(start, end)| HighlightRange { start, end })
                .collect(),
        ));
    }
    Ok(out)
}

/// At most `max_chars` of `text` around its first match, HTML-escaped, every match inside
/// wrapped in the highlight tags. `ranges` are UTF-16 offsets into `text`.
fn paint_snippet(text: &str, ranges: &[(u32, u32)], page: &Page) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let max = (page.max_chars.max(1) as usize).min(units.len().max(1));
    let first = ranges.first().map_or(0, |r| r.0 as usize);
    let mut start = first.saturating_sub(max / 4);
    if start > 0 {
        // Start on a word.
        if let Some(space) = units[..start].iter().rposition(|&u| u == u16::from(b' ')) {
            start = space + 1;
        }
    }
    let end = (start + max).min(units.len());
    let decode = |a: usize, b: usize| String::from_utf16_lossy(&units[a.min(b)..b]);
    let mut out = String::new();
    let mut at = start;
    for &(s, e) in ranges {
        let (s, e) = ((s as usize).max(start), (e as usize).min(end));
        if s >= e || s < at {
            continue;
        }
        out.push_str(&htmlescape::encode_minimal(&decode(at, s)));
        out.push_str(&page.highlight_prefix);
        out.push_str(&htmlescape::encode_minimal(&decode(s, e)));
        out.push_str(&page.highlight_postfix);
        at = e;
    }
    out.push_str(&htmlescape::encode_minimal(&decode(at, end)));
    out
}

// ── Collectors and the hit set as a query ──────────────────────────────────────

/// `(id, address)` of every matching document.
struct IdAddressCollector;

struct IdAddressSegment {
    segment: SegmentOrdinal,
    ids: tantivy::columnar::Column<u64>,
    found: Vec<(u64, DocAddress)>,
}

impl Collector for IdAddressCollector {
    type Fruit = Vec<(u64, DocAddress)>;
    type Child = IdAddressSegment;

    fn for_segment(
        &self,
        segment: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<IdAddressSegment> {
        Ok(IdAddressSegment {
            segment,
            ids: reader.fast_fields().u64("id")?,
            found: Vec::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Vec<(u64, DocAddress)>>) -> tantivy::Result<Self::Fruit> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for IdAddressSegment {
    type Fruit = Vec<(u64, DocAddress)>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        let id = self.ids.first(doc).unwrap_or_default();
        self.found.push((id, DocAddress::new(self.segment, doc)));
    }

    fn harvest(self) -> Self::Fruit {
        self.found
    }
}

/// The books of the matching documents: their first id, size and storage, and with
/// `keep_documents` the matching documents themselves.
struct ScopeBooksCollector {
    keep_documents: bool,
}

#[derive(Default)]
struct SegmentBook {
    min_id: u64,
    docs: u64,
    library: bool,
    documents: Vec<(u64, DocAddress)>,
}

struct ScopeBooksSegment {
    keep_documents: bool,
    segment: SegmentOrdinal,
    paths: Option<tantivy::columnar::StrColumn>,
    ids: tantivy::columnar::Column<u64>,
    checks: tantivy::columnar::Column<u64>,
    books: HashMap<u64, SegmentBook>,
}

impl Collector for ScopeBooksCollector {
    type Fruit = Vec<ScopeBook>;
    type Child = ScopeBooksSegment;

    fn for_segment(
        &self,
        segment: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<ScopeBooksSegment> {
        let fast = reader.fast_fields();
        Ok(ScopeBooksSegment {
            keep_documents: self.keep_documents,
            segment,
            paths: fast.str("filePath")?,
            ids: fast.u64("id")?,
            checks: fast.u64(LINE_CHECK_FIELD)?,
            books: HashMap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Vec<ScopeBook>>) -> tantivy::Result<Vec<ScopeBook>> {
        let mut merged: HashMap<String, ScopeBook> = HashMap::new();
        for book in fruits.into_iter().flatten() {
            match merged.get_mut(&book.file_path) {
                Some(seen) => {
                    seen.min_id = seen.min_id.min(book.min_id);
                    seen.docs += book.docs;
                    seen.library |= book.library;
                    if let (Some(all), Some(more)) = (seen.candidates.as_mut(), book.candidates) {
                        all.extend(more);
                    }
                }
                None => {
                    merged.insert(book.file_path.clone(), book);
                }
            }
        }
        Ok(merged.into_values().collect())
    }
}

impl SegmentCollector for ScopeBooksSegment {
    type Fruit = Vec<ScopeBook>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        let Some(ord) = self.paths.as_ref().and_then(|p| p.term_ords(doc).next()) else {
            return;
        };
        let id = self.ids.first(doc).unwrap_or(u64::MAX);
        let book = self.books.entry(ord).or_insert_with(|| SegmentBook {
            min_id: u64::MAX,
            ..SegmentBook::default()
        });
        book.min_id = book.min_id.min(id);
        book.docs += 1;
        book.library |= self.checks.first(doc).is_some();
        if self.keep_documents {
            book.documents
                .push((id, DocAddress::new(self.segment, doc)));
        }
    }

    fn harvest(self) -> Vec<ScopeBook> {
        let Some(paths) = self.paths else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(self.books.len());
        let mut buf = String::new();
        for (ord, book) in self.books {
            buf.clear();
            if paths.ord_to_str(ord, &mut buf).unwrap_or(false) {
                out.push(ScopeBook {
                    file_path: buf.clone(),
                    min_id: book.min_id,
                    docs: book.docs,
                    library: book.library,
                    candidates: self.keep_documents.then_some(book.documents),
                });
            }
        }
        out
    }
}

/// Exactly the documents of a finished line search, for facet counting on its searcher.
#[derive(Clone, Debug)]
pub(crate) struct HitSetQuery {
    docs: Arc<HashMap<SegmentOrdinal, Vec<DocId>>>,
    segments: Arc<HashMap<tantivy::index::SegmentId, SegmentOrdinal>>,
}

impl HitSetQuery {
    pub(crate) fn new(searcher: &Searcher, outcome: &LineOutcome) -> Self {
        let mut docs: HashMap<SegmentOrdinal, Vec<DocId>> = HashMap::new();
        for hit in &outcome.hits {
            docs.entry(hit.address.segment_ord)
                .or_default()
                .push(hit.address.doc_id);
        }
        for list in docs.values_mut() {
            list.sort_unstable();
            list.dedup();
        }
        let segments = searcher
            .segment_readers()
            .iter()
            .enumerate()
            .map(|(ord, reader)| (reader.segment_id(), ord as SegmentOrdinal))
            .collect();
        HitSetQuery {
            docs: Arc::new(docs),
            segments: Arc::new(segments),
        }
    }
}

impl Query for HitSetQuery {
    fn weight(&self, _scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(self.clone()))
    }
}

impl Weight for HitSetQuery {
    fn scorer(&self, reader: &SegmentReader, _boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let docs = self
            .segments
            .get(&reader.segment_id())
            .and_then(|ord| self.docs.get(ord));
        Ok(match docs {
            Some(docs) if !docs.is_empty() => Box::new(ListScorer {
                docs: docs.clone(),
                at: 0,
            }),
            _ => Box::new(EmptyScorer),
        })
    }

    fn explain(&self, _reader: &SegmentReader, _doc: DocId) -> tantivy::Result<Explanation> {
        Ok(Explanation::new("user regex line hit", 1.0))
    }
}

struct ListScorer {
    docs: Vec<DocId>,
    at: usize,
}

impl DocSet for ListScorer {
    fn advance(&mut self) -> DocId {
        self.at += 1;
        self.doc()
    }

    fn doc(&self) -> DocId {
        self.docs.get(self.at).copied().unwrap_or(TERMINATED)
    }

    fn size_hint(&self) -> u32 {
        self.docs.len() as u32
    }
}

impl Scorer for ListScorer {
    fn score(&mut self) -> Score {
        1.0
    }
}

#[cfg(test)]
#[path = "user_regex_tests.rs"]
mod tests;

#[cfg(test)]
mod read_ahead_tests {
    use super::*;

    #[test]
    fn read_ahead_bytes_block_until_the_previous_book_is_scanned() {
        let budget = ReadAheadBudget::default();
        let cancel = AtomicBool::new(false);
        let first = budget.admit(FETCH_AHEAD_BYTES + 1, &cancel).unwrap();
        std::thread::scope(|scope| {
            let (ready, receiver) = std::sync::mpsc::channel();
            let (admitted, got) = std::sync::mpsc::channel();
            let (budget, cancel) = (&budget, &cancel);
            scope.spawn(move || {
                ready.send(()).unwrap();
                let permit = budget.admit(1, cancel).unwrap();
                admitted.send(()).unwrap();
                drop(permit);
            });
            receiver.recv().unwrap();
            assert!(got
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err());
            drop(first);
            got.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        });
        assert_eq!(*budget.used.lock().unwrap(), 0);
    }

    #[test]
    fn cancellation_releases_a_reader_waiting_for_the_byte_budget() {
        let budget = ReadAheadBudget::default();
        let cancel = AtomicBool::new(false);
        let _first = budget.admit(FETCH_AHEAD_BYTES, &cancel).unwrap();
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| budget.admit(1, &cancel).is_none());
            cancel.store(true, Ordering::Relaxed);
            assert!(waiting.join().unwrap());
        });
    }
    #[test]
    fn two_large_books_do_not_start_fetching_together_and_waiting_can_cancel() {
        let gate = ReadAheadBudget::new(1);
        let cancel = AtomicBool::new(false);
        let first = gate.admit(1, &cancel).unwrap();
        std::thread::scope(|scope| {
            let (fetching, received) = std::sync::mpsc::channel();
            let (gate, cancel) = (&gate, &cancel);
            let second = scope.spawn(move || {
                let permit = gate.admit(1, cancel);
                if permit.is_some() {
                    fetching.send(()).unwrap();
                }
                permit.is_none()
            });
            assert!(received
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err());
            cancel.store(true, Ordering::Relaxed);
            assert!(second.join().unwrap());
        });
        drop(first);
        assert_eq!(*gate.used.lock().unwrap(), 0);
    }
}
