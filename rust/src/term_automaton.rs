//! Whole-term automata over the term dictionary, built as `regex-automata`
//! dense DFAs and shared through one process-wide cache.
//!
//! A pattern accepts a dictionary term when the *whole* term matches — the
//! contract of `tantivy_fst::Regex`, whose syntax restrictions are mirrored
//! here (no look-around, no lazy repetition, no byte classes) so a pattern
//! fails or succeeds exactly as it did there. The DFA is anchored and uses
//! `MatchKind::All`, so a term is accepted iff any of its branches matches
//! it in full, and the accepting state names every branch that matched —
//! which is what lets one DFA over all branches still rank terms by branch.

use std::cell::Cell;
use std::sync::{Arc, Mutex};

use crate::quote_expansion::{is_quote, QuoteTransparent};
use lru::LruCache;
use once_cell::sync::Lazy;
use regex_automata::dfa::{dense, Automaton as _, StartKind};
use regex_automata::nfa::thompson::{self, WhichCaptures};
use regex_automata::util::primitives::StateID;
use regex_automata::util::start;
use regex_automata::{Anchored, MatchKind};
use regex_syntax::hir::{Class, Hir, HirKind};
use tantivy::InvertedIndexReader;

/// Size of the compiled NFA — the instruction budget `tantivy_fst::Regex`
/// enforced (10 MiB).
const NFA_SIZE_LIMIT: usize = 10 << 20;
/// Memory of one finished DFA. Byte classes keep a state far below the
/// 4 KiB it took in tantivy-fst, so this admits much more than its old
/// 8 192-state cap while still bounding a hostile pattern.
const DFA_SIZE_LIMIT: usize = 32 << 20;
/// Working memory of determinization.
const DETERMINIZE_SIZE_LIMIT: usize = 64 << 20;
/// Cache bounds: entry count and the summed DFA memory.
const CACHE_ENTRIES: usize = 256;
const CACHE_BYTES: usize = 96 << 20;

/// One anchored dense DFA over a contiguous range of branches.
pub(crate) struct TermDfa {
    dfa: dense::DFA<Vec<u32>>,
    start: StateID,
    /// Global index of this DFA's pattern 0.
    first_branch: usize,
    /// The branches it was built from, for `Debug`: the page cache keys queries by it.
    branches: String,
}

impl std::fmt::Debug for TermDfa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermDfa")
            .field("patterns", &self.dfa.pattern_len())
            .field("first_branch", &self.first_branch)
            .field("branches", &self.branches)
            .finish()
    }
}

impl TermDfa {
    fn new(
        dfa: dense::DFA<Vec<u32>>,
        first_branch: usize,
        branches: &[String],
    ) -> Result<Self, String> {
        let start = dfa
            .start_state(&start::Config::new().anchored(Anchored::Yes))
            .map_err(|e| format!("DFA start state: {e}"))?;
        Ok(Self {
            dfa,
            start,
            first_branch,
            branches: format!("{branches:?}"),
        })
    }

    pub(crate) fn memory_usage(&self) -> usize {
        self.dfa.memory_usage() + self.branches.len()
    }

    /// The lowest global branch index that accepts `term` in full.
    pub(crate) fn matched_branch(&self, term: &[u8]) -> Option<usize> {
        let mut state = self.start;
        for &byte in term {
            state = self.dfa.next_state(state, byte);
            if self.dfa.is_dead_state(state) {
                return None;
            }
        }
        let end = self.dfa.next_eoi_state(state);
        if !self.dfa.is_match_state(end) {
            return None;
        }
        (0..self.dfa.match_len(end))
            .map(|i| self.dfa.match_pattern(end, i).as_usize())
            .min()
            .map(|pattern| self.first_branch + pattern)
    }

    pub(crate) fn branch_count(&self) -> usize {
        self.dfa.pattern_len()
    }
}

impl tantivy_fst::Automaton for TermDfa {
    type State = StateID;

    #[inline]
    fn start(&self) -> StateID {
        self.start
    }

