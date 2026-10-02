//! What a filtered semantic search has to scan for beyond the books it admits: the texts a
//! live book holds that the vector set does not record in it.
//!
//! A filtered search scans only the vectors with a record in a book the filter admits —
//! the sidecar's book filter, which is what keeps it fast — and a set's records say where
//! its texts were when it was built. A text that has since moved into an admitted book, or
//! been copied into one, from a book the filter does not admit, has a vector whose records
//! name only the books it was in: the filtered scan never reaches it, although a live
//! admitted book holds it. A text a book holds twice is not one of these: the set records a
//! text once per book, and the resolver finds the book's other lines of it.
//!
//! So a filtered search is planned first ([`LiveResolver::plan`]). For every admitted book
//! its **arrivals** are read: the texts, by `chunkKey` column value, that its live lines hold
//! and the set records nowhere in it — computed once per book and kept, for as long as the
//! set's generation does not change, under the book's text hash, which an index commit that
//! leaves the book as it was keeps too. When no admitted book has any, nothing moved, and
//! the plan is the fast filtered scan, exactly as before. Otherwise every arrival is looked
//! up in the set, in one pass over it for those not looked up before: which books record
//! it, and which hold its vector. One that some admitted book records is scanned already.
//! For one no admitted book records, the books that hold its vector join the scan, which
//! fetches more vectors in proportion to the vectors those books add, at most up to the
//! ranking's ceiling; the resolver looks for each arrival in the admitted books it arrived
//! in, and keeps only lines of admitted books, by the live index, as it always does.
//!
//! The arrivals are column values, so a plan is made only for an index with the column;
//! without it — a version 4 index — a filtered search scans the admitted books alone, and
//! finds a text that moved into one once the vectors are updated.
//!
//! [`LiveResolver::plan`]: crate::semantic_resolver::LiveResolver::plan

use otzaria_semantic_search::semantic::oxv::reader::Segment;
use otzaria_semantic_search::semantic::segment_set;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// One generation of an installed vector set as a plan reads it: the book tables and keys of
/// its segments, mapped without the set's lock — nothing here writes, or cleans up — and kept
/// by the session for as long as it serves that generation.
pub(crate) struct SetView {
    dir: PathBuf,
    generation: u64,
    segments: Vec<Segment>,
    /// What each text a plan looked up is to the set, by column value.
    keys: Mutex<HashMap<u64, Arc<KeyBooks>>>,
    /// Each book's arrivals, and what they were computed for.
    arrivals: Mutex<HashMap<Arc<str>, Arrivals>>,
}

/// A book's arrivals, and the state of the book they are of.
struct Arrivals {
    /// The book's `textHash` then: the same text keys the same, so they hold for as long as
    /// it does. `0`, which a book without one has, is never matched.
    text_hash: u64,
    /// The index generation they were computed in, which they hold for whatever the hash.
    index_generation: u64,
    values: Arc<[u64]>,
}

/// Where a vector set holds one text.
#[derive(Debug, Default)]
pub(crate) struct KeyBooks {
    /// Every book with a record of it, primary, extra or foreign: a filtered scan admitting
    /// one of them reaches its vector.
    pub(crate) recorded: Vec<Arc<str>>,
    /// The books whose own slots hold a vector of it: scanning one of them scans that slot.
    pub(crate) vectors: Vec<Arc<str>>,
}

