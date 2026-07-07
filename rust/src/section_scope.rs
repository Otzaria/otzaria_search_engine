//! "Same section" scope support for advanced searches.
//!
//! An index document is a single book line, so "all words in the same
//! paragraph" is just a boolean AND — but "all words under the same heading"
//! spans documents. Every document carries a `sectionId` fast-field value
//! (identical for all lines of one heading block, globally unique across
//! books), and the engine answers the section scope in two passes:
//!
//! 1. For each query word, collect the set of `sectionId` values its
//!    documents cover ([`SectionIdsCollector`]) and intersect the sets —
//!    the sections that contain *every* word.
//! 2. Run the union of the per-word queries wrapped in
//!    [`SectionFilteredQuery`], which admits only documents whose
//!    `sectionId` is in the intersection — so the results are exactly the
//!    lines that carry a query word inside a fully-matching section.
//!
//! The intersection is computed against the same reader generation the
//! wrapped query runs on, and a section id never collides across books
//! (the id embeds the book's catalogue id_base), so a section from a
//! facet-excluded book can never admit lines of another book.
//!
//! Like `gap_phrase`, the wrapper is a plain `Query`/`Weight`/`Scorer`
//! sandwich, so counting, per-book counting, facet counting and boolean
//! composition all see the filtered doc set with no special-casing.

use std::collections::HashSet;
use std::sync::Arc;

use tantivy::collector::{Collector, SegmentCollector};
use tantivy::query::{EmptyScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, SegmentOrdinal, SegmentReader, TERMINATED};

/// The schema name of the section fast field (see `current_schema`).
pub(crate) const SECTION_ID_FIELD: &str = "sectionId";

/// Collects the distinct `sectionId` values of every matching document.
pub(crate) struct SectionIdsCollector;

pub(crate) struct SectionIdsSegmentCollector {
    column: tantivy::columnar::Column<u64>,
    sections: HashSet<u64>,
}

impl Collector for SectionIdsCollector {
    type Fruit = HashSet<u64>;
    type Child = SectionIdsSegmentCollector;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<SectionIdsSegmentCollector> {
        Ok(SectionIdsSegmentCollector {
            column: reader.fast_fields().u64(SECTION_ID_FIELD)?,
            sections: HashSet::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, per_segment: Vec<HashSet<u64>>) -> tantivy::Result<HashSet<u64>> {
        let mut merged: HashSet<u64> = HashSet::new();
        for sections in per_segment {
            merged.extend(sections);
        }
        Ok(merged)
    }
}

impl SegmentCollector for SectionIdsSegmentCollector {
    type Fruit = HashSet<u64>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if let Some(section) = self.column.first(doc) {
            self.sections.insert(section);
        }
    }

    fn harvest(self) -> HashSet<u64> {
        self.sections
    }
}

/// Wraps a query and admits only documents whose `sectionId` fast-field
/// value is in `allowed`. See the module docs.
#[derive(Debug)]
pub(crate) struct SectionFilteredQuery {
    inner: Box<dyn Query>,
    allowed: Arc<HashSet<u64>>,
}

// `Query` requires `Clone`, but `Box<dyn Query>` only clones via `box_clone`.
impl Clone for SectionFilteredQuery {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.box_clone(),
            allowed: self.allowed.clone(),
        }
    }
}

impl SectionFilteredQuery {
    pub(crate) fn new(inner: Box<dyn Query>, allowed: Arc<HashSet<u64>>) -> Self {
        Self { inner, allowed }
    }
}

impl Query for SectionFilteredQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(SectionFilteredWeight {
            inner: self.inner.weight(enable_scoring)?,
            allowed: self.allowed.clone(),
        }))
    }
}

struct SectionFilteredWeight {
    inner: Box<dyn Weight>,
    allowed: Arc<HashSet<u64>>,
}

impl Weight for SectionFilteredWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        if self.allowed.is_empty() {
            return Ok(Box::new(EmptyScorer));
        }
        let inner = self.inner.scorer(reader, boost)?;
        let column = reader.fast_fields().u64(SECTION_ID_FIELD)?;
        Ok(Box::new(SectionFilteredScorer::new(
            inner,
            column,
            self.allowed.clone(),
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        self.inner.explain(reader, doc)
    }
}

struct SectionFilteredScorer {
    inner: Box<dyn Scorer>,
    column: tantivy::columnar::Column<u64>,
    allowed: Arc<HashSet<u64>>,
}

impl SectionFilteredScorer {
    fn new(
        inner: Box<dyn Scorer>,
        column: tantivy::columnar::Column<u64>,
        allowed: Arc<HashSet<u64>>,
    ) -> Self {
        let mut scorer = Self {
            inner,
            column,
            allowed,
        };
        // A freshly built DocSet must already sit on its first matching doc.
        let mut doc = scorer.inner.doc();
        while doc != TERMINATED && !scorer.admitted(doc) {
            doc = scorer.inner.advance();
        }
        scorer
    }

    fn admitted(&self, doc: DocId) -> bool {
        self.column
            .first(doc)
            .is_some_and(|section| self.allowed.contains(&section))
    }
}

impl DocSet for SectionFilteredScorer {
    fn advance(&mut self) -> DocId {
        loop {
            let doc = self.inner.advance();
            if doc == TERMINATED || self.admitted(doc) {
                return doc;
            }
        }
    }

    fn seek(&mut self, target: DocId) -> DocId {
        // Already positioned on an admitted doc at or past the target.
        if self.inner.doc() >= target {
            return self.inner.doc();
        }
        let mut doc = self.inner.seek(target);
        while doc != TERMINATED && !self.admitted(doc) {
            doc = self.inner.advance();
        }
        doc
    }

    fn doc(&self) -> DocId {
        self.inner.doc()
    }

    fn size_hint(&self) -> u32 {
        // Upper bound: filtering only removes docs.
        self.inner.size_hint()
    }
}

impl Scorer for SectionFilteredScorer {
    fn score(&mut self) -> Score {
        self.inner.score()
    }
}