    #[inline]
    fn is_match(&self, state: &StateID) -> bool {
        // Dense DFAs report a match one transition late: the end-of-input
        // transition decides whether the bytes so far form a whole match.
        self.dfa.is_match_state(self.dfa.next_eoi_state(*state))
    }

    #[inline]
    fn can_match(&self, state: &StateID) -> bool {
        !self.dfa.is_dead_state(*state)
    }

    #[inline]
    fn accept(&self, state: &StateID, byte: u8) -> StateID {
        self.dfa.next_state(*state, byte)
    }
}

/// The automata of one branch list, in branch order. Usually a single DFA;
/// a list whose union outgrows the size limits is split into several.
#[derive(Debug)]
pub(crate) struct TermAutomata {
    pub(crate) parts: Vec<Arc<TermDfa>>,
}

impl TermAutomata {
    fn memory_usage(&self) -> usize {
        self.parts.iter().map(|p| p.memory_usage()).sum()
    }

    /// The single DFA of an unsplit list (always the case for one branch).
    pub(crate) fn single(&self) -> Option<&Arc<TermDfa>> {
        match self.parts.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }
}

type CacheValue = Result<Arc<TermAutomata>, String>;

struct Cache {
    entries: LruCache<Vec<String>, CacheValue>,
    bytes: usize,
}

/// Keyed by the branch list alone: a DFA does not depend on the field it
/// scans, so one entry serves the plain and the vocalized dictionary alike.
static CACHE: Lazy<Mutex<Cache>> = Lazy::new(|| {
    Mutex::new(Cache {
        entries: LruCache::unbounded(),
        bytes: 0,
    })
});

thread_local! {
    static BUILDS: Cell<u64> = const { Cell::new(0) };
}

/// DFA builds run by this thread (test hook for the cache).
#[cfg(test)]
pub(crate) fn builds_on_this_thread() -> u64 {
    BUILDS.with(Cell::get)
}

fn cached_bytes(value: &CacheValue) -> usize {
    value.as_ref().map_or(0, |a| a.memory_usage())
}

/// Compiles `branches` (each a whole-term pattern) into automata, served
/// from the shared cache. Failures are cached too, so a pattern over the
/// size limits is not rebuilt by every search, count, page and highlight
/// call behind one user search. Builds run outside the lock.
pub(crate) fn compile_branches(branches: &[String]) -> Result<Arc<TermAutomata>, String> {
    if let Some(hit) = CACHE.lock().unwrap().entries.get(branches) {
        return hit.clone();
    }
    let value = build_branches(branches).map(Arc::new);
    let bytes = cached_bytes(&value);
    let mut cache = CACHE.lock().unwrap();
    if let Some(old) = cache.entries.put(branches.to_vec(), value.clone()) {
        cache.bytes -= cached_bytes(&old);
    }
    cache.bytes += bytes;
    while cache.entries.len() > CACHE_ENTRIES
        || (cache.bytes > CACHE_BYTES && cache.entries.len() > 1)
    {
        match cache.entries.pop_lru() {
            Some((_, old)) => cache.bytes -= cached_bytes(&old),
            None => break,
        }
    }
    value
}

/// One DFA for `pattern` (no splitting), cached like [`compile_branches`].
pub(crate) fn compile_pattern(pattern: &str) -> Result<Arc<TermDfa>, String> {
    let automata = compile_branches(std::slice::from_ref(&pattern.to_string()))?;
    Ok(automata
        .single()
        .expect("a single branch compiles to a single DFA")
        .clone())
}

/// [`compile_branches`] without the cache (cost measurements).
#[cfg(test)]
pub(crate) fn build_uncached(branches: &[String]) -> Result<TermAutomata, String> {
    build_branches(branches)
}

fn build_branches(branches: &[String]) -> Result<TermAutomata, String> {
    BUILDS.with(|c| c.set(c.get() + 1));
    let hirs: Vec<Hir> = branches
        .iter()
        .map(|b| parse(b))
        .collect::<Result<_, _>>()?;
    let mut parts = Vec::new();
    build_range(&hirs, branches, 0, &mut parts)?;
    Ok(TermAutomata { parts })
}