impl SetView {
    /// The set at `dir` as its live generation is, when that is `generation` — the one the
    /// session serves. `Ok(None)` when it is another, because an install or a compaction
    /// moved it on, and when nothing is installed.
    pub(crate) fn open(dir: &Path, generation: u64) -> Result<Option<Self>, String> {
        let Some(info) = segment_set::info(dir).map_err(|err| err.to_string())? else {
            return Ok(None);
        };
        if info.generation != generation {
            return Ok(None);
        }
        let segments = info
            .segments
            .iter()
            // Where the set keeps its segments, as the sidecar lays it out.
            .map(|segment| Segment::open(&dir.join("segments").join(format!("{}.oxv", segment.id))))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        Ok(Some(Self {
            dir: dir.to_path_buf(),
            generation,
            segments,
            keys: Mutex::default(),
            arrivals: Mutex::default(),
        }))
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// The column value of every text the set records in `book`, sorted and once each:
    /// its slots' keys, its extra records' and its foreign records'.
    pub(crate) fn recorded_values(&self, book: &str) -> Vec<u64> {
        let mut values = Vec::new();
        for segment in &self.segments {
            let Some(entry) = Self::entry(segment, book) else {
                continue;
            };
            values.extend(
                entry
                    .slots
                    .clone()
                    .map(|slot| segment.key(slot).column_value()),
            );
            values.extend(
                entry
                    .extras
                    .clone()
                    .map(|index| segment.key(segment.extra(index).0).column_value()),
            );
            values.extend(
                entry
                    .foreign
                    .clone()
                    .map(|index| segment.foreign_record(index).0.column_value()),
            );
        }
        values.sort_unstable();
        values.dedup();
        values
    }

    /// How many records the set holds in `books`, in every segment: the vectors a filtered
    /// scan of them reads, near enough.
    pub(crate) fn records_in<'b>(&self, books: impl IntoIterator<Item = &'b str>) -> u64 {
        books
            .into_iter()
            .map(|book| {
                self.segments
                    .iter()
                    .filter_map(|segment| Self::entry(segment, book))
                    .map(|entry| {
                        u64::from(entry.slots.len() as u32)
                            + u64::from(entry.extras.len() as u32)
                            + u64::from(entry.foreign.len() as u32)
                    })
                    .sum::<u64>()
            })
            .sum()
    }

    fn entry<'s>(
        segment: &'s Segment,
        book: &str,
    ) -> Option<&'s otzaria_semantic_search::semantic::oxv::reader::SegmentBook> {
        let books = segment.books();
        books
            .binary_search_by(|entry| entry.name.as_bytes().cmp(book.as_bytes()))
            .ok()
            .map(|at| &books[at])
    }

    /// Where the set holds each text of `values`: the ones looked up before from memory, the
    /// rest in one pass over every segment's records. A value the set holds nowhere maps to
    /// no books.
    pub(crate) fn key_books(&self, values: &HashSet<u64>) -> HashMap<u64, Arc<KeyBooks>> {
        let mut known = self.keys.lock().unwrap_or_else(PoisonError::into_inner);
        let wanted: HashSet<u64> = values
            .iter()
            .filter(|value| !known.contains_key(*value))
            .copied()
            .collect();
        if !wanted.is_empty() {
            let mut found: HashMap<u64, KeyBooks> = HashMap::new();
            for segment in &self.segments {
                for entry in segment.books() {
                    for slot in entry.slots.clone() {
                        let value = segment.key(slot).column_value();
                        if wanted.contains(&value) {
                            let books = found.entry(value).or_default();
                            books.recorded.push(Arc::clone(&entry.name));
                            books.vectors.push(Arc::clone(&entry.name));
                        }
                    }
                    for index in entry.extras.clone() {
                        let value = segment.key(segment.extra(index).0).column_value();
                        if wanted.contains(&value) {
                            found
                                .entry(value)
                                .or_default()
                                .recorded
                                .push(Arc::clone(&entry.name));
                        }
                    }
                    for index in entry.foreign.clone() {
                        let value = segment.foreign_record(index).0.column_value();
                        if wanted.contains(&value) {
                            found
                                .entry(value)
                                .or_default()
                                .recorded
                                .push(Arc::clone(&entry.name));
                        }
                    }
                }
            }
            for value in wanted {
                let books = found.remove(&value).unwrap_or_default();
                known.insert(value, Arc::new(books));
            }
        }
        values
            .iter()
            .filter_map(|value| Some((*value, Arc::clone(known.get(value)?))))
            .collect()
    }

    /// `book`'s arrivals as computed before, when they still hold: for the same text hash,
    /// or in the same index generation.
    pub(crate) fn known_arrivals(
        &self,
        book: &str,
        text_hash: u64,
        index_generation: u64,
    ) -> Option<Arc<[u64]>> {
        let arrivals = self.arrivals.lock().unwrap_or_else(PoisonError::into_inner);
        let known = arrivals.get(book)?;
        ((text_hash != 0 && known.text_hash == text_hash)
            || known.index_generation == index_generation)
            .then(|| Arc::clone(&known.values))
    }

    pub(crate) fn remember_arrivals(
        &self,
        book: Arc<str>,
        text_hash: u64,
        index_generation: u64,
        values: Arc<[u64]>,
    ) {
        self.arrivals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                book,
                Arrivals {
                    text_hash,
                    index_generation,
                    values,
                },
            );
    }
}
