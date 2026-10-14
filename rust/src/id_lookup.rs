//! Documents found by `id`, which schema 6 does not index (its terms cost ~45MB).
//!
//! A line's neighbour is almost always the next or previous document of its segment —
//! a book's lines are added in order — so [`neighbour`] checks there first; anything else
//! scans the `id` column once for a whole set of ids ([`IdSetQuery`], [`find_ids`]).

use std::collections::HashMap;
use std::sync::Arc;

use tantivy::columnar::Column;
use tantivy::query::{ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, SegmentReader, TERMINATED};

/// The live document of this segment next to `doc` that has `id`.
pub(crate) fn neighbour(
    reader: &SegmentReader,
    ids: &Column<u64>,
    doc: DocId,
    id: u64,
) -> Option<DocId> {
    [doc.checked_add(1), doc.checked_sub(1), Some(doc)]
        .into_iter()
        .flatten()
        .filter(|&candidate| candidate < reader.max_doc())
        .find(|&candidate| ids.first(candidate) == Some(id) && !reader.is_deleted(candidate))
}

/// The live documents of one segment whose `id` is in `ids` (sorted), ascending.
fn docs_with_ids(reader: &SegmentReader, ids: &[u64]) -> tantivy::Result<Vec<DocId>> {
    let (Some(&min), Some(&max)) = (ids.first(), ids.last()) else {
        return Ok(Vec::new());
    };
    let column = reader.fast_fields().u64("id")?;
    let mut docs = Vec::new();
    column.get_docids_for_value_range(min..=max, 0..reader.max_doc(), &mut docs);
    docs.retain(|&doc| {
        !reader.is_deleted(doc)
            && column
                .first(doc)
                .is_some_and(|id| ids.binary_search(&id).is_ok())
    });
    docs.sort_unstable();
    docs.dedup();
    Ok(docs)
}

/// Where each of `ids` lives, as `(segment ordinal, doc)`: one scan per segment.
pub(crate) fn find_ids<'a>(
    readers: impl IntoIterator<Item = &'a SegmentReader>,
    ids: &[u64],
) -> tantivy::Result<HashMap<u64, (usize, DocId)>> {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut found = HashMap::new();
    for (ordinal, reader) in readers.into_iter().enumerate() {
        if found.len() == sorted.len() {
            break;
        }
        let column = reader.fast_fields().u64("id")?;
        for doc in docs_with_ids(reader, &sorted)? {
            if let Some(id) = column.first(doc) {
                found.entry(id).or_insert((ordinal, doc));
            }
        }
    }
    Ok(found)
}

/// Matches the documents whose `id` is one of a set; scores a constant.
#[derive(Clone, Debug)]
pub(crate) struct IdSetQuery {
    ids: Arc<Vec<u64>>,
}

impl IdSetQuery {
    pub(crate) fn new(ids: impl IntoIterator<Item = u64>) -> Self {
        let mut ids: Vec<u64> = ids.into_iter().collect();
        ids.sort_unstable();
        ids.dedup();
        Self { ids: Arc::new(ids) }
    }
}

impl Query for IdSetQuery {
    fn weight(&self, _enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        Ok(Box::new(IdSetWeight {
            ids: Arc::clone(&self.ids),
        }))
    }
}

struct IdSetWeight {
    ids: Arc<Vec<u64>>,
}

impl Weight for IdSetWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let docs = docs_with_ids(reader, &self.ids)?;
        Ok(Box::new(ConstScorer::new(Docs { docs, cursor: 0 }, boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let matches = docs_with_ids(reader, &self.ids)?
            .binary_search(&doc)
            .is_ok();
        if !matches {
            return Err(tantivy::TantivyError::InvalidArgument(format!(
                "document {doc} does not match"
            )));
        }
        Ok(Explanation::new("IdSetQuery", 1.0))
    }
}

/// Ascending documents, as a doc set.
struct Docs {
    docs: Vec<DocId>,
    cursor: usize,
}

impl DocSet for Docs {
    fn advance(&mut self) -> DocId {
        self.cursor = (self.cursor + 1).min(self.docs.len());
        self.doc()
    }

    fn doc(&self) -> DocId {
        self.docs.get(self.cursor).copied().unwrap_or(TERMINATED)
    }

    fn size_hint(&self) -> u32 {
        self.docs.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::DocSetCollector;
    use tantivy::schema::{Schema, FAST};
    use tantivy::{doc, Index};

    #[test]
    fn ids_are_found_beside_a_line_and_across_segments() {
        let mut builder = Schema::builder();
        let id = builder.add_u64_field("id", FAST);
        let index = Index::create_in_ram(builder.build());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for value in [10u64, 11, 12] {
            writer.add_document(doc!(id => value)).unwrap();
        }
        writer.commit().unwrap();
        for value in [13u64, 14] {
            writer.add_document(doc!(id => value)).unwrap();
        }
        writer.commit().unwrap();
        writer
            .delete_query(Box::new(IdSetQuery::new([11])))
            .unwrap();
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        let readers = searcher.segment_readers();
        let (first, second) = if readers[0].max_doc() == 3 {
            (0, 1)
        } else {
            (1, 0)
        };
        let ids = readers[first].fast_fields().u64("id").unwrap();
        assert_eq!(neighbour(&readers[first], &ids, 0, 11), None, "deleted");
        assert_eq!(neighbour(&readers[first], &ids, 1, 12), Some(2));
        assert_eq!(neighbour(&readers[first], &ids, 2, 13), None);

        let found = find_ids(readers, &[13, 10, 11, 99]).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[&10], (first, 0));
        assert_eq!(found[&13], (second, 0));

        let hits = searcher
            .search(&IdSetQuery::new([14, 12, 11]), &DocSetCollector)
            .unwrap();
        assert_eq!(hits.len(), 2);
    }
}