/// Builds `hirs` as one DFA, halving the range while the union is too big.
fn build_range(
    hirs: &[Hir],
    branches: &[String],
    first: usize,
    parts: &mut Vec<Arc<TermDfa>>,
) -> Result<(), String> {
    match build_dfa(hirs) {
        Ok(dfa) => {
            parts.push(Arc::new(TermDfa::new(dfa, first, branches)?));
            Ok(())
        }
        Err(e) if hirs.len() > 1 => {
            log::debug!(
                "union of {} branches does not fit one DFA ({e}); splitting",
                hirs.len()
            );
            let mid = hirs.len() / 2;
            build_range(&hirs[..mid], &branches[..mid], first, parts)?;
            build_range(&hirs[mid..], &branches[mid..], first + mid, parts)
        }
        Err(e) => Err(e),
    }
}

fn build_dfa(hirs: &[Hir]) -> Result<dense::DFA<Vec<u32>>, String> {
    let nfa = thompson::Compiler::new()
        .configure(
            thompson::Config::new()
                .nfa_size_limit(Some(NFA_SIZE_LIMIT))
                .which_captures(WhichCaptures::None),
        )
        .build_many_from_hir(hirs)
        .map_err(|e| format!("regex too large: {e}"))?;
    dense::Builder::new()
        .configure(
            dense::Config::new()
                .match_kind(MatchKind::All)
                .start_kind(StartKind::Anchored)
                .minimize(false)
                .dfa_size_limit(Some(DFA_SIZE_LIMIT))
                .determinize_size_limit(Some(DETERMINIZE_SIZE_LIMIT)),
        )
        .build_from_nfa(&nfa)
        .map_err(|e| format!("regex DFA too large: {e}"))
}

/// Parses with the same parser settings `tantivy_fst::Regex` used and
/// rejects the constructs it rejected.
fn parse(pattern: &str) -> Result<Hir, String> {
    let hir = regex_syntax::Parser::new()
        .parse(pattern)
        .map_err(|e| format!("invalid regex: {e}"))?;
    check_supported(&hir)?;
    Ok(hir)
}

fn check_supported(hir: &Hir) -> Result<(), String> {
    match hir.kind() {
        HirKind::Look(_) => Err("empty-width assertions are not supported in term regexes".into()),
        HirKind::Class(Class::Bytes(_)) => {
            Err("byte classes are not supported in term regexes".into())
        }
        HirKind::Repetition(rep) if !rep.greedy => {
            Err("lazy repetition is not supported in term regexes".into())
        }
        HirKind::Repetition(rep) => check_supported(&rep.sub),
        HirKind::Capture(cap) => check_supported(&cap.sub),
        HirKind::Concat(subs) | HirKind::Alternation(subs) => {
            subs.iter().try_for_each(check_supported)
        }
        HirKind::Empty | HirKind::Literal(_) | HirKind::Class(_) => Ok(()),
    }
}

/// A dictionary term matched for one query word: the lowest branch that
/// accepts it (extra automata rank after every branch) and its document
/// frequency summed over the segments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankedTerm {
    pub(crate) term: String,
    pub(crate) branch: usize,
    pub(crate) doc_freq: u64,
}

/// One segment scan: `(term, branch, doc_freq)` hits and whether the cap cut it.
type ScanHits = (Vec<(String, usize, u32)>, bool);

/// Distinct terms gathered before ranking — a memory cap, far above any
/// collection budget a word is served with.
pub(crate) const COLLECT_CAP: usize = 50_000;

