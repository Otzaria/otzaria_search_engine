use tantivy::query::{EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{
    DocId, DocSet, Score, SeekDangerResult, SegmentReader, TantivyError, Term, TERMINATED,
};

#[derive(Debug)]
pub(crate) struct NegativeQuery {
    inner: Box<dyn Query>,
}

impl NegativeQuery {
    pub(crate) fn new(inner: Box<dyn Query>) -> Self {
        Self { inner }
    }
}

impl Clone for NegativeQuery {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.box_clone(),
        }
    }
}

impl Query for NegativeQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        let disabled = match enable_scoring {
            EnableScoring::Enabled { searcher, .. } => {
                EnableScoring::disabled_from_searcher(searcher)
            }
            disabled @ EnableScoring::Disabled { .. } => disabled,
        };
        Ok(Box::new(NegativeWeight {
            inner: self.inner.weight(disabled)?,
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a Term, bool)) {
        self.inner.query_terms(visitor);
    }
}

struct NegativeWeight {
    inner: Box<dyn Weight>,
}

impl Weight for NegativeWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        Ok(Box::new(NegativeScorer::new(
            self.inner.scorer(reader, boost)?,
        )))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let mut scorer = self.scorer(reader, 1.0)?;
        if doc >= reader.max_doc() || scorer.doc() > doc || scorer.seek(doc) != doc {
            return Err(TantivyError::InvalidArgument(format!(
                "Document #({doc}) does not match the negative query"
            )));
        }
        Ok(Explanation::new("Negative query filter", 0.0))
    }
}

/// Protect phrase scorers from probes before their first match and beyond
/// exhaustion, while retaining candidate-only seeks. A failed danger seek can
/// invalidate the inner scorer: cache its returned bound, never read its doc().
struct NegativeScorer {
    inner: Box<dyn Scorer>,
    lower_bound: DocId,
    size_hint: u32,
    cost: u64,
}

impl NegativeScorer {
    fn new(inner: Box<dyn Scorer>) -> Self {
        let lower_bound = inner.doc();
        let size_hint = inner.size_hint();
        let cost = inner.cost();
        Self {
            inner,
            lower_bound,
            size_hint,
            cost,
        }
    }
}

impl DocSet for NegativeScorer {
    fn advance(&mut self) -> DocId {
        self.lower_bound = self.inner.advance();
        self.lower_bound
    }

    fn seek(&mut self, target: DocId) -> DocId {
        self.lower_bound = self.inner.seek(target);
        self.lower_bound
    }

    fn seek_danger(&mut self, target: DocId) -> SeekDangerResult {
        if target >= TERMINATED || target < self.lower_bound {
            return SeekDangerResult::SeekLowerBound(self.lower_bound.max(target));
        }
        let result = self.inner.seek_danger(target);
        self.lower_bound = match result {
            SeekDangerResult::Found => target,
            SeekDangerResult::SeekLowerBound(bound) => bound,
        };
        result
    }

    fn doc(&self) -> DocId {
        self.lower_bound
    }

    fn size_hint(&self) -> u32 {
        self.size_hint
    }

    fn cost(&self) -> u64 {
        self.cost
    }
}

