//! Documents found by `id`, which schema 6 does not index (its terms cost ~45MB).
//!
//! A book's lines are consecutive documents with consecutive ids, so a segment's `id`
//! column is a few runs per book: [`IdRuns`], read once per segment and kept, answers a
//! lookup with a binary search. A line's neighbour is checked beside it first
//! ([`neighbour`]), which needs no runs at all.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use once_cell::sync::Lazy;
use tantivy::columnar::Column;
use tantivy::index::SegmentId;
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

/// One segment's `id` column as runs of consecutive ids on consecutive documents.
struct IdRuns {
    /// `(first id, first doc, length)`, by first id.
    runs: Vec<(u64, DocId, u32)>,
    /// `max_end[i]`: the largest `first id + length` of `runs[..=i]`, to find a run that
    /// starts earlier but still covers an id (two books can share ids).
    max_end: Vec<u64>,
}

impl IdRuns {
    fn read(reader: &SegmentReader) -> tantivy::Result<Self> {
        let column = reader.fast_fields().u64("id")?;
        let mut runs: Vec<(u64, DocId, u32)> = Vec::new();
        const CHUNK: DocId = 4_096;
        let mut docs = Vec::with_capacity(CHUNK as usize);
        let mut values = vec![None; CHUNK as usize];
        for chunk_start in (0..reader.max_doc()).step_by(CHUNK as usize) {
            docs.clear();
            docs.extend(chunk_start..(chunk_start + CHUNK).min(reader.max_doc()));
            column.first_vals(&docs, &mut values[..docs.len()]);
            for (&doc, &value) in docs.iter().zip(&values) {
                let Some(id) = value else {
                    continue;
                };
                match runs.last_mut() {
                    Some((first, start, len))
                        if *start + *len == doc
                            && first.checked_add(u64::from(*len)) == Some(id) =>
                    {
                        *len += 1
                    }
                    _ => runs.push((id, doc, 1)),
                }
            }
        }
        runs.sort_unstable();
        let mut max_end = Vec::with_capacity(runs.len());
        let mut end = 0u64;
        for &(first, _, len) in &runs {
            end = end.max(first.saturating_add(u64::from(len)));
            max_end.push(end);
        }
        Ok(Self { runs, max_end })
    }

    /// Every document holding `id`, deleted or not.
    fn docs(&self, id: u64, out: &mut Vec<DocId>) {
        let mut i = self.runs.partition_point(|&(first, _, _)| first <= id);
        while i > 0 && self.max_end[i - 1] > id {
            i -= 1;
            let (first, start, len) = self.runs[i];
            if id - first < u64::from(len) {
                out.push(start + (id - first) as DocId);
            }
        }
    }
}

/// Runs per segment, read once: segments never change, and this is a few bytes a book.
static RUNS: Lazy<Mutex<HashMap<SegmentId, Arc<IdRuns>>>> = Lazy::new(Default::default);
const RUNS_CACHED_SEGMENTS: usize = 256;

fn runs_of(reader: &SegmentReader) -> tantivy::Result<Arc<IdRuns>> {
    let key = reader.segment_id();
    if let Some(runs) = RUNS.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return Ok(Arc::clone(runs));
    }
    let runs = Arc::new(IdRuns::read(reader)?);
    let mut cache = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= RUNS_CACHED_SEGMENTS {
        cache.clear();
    }
    cache.insert(key, Arc::clone(&runs));
    Ok(runs)
}

/// The live documents of one segment whose `id` is in `ids`, ascending.
fn docs_with_ids(reader: &SegmentReader, ids: &[u64]) -> tantivy::Result<Vec<DocId>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let runs = runs_of(reader)?;
    let mut docs = Vec::new();
    for &id in ids {
        runs.docs(id, &mut docs);
    }
    docs.retain(|&doc| !reader.is_deleted(doc));
    docs.sort_unstable();
    docs.dedup();
    Ok(docs)
}

/// Where each of `ids` lives, as `(segment ordinal, doc)`.
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

#[cfg(test)]
mod runs_tests {
    use super::*;
    use tantivy::schema::{Schema, FAST};
    use tantivy::{doc, Index};

    /// Two books sharing ids: every document holding one is found.
    #[test]
    fn overlapping_runs_find_every_document() {
        let mut builder = Schema::builder();
        let id = builder.add_u64_field("id", FAST);
        let index = Index::create_in_ram(builder.build());
        let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
        for value in [10u64, 11, 12, 13, 5, 6, 11, 12, 40] {
            writer.add_document(doc!(id => value)).unwrap();
        }
        writer.commit().unwrap();
        let searcher = index.reader().unwrap().searcher();
        let reader = &searcher.segment_readers()[0];
        assert_eq!(docs_with_ids(reader, &[11]).unwrap(), [1, 6]);
        assert_eq!(docs_with_ids(reader, &[12, 40, 7]).unwrap(), [2, 7, 8]);
        assert_eq!(
            docs_with_ids(reader, &[4, 14, 41]).unwrap(),
            Vec::<DocId>::new()
        );
    }
}
