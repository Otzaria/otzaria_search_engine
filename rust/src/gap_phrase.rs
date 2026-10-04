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
#[derive(Clone, Debug)]
pub(crate) struct GapVerifiedPhraseQuery {
    /// The recall-superset phrase query (slop = sum of `gaps`).
    inner: RegexPhraseQuery,
    /// The field whose positional postings verify candidates — must be the
    /// same field `inner` runs against.
    field: Field,
    /// `gaps[i]` = allowed intermediate words between words `i` and `i+1`.
    gaps: Vec<u32>,
}

impl GapVerifiedPhraseQuery {
    pub(crate) fn new(inner: RegexPhraseQuery, field: Field, gaps: Vec<u32>) -> Self {
        debug_assert_eq!(gaps.len() + 1, inner.phrase_terms().len());
        Self { inner, field, gaps }
    }
}

impl Query for GapVerifiedPhraseQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let inner = self.inner.weight(enable_scoring)?;
        // Word regexes: the same automata `inner` compiled, one per word position.
        let regexes = self.inner.regexes()?.to_vec();
        Ok(Box::new(GapVerifiedWeight {
            inner,
            field: self.field,
            regexes,
            gaps: self.gaps.clone(),
        }))
    }
}

struct GapVerifiedWeight {
    inner: Box<dyn Weight>,
    field: Field,
    regexes: Vec<Arc<tantivy_fst::Regex>>,
    gaps: Vec<u32>,
}

impl Weight for GapVerifiedWeight {
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
            self.gaps.clone(),
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        self.inner.explain(reader, doc)
    }
}

/// A phrase query built from pre-materialized term sets — one per word
/// position — with per-pair gap allowances. See the module docs: this is the
/// degrade path for phrases whose expansions outgrow `RegexPhraseQuery`'s
/// hard `max_expansions` ceiling.
#[derive(Clone, Debug)]
pub(crate) struct TermListPhraseQuery {
    /// The field whose positional postings verify candidates — the same
    /// field the terms were materialized from.
    field: Field,
    /// The index terms each word position matched (every position holds at
    /// least one term; an empty position means the caller should have built
    /// an `EmptyQuery` instead).
    position_terms: Vec<Vec<Term>>,
    /// `gaps[i]` = allowed intermediate words between words `i` and `i+1`.
    gaps: Vec<u32>,
}

impl TermListPhraseQuery {
    pub(crate) fn new(field: Field, position_terms: Vec<Vec<Term>>, gaps: Vec<u32>) -> Self {
        debug_assert_eq!(gaps.len() + 1, position_terms.len());
        debug_assert!(position_terms.iter().all(|terms| !terms.is_empty()));
        Self {
            field,
            position_terms,
            gaps,
        }
    }

    /// The candidate driver: a document can only contain the phrase if every
    /// position's term set matches it somewhere — a plain boolean AND of
    /// per-position `TermSetQuery`s, which never compiles a DFA and has no
    /// expansion ceiling. The verifier then trims this recall superset down
    /// to real in-order, within-allowance occurrences.
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
            gaps: self.gaps.clone(),
        }))
    }
}

struct TermListPhraseWeight {
    driver: Box<dyn Weight>,
    field: Field,
    position_terms: Vec<Vec<Term>>,
    gaps: Vec<u32>,
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
            self.gaps.clone(),
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

struct GapVerifiedScorer {
    inner: Box<dyn Scorer>,
    words: Vec<WordPositions>,
    gaps: Vec<u32>,
    // Reused scratch buffers (verification runs per candidate doc).
    cur_positions: Vec<u32>,
    sweep: ChainSweep,
}

impl GapVerifiedScorer {
    fn new(
        inner: Box<dyn Scorer>,
        word_postings: Vec<Vec<SegmentPostings>>,
        gaps: Vec<u32>,
    ) -> Self {
        let mut scorer = Self {
            inner,
            words: word_postings.into_iter().map(WordPositions::new).collect(),
            gaps,
            cur_positions: Vec::new(),
            sweep: ChainSweep::default(),
        };
        // A freshly built DocSet must already sit on its first matching doc.
        let mut doc = scorer.inner.doc();
        while doc != TERMINATED && !scorer.verify(doc) {
            doc = scorer.inner.advance();
        }
        scorer
    }

