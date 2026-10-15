//! Per-pair gap enforcement for phrase queries.
//!
//! tantivy's phrase slop is a single *cumulative, unordered* budget: the
//! positional deviation of every word pair draws from one shared allowance,
//! and `abs_diff` lets adjacent words match in reverse order. The Otzaria UI
//! promises something stricter — an *in-order* phrase where each adjacent
//! pair `i, i+1` allows at most `gaps[i]` intermediate words (the global
//! `distance`, or the per-pair `custom_spacing` values).
//!
//! [`GapVerifiedPhraseQuery`] closes that gap: it wraps the engine's
//! `RegexPhraseQuery` (whose slop is set to the *sum* of the per-pair
//! allowances — a recall superset under the cumulative budget) and re-checks
//! every candidate document against the real token positions from the
//! positional postings, admitting only documents that contain an in-order
//! occurrence `w0 … w1 … w_{k-1}` with every pair inside its own allowance.
//! This is the same intermediate-word model the snippet phrase filter and
//! `display_highlight` use, so what the engine returns, what the results
//! snippet paints, and what an opened book highlights all agree.
//!
//! The wrapper is a straight `Query`/`Weight`/`Scorer` sandwich, so every
//! consumer — top-k collection, counting, per-book counting, facet counting,
//! boolean composition with the facet filter — sees the verified doc set with
//! no special-casing.
//!
//! [`TermListPhraseQuery`] is the expansion-safe sibling: instead of wrapping
//! a `RegexPhraseQuery` (whose weight *errors* when the matched-term count
//! crosses `max_expansions`), it is built from term sets the engine already
//! materialized under its collection budgets — one `Vec<Term>` per word
//! position. Candidate documents are driven by a plain boolean AND of
//! per-position `TermSetQuery`s (a recall superset of any phrase occurrence),
//! and the same [`GapVerifiedScorer`] then admits only documents with an
//! in-order occurrence inside the per-pair allowances. No joined DFA is ever
//! compiled and no expansion ceiling exists to overflow, so this path
//! *degrades* (the materialization budgets truncate from the back) instead of
//! failing — the engine uses it whenever a phrase's expansions outgrow the
//! exact `RegexPhraseQuery` path.
//!
//! [`Proximity::AnyOrder`] drops the order: every word takes its own position
//! and all of them fit one window of `max_span` positions. tantivy's slop is
//! no superset of every permutation, so it only runs through
//! [`TermListPhraseQuery`], verified by [`WordWindows`].

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;

use tantivy::postings::{Postings, SegmentPostings};
use tantivy::query::{
    BooleanQuery, EmptyScorer, EnableScoring, Explanation, Occur, Query, RegexPhraseQuery, Scorer,
    TermSetQuery, Weight,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, InvertedIndexReader, Score, SegmentReader, Term, TERMINATED};
use tantivy_fst::Automaton;

use crate::term_automaton::TermDfa;

/// Positional postings for every materialized term that exists in this
/// segment — one `Vec` per word position. Returns `None` when some position
/// has no term in the segment: the phrase can never match there, so the
/// caller serves an [`EmptyScorer`]. The term-list degrade engine uses this
/// after its bounded materialization step, avoiding another regex scan.
fn positional_postings(
    inverted: &InvertedIndexReader,
    position_terms: &[Vec<Term>],
) -> tantivy::Result<Option<Vec<Vec<SegmentPostings>>>> {
    let mut word_postings = Vec::with_capacity(position_terms.len());
    for terms in position_terms {
        let mut postings = Vec::new();
        for term in terms.iter() {
            if let Some(term_info) = inverted.get_term_info(term)? {
                postings.push(inverted.read_postings_from_terminfo(
                    &term_info,
                    IndexRecordOption::WithFreqsAndPositions,
                )?);
            }
        }
        if postings.is_empty() {
            return Ok(None);
        }
        word_postings.push(postings);
    }
    Ok(Some(word_postings))
}

/// A phrase query whose matches are verified position-by-position against
/// per-pair intermediate-word allowances. See the module docs.
#[derive(Debug)]
pub(crate) struct GapVerifiedPhraseQuery<A = TermDfa> {
    /// The recall-superset phrase query (slop = sum of `gaps`).
    inner: RegexPhraseQuery<A>,
    /// The field whose positional postings verify candidates — must be the
    /// same field `inner` runs against.
    field: Field,
    /// `gaps[i]` = allowed intermediate words between words `i` and `i+1`.
    gaps: Vec<u32>,
}

impl<A> Clone for GapVerifiedPhraseQuery<A> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            field: self.field,
            gaps: self.gaps.clone(),
        }
    }
}

impl<A> GapVerifiedPhraseQuery<A>
where
    A: Automaton + Send + Sync + 'static,
    A::State: Clone,
{
    pub(crate) fn new(inner: RegexPhraseQuery<A>, field: Field, gaps: Vec<u32>) -> Self {
        debug_assert_eq!(gaps.len() + 1, inner.phrase_terms().len());
        Self { inner, field, gaps }
    }
}

impl<A> Query for GapVerifiedPhraseQuery<A>
where
    A: Automaton + Send + Sync + std::fmt::Debug + 'static,
    A::State: Clone,
{
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let inner = self.inner.weight(enable_scoring)?;
        // Word automata: the same ones `inner` runs, one per word position.
        let regexes = self.inner.automata()?.to_vec();
        Ok(Box::new(GapVerifiedWeight {
            inner,
            field: self.field,
            regexes,
            gaps: self.gaps.clone(),
        }))
    }
}

struct GapVerifiedWeight<A> {
    inner: Box<dyn Weight>,
    field: Field,
    regexes: Vec<Arc<A>>,
    gaps: Vec<u32>,
}