/// Every term `automata` (then each of `extra`) matches in `segments`,
/// ranked by branch, then length — the affix letters around the typed root
/// — then document frequency (descending). Scans run in parallel; at most
/// `cap` distinct terms are gathered, in automaton order, and the flag
/// reports whether the cap cut the scan.
pub(crate) fn collect_ranked<T>(
    segments: &[Arc<InvertedIndexReader>],
    automata: &TermAutomata,
    extra: &[T],
    cap: usize,
) -> tantivy::Result<(Vec<RankedTerm>, bool)>
where
    T: tantivy_fst::Automaton + Sync,
    T::State: Clone,
{
    use rayon::prelude::*;

    let cancellation = crate::lexical_cancellation::LexicalFlag::current();
    let branches: usize = automata.parts.iter().map(|p| p.branch_count()).sum();
    let sources = automata.parts.len() + extra.len();
    let pairs: Vec<(usize, usize)> = (0..sources)
        .flat_map(|a| (0..segments.len()).map(move |s| (a, s)))
        .collect();
    let scans: Vec<ScanHits> = pairs
        .par_iter()
        .map(|&(source, segment)| {
            let inverted = &segments[segment];
            if let Some(part) = automata.parts.get(source) {
                let single = (part.branch_count() == 1).then_some(part.first_branch);
                scan(inverted, part.as_ref(), cap, &cancellation, |term| {
                    single.or_else(|| quote_transparent_branch(part, term))
                })
            } else {
                let branch = branches + (source - automata.parts.len());
                scan(
                    inverted,
                    &extra[source - automata.parts.len()],
                    cap,
                    &cancellation,
                    |_| Some(branch),
                )
            }
        })
        .collect::<tantivy::Result<_>>()?;

    let mut capped = false;
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut ranked: Vec<RankedTerm> = Vec::new();
    for (hits, scan_capped) in scans {
        capped |= scan_capped;
        for (term, branch, doc_freq) in hits {
            match index.get(&term) {
                Some(&i) => {
                    let entry = &mut ranked[i];
                    entry.doc_freq += u64::from(doc_freq);
                    entry.branch = entry.branch.min(branch);
                }
                None if ranked.len() >= cap => capped = true,
                None => {
                    index.insert(term.clone(), ranked.len());
                    ranked.push(RankedTerm {
                        term,
                        branch,
                        doc_freq: u64::from(doc_freq),
                    });
                }
            }
        }
    }
    let mut keyed: Vec<(usize, RankedTerm)> = ranked
        .into_iter()
        .map(|t| (t.term.chars().count(), t))
        .collect();
    keyed.sort_unstable_by(|(a_len, a), (b_len, b)| {
        (a.branch, a_len, std::cmp::Reverse(a.doc_freq), &a.term).cmp(&(
            b.branch,
            b_len,
            std::cmp::Reverse(b.doc_freq),
            &b.term,
        ))
    });
    Ok((keyed.into_iter().map(|(_, t)| t).collect(), capped))
}