    /// Does `doc` contain positions `p0 < p1 < … < p_{k-1}` (one per word)
    /// with `p_{i+1} - p_i - 1 <= gaps[i]` for every pair? The inner scorer
    /// emits docs in increasing order, so the word cursors only seek forward.
    fn verify(&mut self, doc: DocId) -> bool {
        for w in 0..self.words.len() {
            self.words[w].positions_in(doc, &mut self.cur_positions);
            if self.cur_positions.is_empty() {
                return false;
            }
            if w == 0 {
                self.sweep.start(&self.cur_positions);
            } else if !self.sweep.extend(&self.cur_positions, self.gaps[w - 1]) {
                return false;
            }
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
        self.inner.score()
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

    #[test]
    fn phrase_cursor_matches_exhaustive_oracle_with_seeks_and_deletes() {
        let mut schema = Schema::builder();
        let text = schema.add_text_field("text", TEXT);
        let id = schema.add_u64_field("id", INDEXED);
        let index = tantivy::Index::create_in_ram(schema.build());
        let mut writer = index
            .writer_with_num_threads::<TantivyDocument>(1, 50_000_000)
            .unwrap();
        writer.set_merge_policy(Box::new(NoMergePolicy));
        let alphabet = [
            "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "b0", "b1", "b2", "b3", "b4",
            "b5", "b6", "b7", "b8", "c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "shared",
            "filler",
        ];
        let mut documents = vec![
            vec![],
            vec!["a0", "filler", "b0", "c0"],
            vec!["a0", "b0", "filler", "c0"],
            vec!["shared", "shared", "shared"],
            vec!["a0", "b0", "c0"],
            vec!["c0", "b0", "a0"],
        ];
        let dense: Vec<_> = alphabet
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
                        alphabet[(seed >> 32) as usize % alphabet.len()]
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
        let reader = index.reader().unwrap();
        let searcher = reader.searcher();
        assert_eq!(searcher.segment_readers().len(), 1);
        let segment = &searcher.segment_readers()[0];
        let inverted = segment.inverted_index(text).unwrap();
        // Exercise direct cursors, heaps, and a mixture within one phrase.
        for words in [
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
        ] {
            let terms: Vec<Vec<_>> = words
                .iter()
                .map(|word| {
                    word.iter()
                        .map(|term| Term::from_field_text(text, term))
                        .collect()
                })
                .collect();
            assert!(positional_postings(
                &inverted,
                &[vec![Term::from_field_text(text, "missing")]]
            )
            .unwrap()
            .is_none());
            for gaps in [
                vec![0, 0],
                vec![2, 0],
                vec![0, 2],
                vec![4, 7],
                vec![u32::MAX, u32::MAX],
            ] {
                for stride in [1, 2, 7] {
                    let candidates: Vec<_> = (0..segment.max_doc())
                        .filter(|&doc| doc != 55 && doc % stride == 0)
                        .collect();
                    let expected: Vec<_> = candidates
                        .iter()
                        .copied()
                        .filter(|&doc| contains_phrase(&documents[doc as usize], &words, &gaps))
                        .collect();
                    let postings = positional_postings(&inverted, &terms).unwrap().unwrap();
                    let mut scorer = GapVerifiedScorer::new(
                        Box::new(Candidates {
                            docs: candidates,
                            cursor: 0,
                        }),
                        postings,
                        gaps.clone(),
                    );
                    let mut expected_cursor = 0;
                    let mut step = 0;
                    loop {
                        let want = expected.get(expected_cursor).copied().unwrap_or(TERMINATED);
                        assert_eq!(
                            scorer.doc(),
                            want,
                            "gaps={gaps:?} stride={stride} step={step}"
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
        }
    }
}
