use tantivy::query::{ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, Score, SegmentReader, TantivyError, Term};

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
        // Exclusions probe before a phrase's first match and after exhaustion.
        // ConstScorer retains DocSet's guarded seek_danger even with scoring disabled.
        Ok(Box::new(ConstScorer::new(
            self.inner.scorer(reader, boost)?,
            0.0,
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
}
