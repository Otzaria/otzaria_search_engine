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

use std::sync::Arc;

use tantivy::postings::{Postings, SegmentPostings};
use tantivy::query::{
    BooleanQuery, EmptyScorer, EnableScoring, Explanation, Occur, Query, RegexPhraseQuery, Scorer,
    TermSetQuery, Weight,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::{DocId, DocSet, Score, SegmentReader, TantivyError, Term, TERMINATED};

/// A phrase query whose matches are verified position-by-position against
/// per-pair intermediate-word allowances. See the module docs.
#[derive(Clone, Debug)]
pub(crate) struct GapVerifiedPhraseQuery {
    /// The recall-superset phrase query (slop = sum of `gaps`).
    inner: RegexPhraseQuery,
    /// The field whose positional postings verify candidates — must be the
    /// same field `inner` runs against.
    field: Field,
    /// One whole-term regex per word position (the same joined patterns
    /// `inner` was built from), used to find each word's index terms.
    word_patterns: Vec<String>,
    /// `gaps[i]` = allowed intermediate words between words `i` and `i+1`.
    gaps: Vec<u32>,
}

impl GapVerifiedPhraseQuery {
    pub(crate) fn new(
        inner: RegexPhraseQuery,
        field: Field,
        word_patterns: Vec<String>,
        gaps: Vec<u32>,
    ) -> Self {
        debug_assert_eq!(gaps.len() + 1, word_patterns.len());
        Self {
            inner,
            field,
            word_patterns,
            gaps,
        }
    }
}

impl Query for GapVerifiedPhraseQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let inner = self.inner.weight(enable_scoring)?;
        // The same patterns already compiled inside `inner`'s weight, so a
        // pattern that fails here would have failed the whole query first.
        let regexes = self
            .word_patterns
            .iter()
            .map(|pattern| {
                tantivy_fst::Regex::new(pattern).map_err(|e| {
                    TantivyError::InvalidArgument(format!(
                        "gap-verify regex failed to compile: {e}"
                    ))
                })
            })
            .collect::<tantivy::Result<Vec<_>>>()?;
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
    regexes: Vec<tantivy_fst::Regex>,
    gaps: Vec<u32>,
}

impl Weight for GapVerifiedWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let inner = self.inner.scorer(reader, boost)?;
        let inverted = reader.inverted_index(self.field)?;
        // Materialize positional postings for every index term each word
        // matches in this segment. Unbounded on purpose: `inner` (the
        // `RegexPhraseQuery` weight) has already enforced `max_expansions`
        // over the same automatons and errored on overflow, so this scan is
        // never larger than one the query already paid for — and a *capped*
        // scan here would silently drop true matches.
        let mut word_postings = Vec::with_capacity(self.regexes.len());
        for regex in &self.regexes {
            let mut postings = Vec::new();
            let mut stream = inverted.terms().search(regex).into_stream()?;
            while stream.advance() {
                postings.push(inverted.read_postings_from_terminfo(
                    stream.value(),
                    IndexRecordOption::WithFreqsAndPositions,
                )?);
            }
            if postings.is_empty() {
                // A word with no matching term in this segment can never form
                // a phrase here (and `inner` cannot match either).
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
    position_terms: Vec<Arc<Vec<Term>>>,
    /// `gaps[i]` = allowed intermediate words between words `i` and `i+1`.
    gaps: Vec<u32>,
}

impl TermListPhraseQuery {
    pub(crate) fn new(field: Field, position_terms: Vec<Arc<Vec<Term>>>, gaps: Vec<u32>) -> Self {
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
    position_terms: Vec<Arc<Vec<Term>>>,
    gaps: Vec<u32>,
}

impl Weight for TermListPhraseWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let inner = self.driver.scorer(reader, boost)?;
        let inverted = reader.inverted_index(self.field)?;
        // Positional postings for every materialized term that exists in
        // this segment. Bounded by the materialization budgets the caller
        // already enforced — the collection that produced `position_terms`
        // is the cost guard, not a second ceiling here.
        let mut word_postings = Vec::with_capacity(self.position_terms.len());
        for terms in &self.position_terms {
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
                // A word with no matching term in this segment can never
                // form a phrase here (and the AND driver cannot match
                // either).
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
        self.driver.explain(reader, doc)
    }
}

struct GapVerifiedScorer {
    inner: Box<dyn Scorer>,
    /// Positional postings per word position (≥1 term per word).
    word_postings: Vec<Vec<SegmentPostings>>,
    gaps: Vec<u32>,
    // Reused scratch buffers (verification runs per candidate doc).
    pos_buf: Vec<u32>,
    cur_positions: Vec<u32>,
    feasible: Vec<u32>,
    next_feasible: Vec<u32>,
}

impl GapVerifiedScorer {
    fn new(
        inner: Box<dyn Scorer>,
        word_postings: Vec<Vec<SegmentPostings>>,
        gaps: Vec<u32>,
    ) -> Self {
        let mut scorer = Self {
            inner,
            word_postings,
            gaps,
            pos_buf: Vec::new(),
            cur_positions: Vec::new(),
            feasible: Vec::new(),
            next_feasible: Vec::new(),
        };
        // A freshly built DocSet must already sit on its first matching doc.
        let mut doc = scorer.inner.doc();
        while doc != TERMINATED && !scorer.verify(doc) {
            doc = scorer.inner.advance();
        }
        scorer
    }

    /// Does `doc` contain positions `p0 < p1 < … < p_{k-1}` (one per word)
    /// with `p_{i+1} - p_i - 1 <= gaps[i]` for every pair?
    ///
    /// Runs a forward feasibility sweep: `feasible` holds every position at
    /// which a valid chain over words `0..=w` can end; each next word keeps
    /// the positions reachable from any of them. Both lists are sorted, so
    /// each step is a linear two-pointer merge — no backtracking, and (unlike
    /// a greedy earliest-position chain) no false negatives when a later
    /// start is the only one whose window reaches the next word.
    fn verify(&mut self, doc: DocId) -> bool {
        for w in 0..self.word_postings.len() {
            // Gather this word's positions in `doc`, merged across all the
            // index terms the word's pattern matched. The inner scorer emits
            // docs in increasing order, so the postings only ever seek
            // forward.
            self.cur_positions.clear();
            for postings in &mut self.word_postings[w] {
                if postings.doc() < doc {
                    postings.seek(doc);
                }
                if postings.doc() == doc {
                    postings.positions(&mut self.pos_buf);
                    self.cur_positions.extend_from_slice(&self.pos_buf);
                }
            }
            if self.cur_positions.is_empty() {
                return false;
            }
            self.cur_positions.sort_unstable();

            if w == 0 {
                std::mem::swap(&mut self.feasible, &mut self.cur_positions);
                continue;
            }

            // q extends a chain iff some feasible p satisfies
            // q - gap - 1 <= p <= q - 1 (strictly after p, within the gap).
            let window = self.gaps[w - 1] as u64 + 1;
            self.next_feasible.clear();
            let mut j = 0usize;
            for &q in &self.cur_positions {
                let lo = (q as u64).saturating_sub(window);
                while j < self.feasible.len() && (self.feasible[j] as u64) < lo {
                    j += 1;
                }
                if j < self.feasible.len() && self.feasible[j] < q {
                    self.next_feasible.push(q);
                }
            }
            if self.next_feasible.is_empty() {
                return false;
            }
            std::mem::swap(&mut self.feasible, &mut self.next_feasible);
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