impl<A> Weight for GapVerifiedWeight<A>
where
    A: Automaton + Send + Sync + 'static,
    A::State: Clone,
{
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let inner = self.inner.scorer(reader, boost)?;
        let inverted = reader.inverted_index(self.field)?;
        // The exact path deliberately keeps the historical verifier: it
        // rescans the joined automata per segment instead of relying on a
        // globally capped term cache. Tantivy's expansion limit is per
        // segment, whereas a global collection can incorrectly demote a
        // previously valid phrase to the flat-scoring degrade path.
        let mut word_postings = Vec::with_capacity(self.regexes.len());
        for regex in &self.regexes {
            let mut postings = Vec::new();
            let mut stream = inverted.terms().search(regex.as_ref()).into_stream()?;
            while stream.advance() {
                postings.push(inverted.read_postings_from_terminfo(
                    stream.value(),
                    IndexRecordOption::WithFreqsAndPositions,
                )?);
            }
            if postings.is_empty() {
                return Ok(Box::new(EmptyScorer));
            }
            word_postings.push(postings);
        }
        Ok(Box::new(GapVerifiedScorer::new(
            inner,
            word_postings,
            Proximity::Ordered(self.gaps.clone()),
            false,
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        self.inner.explain(reader, doc)
    }
}

/// A phrase query built from pre-materialized term sets — one per word
/// position — under a [`Proximity`]. See the module docs: this is the degrade
/// path for phrases whose expansions outgrow `RegexPhraseQuery`'s hard
/// `max_expansions` ceiling, and the only path of an any-order query.
#[derive(Clone, Debug)]
pub(crate) struct TermListPhraseQuery {
    /// The field whose positional postings verify candidates — the same
    /// field the terms were materialized from.
    field: Field,
    /// The index terms each word position matched (every position holds at
    /// least one term; an empty position means the caller should have built
    /// an `EmptyQuery` instead).
    position_terms: Vec<Vec<Term>>,
    proximity: Proximity,
}

impl TermListPhraseQuery {
    pub(crate) fn new(field: Field, position_terms: Vec<Vec<Term>>, proximity: Proximity) -> Self {
        if let Proximity::Ordered(gaps) = &proximity {
            debug_assert_eq!(gaps.len() + 1, position_terms.len());
        }
        debug_assert!(position_terms.iter().all(|terms| !terms.is_empty()));
        Self {
            field,
            position_terms,
            proximity,
        }
    }

    /// The candidate driver: a document can only contain the phrase if every
    /// position's term set matches it somewhere — a plain boolean AND of
    /// per-position `TermSetQuery`s, which never compiles a DFA and has no
    /// expansion ceiling. The verifier then trims this recall superset down
    /// to real occurrences under the proximity.
    fn driver(&self) -> BooleanQuery {
        let clauses: Vec<(Occur, Box<dyn Query>)> = self
            .position_terms
            .iter()
            .map(|terms| {
                (
                    Occur::Must,
                    Box::new(TermSetQuery::new(terms.iter().cloned())) as Box<dyn Query>,
                )
            })
            .collect();
        BooleanQuery::new(clauses)
    }
}

impl Query for TermListPhraseQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(TermListPhraseWeight {
            driver: self.driver().weight(enable_scoring)?,
            field: self.field,
            position_terms: self.position_terms.clone(),
            proximity: self.proximity.clone(),
            rank_span: enable_scoring.is_scoring_enabled(),
        }))
    }
}

struct TermListPhraseWeight {
    driver: Box<dyn Weight>,
    field: Field,
    position_terms: Vec<Vec<Term>>,
    proximity: Proximity,
    rank_span: bool,
}