impl Scorer for NegativeScorer {
    fn score(&mut self) -> Score {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::{Count, TopDocs};
    use tantivy::query::{BooleanQuery, Occur, PhraseQuery, TermQuery};
    use tantivy::schema::{Field, IndexRecordOption, Schema, TEXT};
    use tantivy::{doc, Index, IndexWriter, Searcher, TERMINATED};

    fn single_segment(lines: &[&str]) -> (Searcher, Field) {
        let mut schema = Schema::builder();
        let text = schema.add_text_field("text", TEXT);
        let index = Index::create_in_ram(schema.build());
        let mut writer: IndexWriter = index.writer_with_num_threads(1, 50_000_000).unwrap();
        for line in lines {
            writer.add_document(doc!(text => *line)).unwrap();
        }
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        assert_eq!(searcher.segment_readers().len(), 1);
        (searcher, text)
    }

    fn positive(text: Field) -> TermQuery {
        TermQuery::new(
            Term::from_field_text(text, "keep"),
            IndexRecordOption::WithFreqs,
        )
    }

    fn negative(text: Field) -> PhraseQuery {
        PhraseQuery::new(vec![
            Term::from_field_text(text, "left"),
            Term::from_field_text(text, "right"),
        ])
    }

    fn excluding(text: Field) -> BooleanQuery {
        BooleanQuery::new(vec![
            (Occur::Must, Box::new(positive(text))),
            (
                Occur::MustNot,
                Box::new(NegativeQuery::new(Box::new(negative(text)))),
            ),
        ])
    }

    fn hits(searcher: &Searcher, query: &dyn Query) -> Vec<(u32, Score)> {
        let mut hits: Vec<_> = searcher
            .search(query, &TopDocs::with_limit(10).order_by_score())
            .unwrap()
            .into_iter()
            .map(|(score, address)| (address.doc_id, score))
            .collect();
        hits.sort_unstable_by_key(|(doc, _)| *doc);
        hits
    }

    #[test]
    fn exhausted_negative_phrase_preserves_positive_matches_and_scores() {
        let (searcher, text) = single_segment(&["keep left", "right keep"]);
        let scorer = negative(text)
            .weight(EnableScoring::disabled_from_searcher(&searcher))
            .unwrap()
            .scorer(searcher.segment_reader(0), 1.0)
            .unwrap();
        assert_eq!(scorer.doc(), TERMINATED);
        let query = excluding(text);
        assert_eq!(searcher.search(&query, &Count).unwrap(), 2);
        assert_eq!(hits(&searcher, &query), hits(&searcher, &positive(text)));
    }

    #[test]
    fn negative_phrase_excludes_only_its_match_before_and_after_exhaustion() {
        let (searcher, text) =
            single_segment(&["keep before", "keep left right", "keep after", "keep last"]);
        let query = excluding(text);
        assert_eq!(searcher.search(&query, &Count).unwrap(), 3);
        let expected: Vec<_> = hits(&searcher, &positive(text))
            .into_iter()
            .filter(|(doc, _)| *doc != 1)
            .collect();
        assert_eq!(hits(&searcher, &query), expected);
        let weight = NegativeQuery::new(Box::new(negative(text)))
            .weight(EnableScoring::disabled_from_searcher(&searcher))
            .unwrap();
        let reader = searcher.segment_reader(0);
        assert!(weight.explain(reader, 0).is_err());
        assert_eq!(weight.explain(reader, 1).unwrap().value(), 0.0);
        assert!(weight.explain(reader, 2).is_err());
        assert!(weight.explain(reader, reader.max_doc()).is_err());
        assert!(weight.explain(reader, TERMINATED).is_err());
    }
    #[test]
    fn negative_regex_phrase_probes_only_the_positive_candidate() {
        use tantivy::query::RegexPhraseQuery;

        let mut lines = vec!["left right", "keep left gap right"];
        lines.extend(std::iter::repeat_n("left gap right", 512));
        lines.push("keep left right");
        lines.push("keep after");
        let (searcher, text) = single_segment(&lines);
        let phrase = || RegexPhraseQuery::new(text, vec!["left".into(), "right".into()]);
        let enable = EnableScoring::disabled_from_searcher(&searcher);
        let bare_weight = phrase().weight(enable).unwrap();
        let weight = NegativeQuery::new(Box::new(phrase()))
            .weight(enable)
            .unwrap();
        let reader = searcher.segment_reader(0);
        let mut bare = bare_weight.scorer(reader, 1.0).unwrap();
        let mut guarded = weight.scorer(reader, 1.0).unwrap();
        assert_eq!(bare.doc(), 0);
        assert_eq!(guarded.doc(), 0);
        // A failed candidate must not eagerly scan 512 unrelated documents to
        // locate the next complete phrase. Compare work, without a timing gate.
        let expected = bare.seek_danger(1);
        assert_eq!(guarded.seek_danger(1), expected);
        // After a failed danger seek, only further danger seeks are legal.
        assert_eq!(guarded.seek_danger(514), bare.seek_danger(514));
        assert_eq!(guarded.score(), 0.0);
        assert_eq!(guarded.seek_danger(515), bare.seek_danger(515));
        assert_eq!(
            guarded.seek_danger(TERMINATED),
            bare.seek_danger(TERMINATED)
        );

        let query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(positive(text))),
            (
                Occur::MustNot,
                Box::new(NegativeQuery::new(Box::new(phrase()))),
            ),
        ]);
        assert_eq!(searcher.search(&query, &Count).unwrap(), 2);
        let expected: Vec<_> = hits(&searcher, &positive(text))
            .into_iter()
            .filter(|(doc, _)| *doc != 514)
            .collect();
        assert_eq!(hits(&searcher, &query), expected);
    }

    #[test]
    fn negative_scorer_respects_invalid_state_and_cached_bounds() {
        use std::sync::{Arc, Mutex};

        struct StrictScorer {
            doc: DocId,
            valid: bool,
            probes: Arc<Mutex<Vec<DocId>>>,
        }
        impl DocSet for StrictScorer {
            fn advance(&mut self) -> DocId {
                assert!(self.valid);
                self.seek(self.doc.saturating_add(1).min(TERMINATED))
            }
            fn seek(&mut self, target: DocId) -> DocId {
                assert!(self.valid);
                assert!(target >= self.doc);
                self.doc = [1, 5, 10]
                    .into_iter()
                    .find(|doc| *doc >= target)
                    .unwrap_or(TERMINATED);
                self.doc
            }
            fn seek_danger(&mut self, target: DocId) -> SeekDangerResult {
                self.probes.lock().unwrap().push(target);
                let bound = [1, 5, 10]
                    .into_iter()
                    .find(|doc| *doc >= target)
                    .unwrap_or(TERMINATED);
                self.doc = bound;
                self.valid = target == bound && bound != TERMINATED;
                if self.valid {
                    SeekDangerResult::Found
                } else {
                    SeekDangerResult::SeekLowerBound(bound)
                }
            }
            fn doc(&self) -> DocId {
                assert!(self.valid);
                self.doc
            }
            fn size_hint(&self) -> u32 {
                assert!(self.valid);
                3
            }
            fn cost(&self) -> u64 {
                assert!(self.valid);
                99
            }
        }
        impl Scorer for StrictScorer {
            fn score(&mut self) -> Score {
                panic!("negative scorer must not evaluate scores")
            }
        }
        let probes = Arc::new(Mutex::new(Vec::new()));
        let mut scorer = NegativeScorer::new(Box::new(StrictScorer {
            doc: 1,
            valid: true,
            probes: probes.clone(),
        }));
        assert_eq!(scorer.seek_danger(0), SeekDangerResult::SeekLowerBound(1));
        assert_eq!(scorer.seek_danger(1), SeekDangerResult::Found);
        assert_eq!(scorer.score(), 0.0);
        assert_eq!(scorer.seek_danger(2), SeekDangerResult::SeekLowerBound(5));
        assert_eq!(scorer.seek_danger(3), SeekDangerResult::SeekLowerBound(5));
        assert_eq!(scorer.seek_danger(4), SeekDangerResult::SeekLowerBound(5));
        assert_eq!(scorer.seek_danger(5), SeekDangerResult::Found);
        // Normal DocSet methods are legal again after Found.
        assert_eq!(scorer.doc(), 5);
        assert_eq!(scorer.seek(5), 5);
        assert_eq!(scorer.advance(), 10);
        assert_eq!(scorer.seek(10), 10);
        assert_eq!(
            scorer.seek_danger(11),
            SeekDangerResult::SeekLowerBound(TERMINATED)
        );
        assert_eq!(
            scorer.seek_danger(12),
            SeekDangerResult::SeekLowerBound(TERMINATED)
        );
        assert_eq!(
            scorer.seek_danger(TERMINATED),
            SeekDangerResult::SeekLowerBound(TERMINATED)
        );
        assert_eq!(*probes.lock().unwrap(), vec![1, 2, 5, 11]);
        // Planner estimates are cached before entering a potentially invalid state.
        assert_eq!(scorer.size_hint(), 3);
        assert_eq!(scorer.cost(), 99);
    }
}
