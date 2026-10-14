//! Whole-term automata over the term dictionary, built as `regex-automata`
//! dense DFAs.
//!
//! A pattern accepts a dictionary term when the *whole* term matches — the
//! contract of `tantivy_fst::Regex`, whose syntax restrictions are mirrored
//! here (no look-around, no lazy repetition, no byte classes) so a pattern
//! fails or succeeds exactly as it did there. The DFA is anchored and uses
//! `MatchKind::All`, so a term is accepted iff any of its branches matches
//! it in full, and the accepting state names every branch that matched —
//! which is what lets one DFA over all branches still rank terms by branch.

use std::sync::Arc;

use regex_automata::dfa::{dense, Automaton as _, StartKind};
use regex_automata::nfa::thompson::{self, WhichCaptures};
use regex_automata::util::primitives::StateID;
use regex_automata::util::start;
use regex_automata::{Anchored, MatchKind};
use regex_syntax::hir::{Class, Hir, HirKind};

/// Size of the compiled NFA — the instruction budget `tantivy_fst::Regex`
/// enforced (10 MiB).
const NFA_SIZE_LIMIT: usize = 10 << 20;
/// Memory of one finished DFA. Byte classes keep a state far below the
/// 4 KiB it took in tantivy-fst, so this admits much more than its old
/// 8 192-state cap while still bounding a hostile pattern.
const DFA_SIZE_LIMIT: usize = 32 << 20;
/// Working memory of determinization.
const DETERMINIZE_SIZE_LIMIT: usize = 64 << 20;

/// One anchored dense DFA over a contiguous range of branches.
pub(crate) struct TermDfa {
    dfa: dense::DFA<Vec<u32>>,
    start: StateID,
    /// Global index of this DFA's pattern 0.
    first_branch: usize,
}

impl std::fmt::Debug for TermDfa {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TermDfa")
            .field("patterns", &self.dfa.pattern_len())
            .field("first_branch", &self.first_branch)
            .finish()
    }
}

impl TermDfa {
    fn new(dfa: dense::DFA<Vec<u32>>, first_branch: usize) -> Result<Self, String> {
        let start = dfa
            .start_state(&start::Config::new().anchored(Anchored::Yes))
            .map_err(|e| format!("DFA start state: {e}"))?;
        Ok(Self {
            dfa,
            start,
            first_branch,
        })
    }

    pub(crate) fn memory_usage(&self) -> usize {
        self.dfa.memory_usage()
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

/// Compiles `branches` (each a whole-term pattern) into automata.
pub(crate) fn compile_branches(branches: &[String]) -> Result<Arc<TermAutomata>, String> {
    build_branches(branches).map(Arc::new)
}

/// One DFA for `pattern` (no splitting).
pub(crate) fn compile_pattern(pattern: &str) -> Result<Arc<TermDfa>, String> {
    let automata = compile_branches(std::slice::from_ref(&pattern.to_string()))?;
    Ok(automata
        .single()
        .expect("a single branch compiles to a single DFA")
        .clone())
}

fn build_branches(branches: &[String]) -> Result<TermAutomata, String> {
    let hirs: Vec<Hir> = branches
        .iter()
        .map(|b| parse(b))
        .collect::<Result<_, _>>()?;
    let mut parts = Vec::new();
    build_range(&hirs, 0, &mut parts)?;
    Ok(TermAutomata { parts })
}

/// Builds `hirs` as one DFA, halving the range while the union is too big.
fn build_range(hirs: &[Hir], first: usize, parts: &mut Vec<Arc<TermDfa>>) -> Result<(), String> {
    match build_dfa(hirs) {
        Ok(dfa) => {
            parts.push(Arc::new(TermDfa::new(dfa, first)?));
            Ok(())
        }
        Err(e) if hirs.len() > 1 => {
            log::debug!(
                "union of {} branches does not fit one DFA ({e}); splitting",
                hirs.len()
            );
            let mid = hirs.len() / 2;
            build_range(&hirs[..mid], first, parts)?;
            build_range(&hirs[mid..], first + mid, parts)
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

#[cfg(test)]
mod tests {
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
}