impl Weight for TermListPhraseWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let inner = self.driver.scorer(reader, boost)?;
        let inverted = reader.inverted_index(self.field)?;
        // Positional postings bounded by the materialization budgets the
        // caller already enforced — the collection that produced
        // `position_terms` is the cost guard, not a second ceiling here.
        let Some(word_postings) = positional_postings(&inverted, &self.position_terms)? else {
            // A word with no matching term in this segment can never form a
            // phrase here (and the AND driver cannot match either).
            return Ok(Box::new(EmptyScorer));
        };
        Ok(Box::new(GapVerifiedScorer::new(
            inner,
            word_postings,
            self.proximity.clone(),
            self.rank_span,
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        self.driver.explain(reader, doc)
    }
}

/// Tiny term sets are cheaper to scan directly; beyond this crossover, the
// heap avoids touching the many cursors that do not occur in a candidate.
const LINEAR_POSTINGS_LIMIT: usize = 8;

/// One query word's positions, merged across every index term its pattern
/// matched, read document by document from forward-only positional cursors.
pub(crate) struct WordPositions {
    postings: Vec<SegmentPostings>,
    /// `(current doc, postings index)`, smallest doc on top. A word can carry
    /// thousands of terms; reading a doc touches only the postings that lag
    /// behind it or sit on it, instead of seeking every one of them.
    heap: BinaryHeap<Reverse<(DocId, usize)>>,
    /// Matches from the last doc stay outside the heap. Seeking these
    /// directly keeps dense term sets linear instead of repeatedly popping
    /// and reinserting each matching cursor.
    on_doc: Vec<usize>,
    pos_buf: Vec<u32>,
}

impl WordPositions {
    pub(crate) fn new(postings: Vec<SegmentPostings>) -> Self {
        let heap = if postings.len() <= LINEAR_POSTINGS_LIMIT {
            BinaryHeap::new()
        } else {
            postings
                .iter()
                .enumerate()
                .filter(|(_, p)| p.doc() != TERMINATED)
                .map(|(i, p)| Reverse((p.doc(), i)))
                .collect()
        };
        Self {
            postings,
            heap,
            on_doc: Vec::new(),
            pos_buf: Vec::new(),
        }
    }

    /// Replaces `out` with this word's positions in `doc`, sorted. Successive
    /// calls must not go back to an earlier doc: the cursors only seek forward.
    pub(crate) fn positions_in(&mut self, doc: DocId, out: &mut Vec<u32>) {
        out.clear();
        let postings = &mut self.postings;
        if postings.len() <= LINEAR_POSTINGS_LIMIT {
            for posting in postings {
                if posting.doc() < doc {
                    posting.seek(doc);
                }
                if posting.doc() == doc {
                    posting.positions(&mut self.pos_buf);
                    out.extend_from_slice(&self.pos_buf);
                }
            }
        } else {
            let heap = &mut self.heap;
            let pending = &mut self.on_doc;
            if pending.len() * 2 >= postings.len() {
                // Dense candidates favor a contiguous cursor sweep. Rebuild
                // only the future heap in linear time, rather than paying heap
                // operations or indirect indexing for most of the terms. A
                // sparse next doc immediately returns to the heap path.
                let mut future = std::mem::take(heap).into_vec();
                future.clear();
                pending.clear();
                for (i, posting) in postings.iter_mut().enumerate() {
                    let at = if posting.doc() < doc {
                        posting.seek(doc)
                    } else {
                        posting.doc()
                    };
                    if at == doc {
                        posting.positions(&mut self.pos_buf);
                        out.extend_from_slice(&self.pos_buf);
                        pending.push(i);
                    } else if at != TERMINATED {
                        future.push(Reverse((at, i)));
                    }
                }
                *heap = BinaryHeap::from(future);
            } else {
                // `retain` does not rewrite indices until one cursor leaves
                // the doc. Dense sets therefore take the same direct
                // seek/positions path without redundant index compaction.
                let pos_buf = &mut self.pos_buf;
                pending.retain(|&i| {
                    let at = postings[i].seek(doc);
                    if at == doc {
                        postings[i].positions(pos_buf);
                        out.extend_from_slice(pos_buf);
                        true
                    } else {
                        if at != TERMINATED {
                            heap.push(Reverse((at, i)));
                        }
                        false
                    }
                });
                while let Some(&Reverse((at, i))) = heap.peek() {
                    if at > doc {
                        break;
                    }
                    heap.pop();
                    let next = if at < doc { postings[i].seek(doc) } else { at };
                    if next == doc {
                        postings[i].positions(pos_buf);
                        out.extend_from_slice(pos_buf);
                        pending.push(i);
                    } else if next != TERMINATED {
                        heap.push(Reverse((next, i)));
                    }
                }
            }
        }
        out.sort_unstable();
    }
}

/// Forward feasibility sweep over in-order word occurrences: after
/// [`Self::start`] and one [`Self::extend`] per further word, [`Self::ends`]
/// holds every position at which a valid chain over those words can end.
/// Both lists are sorted, so each step is a linear two-pointer merge — no
/// backtracking, and (unlike a greedy earliest-position chain) no false
/// negatives when a later start is the only one whose window reaches the
/// next word.
#[derive(Default)]
pub(crate) struct ChainSweep {
    feasible: Vec<u32>,
    next: Vec<u32>,
}

impl ChainSweep {
    pub(crate) fn start(&mut self, first_word: &[u32]) {
        self.feasible.clear();
        self.feasible.extend_from_slice(first_word);
    }

    /// Keeps the positions of the next word reachable from a chain end with
    /// at most `gap` intermediate words; false when none is.
    pub(crate) fn extend(&mut self, positions: &[u32], gap: u32) -> bool {
        // q extends a chain iff some feasible p satisfies
        // q - gap - 1 <= p <= q - 1 (strictly after p, within the gap).
        let window = gap as u64 + 1;
        self.next.clear();
        let mut j = 0usize;
        for &q in positions {
            let lo = (q as u64).saturating_sub(window);
            while j < self.feasible.len() && (self.feasible[j] as u64) < lo {
                j += 1;
            }
            if j < self.feasible.len() && self.feasible[j] < q {
                self.next.push(q);
            }
        }
        std::mem::swap(&mut self.feasible, &mut self.next);
        !self.feasible.is_empty()
    }

    pub(crate) fn ends(&self) -> &[u32] {
        &self.feasible
    }

    /// Runs the whole chain; false as soon as a word cannot extend it.
    fn chain(&mut self, words: &[Vec<u32>], gaps: &[u32]) -> bool {
        let Some((first, rest)) = words.split_first() else {
            return false;
        };
        self.start(first);
        !self.feasible.is_empty()
            && rest
                .iter()
                .zip(gaps)
                .all(|(positions, &gap)| self.extend(positions, gap))
    }
}

/// The anchoring check of a phrase that ends a line: the fewest words between
/// a chain's last word and position `last` (the line's last content word),
/// when that is at most `max_slack`. `words` holds each word's sorted positions.
pub(crate) fn end_anchored_slack(
    sweep: &mut ChainSweep,
    words: &[Vec<u32>],
    gaps: &[u32],
    last: u32,
    max_slack: u32,
) -> Option<u32> {
    if !sweep.chain(words, gaps) {
        return None;
    }
    let end = sweep.ends().iter().rev().find(|&&end| end <= last)?;
    let slack = last - end;
    (slack <= max_slack).then_some(slack)
}

/// The anchoring check of a phrase that starts a line: the fewest words
/// between position `first` (the line's first content word) and a chain's first
/// word, when that is at most `max_slack`.
pub(crate) fn start_anchored_slack(
    sweep: &mut ChainSweep,
    words: &[Vec<u32>],
    gaps: &[u32],
    first: u32,
    max_slack: u32,
) -> Option<u32> {
    let (head, rest) = words.split_first()?;
    let window_end = first.saturating_add(max_slack);
    head.iter()
        .filter(|&&start| start >= first && start <= window_end)
        .find(|&&start| {
            sweep.start(&[start]);
            rest.iter()
                .zip(gaps)
                .all(|(positions, &gap)| sweep.extend(positions, gap))
        })
        .map(|&start| start - first)
}

/// How the words of a multi-word query must lie relative to each other.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Proximity {
    /// In query order; `gaps[i]` = allowed intermediate words between words
    /// `i` and `i+1`.
    Ordered(Vec<u32>),
    /// In any order, each word at its own position, the last position at most
    /// `max_span` after the first.
    AnyOrder { max_span: u32 },
}