/// The branch [`TermDfa::matched_branch`] gives `term` as written or without its
/// quotes (the scan is quote-transparent), the lower of the two.
fn quote_transparent_branch(part: &TermDfa, term: &[u8]) -> Option<usize> {
    let written = part.matched_branch(term);
    if !term.iter().copied().any(is_quote) {
        return written;
    }
    let bare: Vec<u8> = term.iter().copied().filter(|&b| !is_quote(b)).collect();
    let bare = (!bare.is_empty())
        .then(|| part.matched_branch(&bare))
        .flatten();
    match (written, bare) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn scan<A>(
    inverted: &InvertedIndexReader,
    automaton: &A,
    cap: usize,
    cancellation: &crate::lexical_cancellation::LexicalFlag,
    branch_of: impl Fn(&[u8]) -> Option<usize>,
) -> tantivy::Result<ScanHits>
where
    A: tantivy_fst::Automaton,
    A::State: Clone,
{
    let mut hits = Vec::new();
    let mut stream = inverted
        .terms()
        .search(QuoteTransparent::new(automaton))
        .into_stream()?;
    while stream.advance() {
        cancellation.check_tantivy()?;
        if hits.len() >= cap {
            return Ok((hits, true));
        }
        let key = stream.key();
        if let (Ok(term), Some(branch)) = (std::str::from_utf8(key), branch_of(key)) {
            hits.push((term.to_string(), branch, stream.value().doc_freq));
        }
    }
    Ok((hits, false))
}

/// The ranked prefix a word is served with: terms are taken in rank order
/// until `max_terms` terms or `postings_budget` summed document frequency
/// is reached (the term that reaches a budget is kept). `truncated` when a
/// budget or the collection cap dropped any matching term.
pub(crate) fn within_budgets(
    ranked: Vec<RankedTerm>,
    capped: bool,
    max_terms: usize,
    postings_budget: u64,
) -> (Vec<RankedTerm>, bool) {
    let total = ranked.len();
    let mut kept = Vec::with_capacity(total.min(max_terms.max(1)));
    let mut cost = 0u64;
    for term in ranked {
        cost = cost.saturating_add(term.doc_freq);
        kept.push(term);
        if kept.len() >= max_terms || cost >= postings_budget {
            break;
        }
    }
    let truncated = capped || kept.len() < total;
    (kept, truncated)
}

#[cfg(test)]
mod tests {
    /// The page cache keys a search by its query's `Debug` text.
    #[test]
    fn different_patterns_print_differently() {
        let one = super::compile_pattern("א.ב").unwrap();
        let other = super::compile_pattern("ג.ד").unwrap();
        assert_ne!(format!("{one:?}"), format!("{other:?}"));
    }

    use super::*;
    use tantivy_fst::Automaton;

    fn accepts<A: Automaton>(a: &A, s: &str) -> bool {
        let mut state = a.start();
        for &b in s.as_bytes() {
            if !a.can_match(&state) {
                return false;
            }
            state = a.accept(&state, b);
        }
        a.is_match(&state)
    }

    #[test]
    fn whole_term_semantics() {
        let dfa = compile_pattern("(?:ו|ב)?שבת").unwrap();
        assert!(accepts(dfa.as_ref(), "שבת"));
        assert!(accepts(dfa.as_ref(), "בשבת"));
        assert!(!accepts(dfa.as_ref(), "בשבתות"));
        assert!(!accepts(dfa.as_ref(), "שב"));
    }

    #[test]
    fn rejects_what_tantivy_fst_rejects() {
        for bad in [
            "^שבת",
            "שבת$",
            r"\bשבת",
            "שב.*?ת",
            "(?-u:\\xFF)",
            "(",
            "[א-",
        ] {
            assert!(
                tantivy_fst::Regex::new(bad).is_err(),
                "tantivy-fst accepted {bad:?}"
            );
            assert!(compile_pattern(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn matched_branch_is_the_lowest_accepting_branch() {
        let branches: Vec<String> = ["שבת", ".{0,2}שבת", "שבת.{0,2}"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let automata = compile_branches(&branches).unwrap();
        let dfa = automata.single().unwrap();
        assert_eq!(dfa.matched_branch("שבת".as_bytes()), Some(0));
        assert_eq!(dfa.matched_branch("בשבת".as_bytes()), Some(1));
        assert_eq!(dfa.matched_branch("שבתו".as_bytes()), Some(2));
        assert_eq!(dfa.matched_branch("בשבתו".as_bytes()), None);
    }

    #[test]
    fn a_quoted_term_ranks_by_the_branch_of_its_quote_free_spelling() {
        let automata = build_uncached(&["רמבם".to_string(), "ה.*".to_string()]).unwrap();
        let dfa = automata.single().unwrap();
        assert_eq!(quote_transparent_branch(dfa, "רמב\"ם".as_bytes()), Some(0));
        assert_eq!(quote_transparent_branch(dfa, "הרמב\"ם".as_bytes()), Some(1));
        assert_eq!(quote_transparent_branch(dfa, "\"".as_bytes()), None);
    }

    #[test]
    fn cache_serves_repeats_and_failures() {
        let pattern = vec!["(?:קאש|טסט)מטמון".to_string()];
        let before = builds_on_this_thread();
        compile_branches(&pattern).unwrap();
        compile_branches(&pattern).unwrap();
        compile_pattern("(?:קאש|טסט)מטמון").unwrap();
        assert_eq!(builds_on_this_thread() - before, 1);

        let bad = vec!["(?:א|ב".to_string()];
        let before = builds_on_this_thread();
        assert!(compile_branches(&bad).is_err());
        assert!(compile_branches(&bad).is_err());
        assert_eq!(builds_on_this_thread() - before, 1);
    }
}