impl Proximity {
    /// The span an ordered match of `word_count` words, with at most `distance`
    /// words between each pair, can take — so any order never loses an ordered hit.
    pub(crate) fn any_order(word_count: usize, distance: u32) -> Self {
        let pairs = u32::try_from(word_count.saturating_sub(1)).unwrap_or(u32::MAX);
        Self::AnyOrder {
            max_span: pairs.saturating_mul(distance.saturating_add(1)),
        }
    }

    /// Strictly adjacent, in order.
    pub(crate) fn adjacent(word_count: usize) -> Self {
        Self::Ordered(vec![0; word_count.saturating_sub(1)])
    }

    pub(crate) fn is_any_order(&self) -> bool {
        matches!(self, Self::AnyOrder { .. })
    }
}

/// Windows of word occurrences for [`Proximity::AnyOrder`]. Fed in increasing
/// position order with the query words each position can fill; finds windows
/// where every word gets a distinct position. Word sets are lists, not bit
/// masks, so any word count works.
#[derive(Default)]
pub(crate) struct WordWindows {
    word_count: usize,
    positions: Vec<u32>,
    /// Entry `e` holds `words[starts[e]..starts[e + 1]]`.
    starts: Vec<usize>,
    words: Vec<usize>,
    coverage: Vec<u32>,
    adjacency: Vec<Vec<usize>>,
    owner: Vec<usize>,
    visited: Vec<bool>,
    stack: Vec<(usize, usize)>,
}

const UNMATCHED: usize = usize::MAX;

impl WordWindows {
    pub(crate) fn reset(&mut self, word_count: usize) {
        self.word_count = word_count;
        self.positions.clear();
        self.starts.clear();
        self.starts.push(0);
        self.words.clear();
    }

    /// Records that `word` can fill `position`, which must not precede the
    /// previous one. Returns the new slot (see [`Self::slot`]), `None` for a repeat.
    pub(crate) fn push(&mut self, position: u32, word: usize) -> Option<usize> {
        debug_assert!(word < self.word_count);
        if self.positions.last() != Some(&position) {
            debug_assert!(self.positions.last().is_none_or(|&last| last < position));
            self.positions.push(position);
            self.starts.push(self.words.len());
        }
        let begin = self.starts[self.starts.len() - 2];
        if self.words[begin..].contains(&word) {
            return None;
        }
        self.words.push(word);
        *self.starts.last_mut().unwrap() = self.words.len();
        Some(self.words.len() - 1)
    }

    /// The slot [`Self::push`] returned for `word` at entry `entry`.
    pub(crate) fn slot(&self, entry: usize, word: usize) -> usize {
        let begin = self.starts[entry];
        begin
            + self.words[begin..self.starts[entry + 1]]
                .iter()
                .position(|&w| w == word)
                .expect("an assigned word fills its entry")
    }

    /// Last minus first position of an assignment from [`Self::next_window`].
    pub(crate) fn assigned_span(&self, assignment: &[usize]) -> u32 {
        let first = assignment.iter().min().copied().unwrap_or(0);
        let last = assignment.iter().max().copied().unwrap_or(0);
        self.positions[last] - self.positions[first]
    }

    /// The window ending earliest, among entries `from..`, whose positions span
    /// at most `max_span` and give every word a distinct entry. On success
    /// `assignment[w]` is word `w`'s entry (the latest ones are preferred, for a
    /// tight occurrence) and the window's last entry is returned.
    pub(crate) fn next_window(
        &mut self,
        from: usize,
        max_span: u32,
        assignment: &mut Vec<usize>,
    ) -> Option<usize> {
        let k = self.word_count;
        if k == 0 {
            return None;
        }
        self.coverage.clear();
        self.coverage.resize(k, 0);
        let mut covered = 0;
        // Entries several words can fill; without any, coverage implies a matching.
        let mut shared = 0;
        let mut left = from;
        for right in from..self.positions.len() {
            let entry = &self.words[self.starts[right]..self.starts[right + 1]];
            shared += usize::from(entry.len() > 1);
            for &w in entry {
                covered += usize::from(self.coverage[w] == 0);
                self.coverage[w] += 1;
            }
            while self.positions[right] - self.positions[left] > max_span {
                let entry = &self.words[self.starts[left]..self.starts[left + 1]];
                shared -= usize::from(entry.len() > 1);
                for &w in entry {
                    self.coverage[w] -= 1;
                    covered -= usize::from(self.coverage[w] == 0);
                }
                left += 1;
            }
            if covered < k || right + 1 - left < k {
                continue;
            }
            if shared == 0 {
                assignment.clear();
                assignment.resize(k, UNMATCHED);
                for e in (left..=right).rev() {
                    let w = self.words[self.starts[e]];
                    if assignment[w] == UNMATCHED {
                        assignment[w] = e;
                    }
                }
                return Some(right);
            }
            if self.match_window(left, right, assignment) {
                return Some(right);
            }
        }
        None
    }

    /// Kuhn's bipartite matching of the words to the entries `left..=right`.
    fn match_window(&mut self, left: usize, right: usize, assignment: &mut Vec<usize>) -> bool {
        let k = self.word_count;
        let n = right + 1 - left;
        self.adjacency.resize_with(k, Vec::new);
        self.adjacency.iter_mut().for_each(Vec::clear);
        for e in (left..=right).rev() {
            for &w in &self.words[self.starts[e]..self.starts[e + 1]] {
                self.adjacency[w].push(e - left);
            }
        }
        self.owner.clear();
        self.owner.resize(n, UNMATCHED);
        for w in 0..k {
            self.visited.clear();
            self.visited.resize(n, false);
            if !self.augment(w) {
                return false;
            }
        }
        assignment.clear();
        assignment.resize(k, UNMATCHED);
        for (i, &w) in self.owner.iter().enumerate() {
            if w != UNMATCHED {
                assignment[w] = left + i;
            }
        }
        true
    }

    /// An augmenting path from `start`; iterative, so a long query cannot
    /// exhaust the stack. Each frame takes the entry it tried last.
    fn augment(&mut self, start: usize) -> bool {
        self.stack.clear();
        self.stack.push((start, 0));
        while let Some(&(w, i)) = self.stack.last() {
            let Some(&e) = self.adjacency[w].get(i) else {
                self.stack.pop();
                continue;
            };
            self.stack.last_mut().unwrap().1 += 1;
            if self.visited[e] {
                continue;
            }
            self.visited[e] = true;
            let holder = self.owner[e];
            if holder == UNMATCHED {
                for &(word, next) in &self.stack {
                    self.owner[self.adjacency[word][next - 1]] = word;
                }
                return true;
            }
            self.stack.push((holder, 0));
        }
        false
    }
}

struct GapVerifiedScorer {
    inner: Box<dyn Scorer>,
    words: Vec<WordPositions>,
    proximity: Proximity,
    // Reused scratch buffers (verification runs per candidate doc).
    cur_positions: Vec<u32>,
    sweep: ChainSweep,
    word_positions: Vec<Vec<u32>>,
    merge: BinaryHeap<Reverse<(u32, usize, usize)>>,
    windows: WordWindows,
    assignment: Vec<usize>,
    /// Any order under scoring: the tightest window's span lifts the score.
    rank_span: bool,
    span_bonus: Score,
}

impl GapVerifiedScorer {
    fn new(
        inner: Box<dyn Scorer>,
        word_postings: Vec<Vec<SegmentPostings>>,
        proximity: Proximity,
        rank_span: bool,
    ) -> Self {
        let word_count = word_postings.len();
        let proximity_is_any_order = proximity.is_any_order();
        let mut scorer = Self {
            inner,
            words: word_postings.into_iter().map(WordPositions::new).collect(),
            proximity,
            cur_positions: Vec::new(),
            sweep: ChainSweep::default(),
            word_positions: vec![Vec::new(); word_count],
            merge: BinaryHeap::new(),
            windows: WordWindows::default(),
            assignment: Vec::new(),
            rank_span: rank_span && proximity_is_any_order,
            span_bonus: 0.0,
        };
        // A freshly built DocSet must already sit on its first matching doc.
        let mut doc = scorer.inner.doc();
        while doc != TERMINATED && !scorer.verify(doc) {
            doc = scorer.inner.advance();
        }
        scorer
    }

    /// Does `doc` satisfy the proximity? The inner scorer emits docs in
    /// increasing order, so the word cursors only seek forward.
    fn verify(&mut self, doc: DocId) -> bool {
        match self.proximity {
            Proximity::Ordered(_) => self.verify_ordered(doc),
            Proximity::AnyOrder { max_span } => self.verify_any_order(doc, max_span),
        }
    }

    /// Positions `p0 < p1 < … < p_{k-1}` (one per word) with
    /// `p_{i+1} - p_i - 1 <= gaps[i]` for every pair.
    fn verify_ordered(&mut self, doc: DocId) -> bool {
        let Proximity::Ordered(gaps) = &self.proximity else {
            return false;
        };
        for w in 0..self.words.len() {
            self.words[w].positions_in(doc, &mut self.cur_positions);
            if self.cur_positions.is_empty() {
                return false;
            }
            if w == 0 {
                self.sweep.start(&self.cur_positions);
            } else if !self.sweep.extend(&self.cur_positions, gaps[w - 1]) {
                return false;
            }
        }
        true
    }

    /// Distinct positions, one per word, within one `max_span` window.
    fn verify_any_order(&mut self, doc: DocId, max_span: u32) -> bool {
        for (word, positions) in self.words.iter_mut().zip(&mut self.word_positions) {
            word.positions_in(doc, positions);
            if positions.is_empty() {
                return false;
            }
        }
        // k-way merge: the windows take positions in increasing order.
        self.windows.reset(self.words.len());
        self.merge.clear();
        for (w, positions) in self.word_positions.iter().enumerate() {
            self.merge.push(Reverse((positions[0], w, 0)));
        }
        while let Some(Reverse((position, w, i))) = self.merge.pop() {
            self.windows.push(position, w);
            if let Some(&next) = self.word_positions[w].get(i + 1) {
                self.merge.push(Reverse((next, w, i + 1)));
            }
        }
        let Some(_) = self.windows.next_window(0, max_span, &mut self.assignment) else {
            return false;
        };
        if self.rank_span {
            let mut span = self.windows.assigned_span(&self.assignment);
            // חיפוש בינארי על הרוחב כולל חלונות חופפים בלי לסרוק מחדש מכל התחלה.
            let mut lower = (self.words.len() - 1) as u32;
            while lower < span {
                let middle = lower + (span - lower) / 2;
                if self
                    .windows
                    .next_window(0, middle, &mut self.assignment)
                    .is_some()
                {
                    span = self.windows.assigned_span(&self.assignment);
                } else {
                    lower = middle + 1;
                }
            }
            self.span_bonus = 1.0 / (1.0 + span as Score);
        }
        true
    }
}

impl DocSet for GapVerifiedScorer {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.inner.advance();
            if doc == TERMINATED || self.verify(doc) {
                return doc;
            }
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        // Already positioned on a verified doc at or past the target
        // (re-verifying it would re-read positions the postings cursors have
        // already stepped past).
        if self.inner.doc() >= target {
            return self.inner.doc();
        }
        let mut doc = self.inner.seek(target);
        while doc != TERMINATED && !self.verify(doc) {
            doc = self.inner.advance();
        }
        doc
    }

    fn doc(&self) -> DocId {
        self.inner.doc()
    }

    fn size_hint(&self) -> u32 {
        // Upper bound: verification only removes docs.
        self.inner.size_hint()
    }
}

impl Scorer for GapVerifiedScorer {
    fn score(&mut self) -> Score {
        self.inner.score() + self.span_bonus
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::merge_policy::NoMergePolicy;
    use tantivy::schema::{Schema, INDEXED, TEXT};
    use tantivy::TantivyDocument;

    struct Candidates {
        docs: Vec<DocId>,
        cursor: usize,
    }

    impl DocSet for Candidates {
        fn advance(&mut self) -> DocId {
            self.cursor = (self.cursor + 1).min(self.docs.len());
            self.doc()
        }

        fn seek(&mut self, target: DocId) -> DocId {
            self.cursor += self.docs[self.cursor..].partition_point(|&doc| doc < target);
            self.doc()
        }

        fn doc(&self) -> DocId {
            self.docs.get(self.cursor).copied().unwrap_or(TERMINATED)
        }

        fn size_hint(&self) -> u32 {
            self.docs.len() as u32
        }
    }

    impl Scorer for Candidates {
        fn score(&mut self) -> Score {
            2.75 + self.doc() as Score * 0.001
        }
    }

    // Exhaustively try every valid next occurrence. This deliberately does
    // not use postings cursors or the verifier's feasibility sweep.
    fn contains_phrase(tokens: &[&str], words: &[Vec<&str>], gaps: &[u32]) -> bool {
        fn extend(
            tokens: &[&str],
            words: &[Vec<&str>],
            gaps: &[u32],
            word: usize,
            previous: Option<usize>,
        ) -> bool {
            if word == words.len() {
                return true;
            }
            tokens.iter().enumerate().any(|(position, token)| {
                words[word].contains(token)
                    && previous.is_none_or(|previous| {
                        position > previous
                            && (position - previous - 1) as u64 <= gaps[word - 1] as u64
                    })
                    && extend(tokens, words, gaps, word + 1, Some(position))
            })
        }
        extend(tokens, words, gaps, 0, None)
    }

    #[test]
    fn anchored_slack_measures_the_words_to_the_line_edge() {
        let mut sweep = ChainSweep::default();
        // Words at positions: a {1, 6}, b {2, 7, 9}; gaps between a and b.
        let words = [vec![1, 6], vec![2, 7, 9]];
        assert_eq!(end_anchored_slack(&mut sweep, &words, &[0], 7, 0), Some(0));
        assert_eq!(end_anchored_slack(&mut sweep, &words, &[0], 8, 0), None);
        assert_eq!(end_anchored_slack(&mut sweep, &words, &[0], 8, 1), Some(1));
        // `last` before every chain end: no chain ends on the line's content.
        assert_eq!(end_anchored_slack(&mut sweep, &words, &[2], 1, 9), None);
        assert_eq!(end_anchored_slack(&mut sweep, &words, &[2], 9, 0), Some(0));
        assert_eq!(
            start_anchored_slack(&mut sweep, &words, &[0], 1, 0),
            Some(0)
        );
        assert_eq!(start_anchored_slack(&mut sweep, &words, &[0], 0, 0), None);
        assert_eq!(
            start_anchored_slack(&mut sweep, &words, &[0], 0, 1),
            Some(1)
        );
        // The nearest start whose chain completes wins, not the nearest occurrence.
        let words = [vec![1, 3], vec![9]];
        assert_eq!(
            start_anchored_slack(&mut sweep, &words, &[5], 1, 4),
            Some(2)
        );
        assert_eq!(start_anchored_slack(&mut sweep, &words, &[5], 1, 1), None);
    }

    // Every injective assignment of words to positions inside one window.
    // Deliberately free of the windows' sliding and matching.
    fn contains_any_order(tokens: &[&str], words: &[Vec<&str>], max_span: u32) -> bool {
        fn assign(
            tokens: &[&str],
            words: &[Vec<&str>],
            max_span: u32,
            used: &mut Vec<usize>,
        ) -> bool {
            let word = used.len();
            if word == words.len() {
                let low = used.iter().min().unwrap();
                let high = used.iter().max().unwrap();
                return (high - low) as u64 <= max_span as u64;
            }
            (0..tokens.len()).any(|position| {
                if used.contains(&position) || !words[word].contains(&tokens[position]) {
                    return false;
                }
                used.push(position);
                let found = assign(tokens, words, max_span, used);
                used.pop();
                found
            })
        }
        assign(tokens, words, max_span, &mut Vec::new())
    }

    const ALPHABET: [&str; 29] = [
        "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "b0", "b1", "b2", "b3", "b4", "b5",
        "b6", "b7", "b8", "c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "shared", "filler",
    ];

    /// One segment over fixed and seeded documents; doc 55 is deleted.
    fn oracle_index() -> (tantivy::Index, Field, Vec<Vec<&'static str>>) {
        let mut schema = Schema::builder();
        let text = schema.add_text_field("text", TEXT);
        let id = schema.add_u64_field("id", INDEXED);
        let index = tantivy::Index::create_in_ram(schema.build());
        let mut writer = index
            .writer_with_num_threads::<TantivyDocument>(1, 50_000_000)
            .unwrap();
        writer.set_merge_policy(Box::new(NoMergePolicy));
        let mut documents = vec![
            vec![],
            vec!["a0", "filler", "b0", "c0"],
            vec!["a0", "b0", "filler", "c0"],
            vec!["shared", "shared", "shared"],
            vec!["a0", "b0", "c0"],
            vec!["c0", "b0", "a0"],
        ];
        let dense: Vec<_> = ALPHABET
            .iter()
            .copied()
            .filter(|&token| token != "filler")
            .collect();
        documents.extend([dense.clone(), dense, vec!["a0", "b0", "c0"], vec![]]);
        let mut seed = 0x97a23b5du64;
        for _ in 0..256 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let length = (seed >> 32) as usize % 24;
            documents.push(
                (0..length)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        ALPHABET[(seed >> 32) as usize % ALPHABET.len()]
                    })
                    .collect(),
            );
        }
        documents[55] = vec!["deleted", "b0", "c0"];
        for (doc, tokens) in documents.iter().enumerate() {
            writer
                .add_document(tantivy::doc!(text => tokens.join(" "), id => doc as u64))
                .unwrap();
        }
        writer.commit().unwrap();
        writer.delete_term(Term::from_field_u64(id, 55));
        writer.commit().unwrap();
        (index, text, documents)
    }

    /// Drives the scorer with advances and seeks over strided candidates and
    /// compares every step with `oracle`.
    fn assert_scorer_matches_oracle(
        segment: &SegmentReader,
        text: Field,
        words: &[Vec<&str>],
        proximity: &Proximity,
        oracle: &dyn Fn(&[&str]) -> bool,
        documents: &[Vec<&str>],
    ) {
        let inverted = segment.inverted_index(text).unwrap();
        let terms: Vec<Vec<_>> = words
            .iter()
            .map(|word| {
                word.iter()
                    .map(|term| Term::from_field_text(text, term))
                    .collect()
            })
            .collect();
        for stride in [1, 2, 7] {
            let candidates: Vec<_> = (0..segment.max_doc())
                .filter(|&doc| doc != 55 && doc % stride == 0)
                .collect();
            let expected: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|&doc| oracle(&documents[doc as usize]))
                .collect();
            let postings = positional_postings(&inverted, &terms).unwrap().unwrap();
            let mut scorer = GapVerifiedScorer::new(
                Box::new(Candidates {
                    docs: candidates,
                    cursor: 0,
                }),
                postings,
                proximity.clone(),
                false,
            );
            let mut expected_cursor = 0;
            let mut step = 0;
            loop {
                let want = expected.get(expected_cursor).copied().unwrap_or(TERMINATED);
                assert_eq!(
                    scorer.doc(),
                    want,
                    "{proximity:?} words={words:?} stride={stride} step={step}"
                );
                if want == TERMINATED {
                    break;
                }
                assert_eq!(scorer.score(), 2.75 + want as Score * 0.001);
                // Repeated seeks to/before an already verified doc must
                // neither reread positions nor change its score.
                assert_eq!(scorer.seek(want), want);
                assert_eq!(scorer.seek(want.saturating_sub(1)), want);
                if step % 3 == 0 {
                    let target = want + 5;
                    expected_cursor +=
                        expected[expected_cursor..].partition_point(|&doc| doc < target);
                    assert_eq!(
                        scorer.seek(target),
                        expected.get(expected_cursor).copied().unwrap_or(TERMINATED)
                    );
                } else {
                    expected_cursor += 1;
                    assert_eq!(
                        scorer.advance(),
                        expected.get(expected_cursor).copied().unwrap_or(TERMINATED)
                    );
                }
                step += 1;
            }
            assert_eq!(scorer.seek(TERMINATED), TERMINATED);
        }
    }

    fn oracle_word_sets() -> Vec<Vec<Vec<&'static str>>> {
        // Direct cursors, heaps, and a mixture within one phrase.
        vec![
            vec![
                vec!["a0", "a1", "shared", "deleted", "missing"],
                vec!["b0", "b1", "shared"],
                vec!["c0", "c1", "shared"],
            ],
            vec![
                vec![
                    "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "shared", "deleted",
                    "missing",
                ],
                vec![
                    "b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7", "b8", "shared",
                ],
                vec![
                    "c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "shared",
                ],
            ],
            vec![
                vec![
                    "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "shared", "deleted",
                    "missing",
                ],
                vec!["b0", "b1", "shared"],
                vec!["c0", "c1", "shared"],
            ],
            vec![vec!["deleted"], vec!["b0"], vec!["c0"]],
        ]
    }

    #[test]
    fn phrase_cursor_matches_exhaustive_oracle_with_seeks_and_deletes() {
        let (index, text, documents) = oracle_index();
        let searcher = index.reader().unwrap().searcher();
        assert_eq!(searcher.segment_readers().len(), 1);
        let segment = &searcher.segment_readers()[0];
        let inverted = segment.inverted_index(text).unwrap();
        assert!(
            positional_postings(&inverted, &[vec![Term::from_field_text(text, "missing")]])
                .unwrap()
                .is_none()
        );
        for words in oracle_word_sets() {
            for gaps in [
                vec![0, 0],
                vec![2, 0],
                vec![0, 2],
                vec![4, 7],
                vec![u32::MAX, u32::MAX],
            ] {
                assert_scorer_matches_oracle(
                    segment,
                    text,
                    &words,
                    &Proximity::Ordered(gaps.clone()),
                    &|tokens| contains_phrase(tokens, &words, &gaps),
                    &documents,
                );
            }
        }
    }

    #[test]
    fn any_order_cursor_matches_exhaustive_oracle_with_seeks_and_deletes() {
        let (index, text, documents) = oracle_index();
        let searcher = index.reader().unwrap().searcher();
        let segment = &searcher.segment_readers()[0];
        let mut word_sets = oracle_word_sets();
        // Overlapping sets need distinct positions: a repeated word, and a
        // word whose set contains the others'.
        word_sets.push(vec![vec!["a0", "a1"], vec!["a0", "a1"]]);
        word_sets.push(vec![
            vec!["a0", "shared"],
            vec!["a0", "shared"],
            vec!["shared"],
        ]);
        word_sets.push(vec![vec!["c0", "b0", "a0"], vec!["a0"], vec!["b0", "a0"]]);
        for words in word_sets {
            for max_span in [0, 1, 2, 3, 5, 9, u32::MAX] {
                assert_scorer_matches_oracle(
                    segment,
                    text,
                    &words,
                    &Proximity::AnyOrder { max_span },
                    &|tokens| contains_any_order(tokens, &words, max_span),
                    &documents,
                );
            }
        }
    }

    #[test]
    fn any_order_scores_use_the_minimum_span_of_every_assignment() {
        let (index, text, documents) = oracle_index();
        let searcher = index.reader().unwrap().searcher();
        let segment = &searcher.segment_readers()[0];
        let inverted = segment.inverted_index(text).unwrap();
        let mut cases = oracle_word_sets();
        cases.extend([
            vec![vec!["a0", "shared"], vec!["b0", "shared"]],
            vec![
                vec!["a0", "shared"],
                vec!["a0", "shared"],
                vec!["b0", "shared"],
            ],
            vec![vec!["shared"]],
        ]);
        for words in cases {
            let terms: Vec<Vec<Term>> = words
                .iter()
                .map(|word| {
                    word.iter()
                        .map(|term| Term::from_field_text(text, term))
                        .collect()
                })
                .collect();
            for max_span in [0, 1, 2, 5, u32::MAX] {
                let candidates: Vec<_> = (0..segment.max_doc())
                    .filter(|&doc| !segment.is_deleted(doc))
                    .collect();
                let expected: Vec<_> = candidates
                    .iter()
                    .filter_map(|&doc| {
                        let tokens = &documents[doc as usize];
                        (0..tokens.len() as u32)
                            .take_while(|&span| span <= max_span)
                            .find(|&span| contains_any_order(tokens, &words, span))
                            .map(|span| (doc, span))
                    })
                    .collect();
                let postings = positional_postings(&inverted, &terms).unwrap().unwrap();
                let mut scorer = GapVerifiedScorer::new(
                    Box::new(Candidates {
                        docs: candidates,
                        cursor: 0,
                    }),
                    postings,
                    Proximity::AnyOrder { max_span },
                    true,
                );
                for (doc, span) in expected {
                    assert_eq!(scorer.doc(), doc, "{words:?} max_span={max_span}");
                    let expected_score = 2.75 + doc as Score * 0.001 + 1.0 / (1.0 + span as Score);
                    assert!(
                        (scorer.score() - expected_score).abs() < 0.000001,
                        "{words:?} doc={doc} max_span={max_span} minimum={span} actual={}",
                        scorer.score()
                    );
                    scorer.advance();
                }
                assert_eq!(scorer.doc(), TERMINATED);
            }
        }
    }

    #[test]
    fn word_windows_match_exhaustive_assignment_on_random_overlaps() {
        let mut seed = 0x2545f491u64;
        let mut next = |bound: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % bound
        };
        let mut windows = WordWindows::default();
        let mut assignment = Vec::new();
        for _ in 0..4000 {
            let k = 1 + next(4);
            let length = next(10);
            // Each position holds a random subset of the words.
            let sets: Vec<Vec<usize>> = (0..length)
                .map(|_| (0..k).filter(|_| next(3) == 0).collect())
                .collect();
            let max_span = next(6) as u32;
            let names: Vec<String> = (0..length).map(|p| format!("p{p}")).collect();
            let words: Vec<Vec<&str>> = (0..k)
                .map(|w| {
                    (0..length)
                        .filter(|&p| sets[p].contains(&w))
                        .map(|p| names[p].as_str())
                        .collect()
                })
                .collect();
            let tokens: Vec<&str> = names.iter().map(String::as_str).collect();
            windows.reset(k);
            for (position, set) in sets.iter().enumerate() {
                for &w in set {
                    windows.push(position as u32, w);
                }
            }
            let found = windows.next_window(0, max_span, &mut assignment);
            assert_eq!(
                found.is_some(),
                contains_any_order(&tokens, &words, max_span),
                "{sets:?} span={max_span}"
            );
            if let Some(last) = found {
                let mut entries = assignment.clone();
                entries.sort_unstable();
                entries.dedup();
                assert_eq!(entries.len(), k, "distinct entries: {sets:?}");
                assert_eq!(*entries.last().unwrap(), last);
                let span = windows.positions[last] - windows.positions[entries[0]];
                assert!(span <= max_span);
                for (w, &e) in assignment.iter().enumerate() {
                    assert!(sets[windows.positions[e] as usize].contains(&w));
                }
            }
        }
    }

    #[test]
    fn every_ordered_match_is_an_any_order_match() {
        let alphabet = ["a", "b", "c", "x"];
        let mut seed = 0x5bd1e995u64;
        let mut next = |bound: usize| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize % bound
        };
        let mut windows = WordWindows::default();
        let mut assignment = Vec::new();
        let mut ordered_hits = 0;
        for _ in 0..3000 {
            let k = 2 + next(3);
            let distance = next(4) as u32;
            let tokens: Vec<&str> = (0..next(14)).map(|_| alphabet[next(4)]).collect();
            // Overlapping sets too, so repeated words need distinct positions.
            let words: Vec<Vec<&str>> = (0..k)
                .map(|_| {
                    let mut set = vec![alphabet[next(3)]];
                    if next(3) == 0 {
                        set.push(alphabet[next(3)]);
                    }
                    set
                })
                .collect();
            if !contains_phrase(&tokens, &words, &vec![distance; k - 1]) {
                continue;
            }
            ordered_hits += 1;
            let Proximity::AnyOrder { max_span } = Proximity::any_order(k, distance) else {
                unreachable!()
            };
            windows.reset(k);
            for (position, token) in tokens.iter().enumerate() {
                for (w, set) in words.iter().enumerate() {
                    if set.contains(token) {
                        windows.push(position as u32, w);
                    }
                }
            }
            assert!(
                windows.next_window(0, max_span, &mut assignment).is_some(),
                "{tokens:?} {words:?} distance={distance}"
            );
        }
        assert!(ordered_hits > 100);
    }

    #[test]
    fn word_windows_handle_more_than_64_words() {
        let k = 100;
        let mut windows = WordWindows::default();
        let mut assignment = Vec::new();
        // Every position can fill every word: only the window size decides.
        windows.reset(k);
        for position in 0..150u32 {
            for w in 0..k {
                windows.push(position * 2, w);
            }
        }
        assert_eq!(windows.next_window(0, 197, &mut assignment), None);
        assert_eq!(windows.next_window(0, 198, &mut assignment), Some(99));
        // Then each position fills exactly one word, in reverse order.
        windows.reset(k);
        for w in (0..k).rev() {
            windows.push((k - 1 - w) as u32, w);
        }
        assert_eq!(windows.next_window(0, 98, &mut assignment), None);
        assert_eq!(windows.next_window(0, 99, &mut assignment), Some(99));
        assert_eq!(assignment[0], 99);
    }
}
