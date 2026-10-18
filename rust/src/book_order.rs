//! Book numbers, the high half of every document id, and the order books are shown in.
//!
//! A book's number is assigned by the engine the first time its `filePath` is written, and is
//! never given to another book of the same index: a number handed out is reserved on disk
//! ([`BOOK_NUMBERS_FILE`]) before it is used. The catalogue and generation order are not in the
//! ids at all: they are a mapping from `filePath` the application pushes
//! ([`BOOK_ORDER_FILE`]), applied at query time through [`OrderTables`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tantivy::collector::sort_key::NaturalComparator;
use tantivy::collector::{Collector, SegmentCollector, SegmentSortKeyComputer, SortKeyComputer};
use tantivy::columnar::Column;
use tantivy::index::SegmentId;
use tantivy::schema::IndexRecordOption;
use tantivy::{DocId, DocSet, Score, Searcher, SegmentOrdinal, SegmentReader, TERMINATED};

use crate::api::search_engine::combine_fingerprint;
use crate::index_directory::atomic_replace;

/// The order mapping, beside the index files.
pub(crate) const BOOK_ORDER_FILE: &str = "otzaria_book_order.json";
/// The highest book number ever handed out (reserved), beside the index files.
pub(crate) const BOOK_NUMBERS_FILE: &str = "otzaria_book_numbers.json";
/// Numbers reserved on disk at a time, so indexing a library writes the file rarely.
const NUMBER_RESERVATION: u32 = 64;

const ORDINAL_MASK: u64 = 0xFFFF_FFFF;

/// The book number of a document id.
pub(crate) fn book_number_of(id: u64) -> u64 {
    id >> 32
}

/// The id of line `ordinal` (0-based) of book `number`.
#[cfg(test)]
pub(crate) fn document_id(number: u32, ordinal: u32) -> u64 {
    (u64::from(number) << 32) + u64::from(ordinal) + 1
}

/// One book's place in the catalogue and among the generations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BookPlace {
    pub catalogue_order: u32,
    pub generation_order: u32,
}

/// The mapping the application pushed, and its revision (bumped on every change).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct OrderMapping {
    pub revision: u64,
    pub places: HashMap<String, BookPlace>,
}

#[derive(Serialize, Deserialize)]
struct OrderFile {
    revision: u64,
    books: Vec<OrderFileEntry>,
}

#[derive(Serialize, Deserialize)]
struct OrderFileEntry {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(rename = "catalogueOrder")]
    catalogue_order: u32,
    #[serde(rename = "generationOrder")]
    generation_order: u32,
}

impl OrderMapping {
    /// The mapping stored in `index_dir`; empty when there is none or it cannot be read.
    pub(crate) fn load(index_dir: &Path) -> Self {
        let path = index_dir.join(BOOK_ORDER_FILE);
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default();
        };
        match serde_json::from_slice::<OrderFile>(&bytes) {
            Ok(file) => Self {
                revision: file.revision,
                places: file
                    .books
                    .into_iter()
                    .map(|entry| {
                        (
                            entry.file_path,
                            BookPlace {
                                catalogue_order: entry.catalogue_order,
                                generation_order: entry.generation_order,
                            },
                        )
                    })
                    .collect(),
            },
            Err(error) => {
                log::warn!("ignoring unreadable {path:?}: {error}");
                Self::default()
            }
        }
    }

    /// Replaces the file in `index_dir` with this mapping, atomically.
    pub(crate) fn save(&self, index_dir: &Path) -> Result<()> {
        let mut books: Vec<OrderFileEntry> = self
            .places
            .iter()
            .map(|(file_path, place)| OrderFileEntry {
                file_path: file_path.clone(),
                catalogue_order: place.catalogue_order,
                generation_order: place.generation_order,
            })
            .collect();
        books.sort_by(|a, b| {
            (a.catalogue_order, &a.file_path).cmp(&(b.catalogue_order, &b.file_path))
        });
        let file = OrderFile {
            revision: self.revision,
            books,
        };
        let path = index_dir.join(BOOK_ORDER_FILE);
        atomic_replace(&path, &serde_json::to_vec(&file)?)
            .with_context(|| format!("writing {path:?}"))
    }
}

/// When and how large the mapping file in `index_dir` is: a cheap way to tell that another
/// engine replaced it.
pub(crate) fn order_file_stamp(index_dir: &Path) -> Option<(std::time::SystemTime, u64)> {
    let metadata = std::fs::metadata(index_dir.join(BOOK_ORDER_FILE)).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

#[derive(Serialize, Deserialize)]
struct NumbersFile {
    reserved: u32,
}

/// Which book holds which number, as far as this engine knows: the books of every segment it
/// has read, and every number it handed out since.
pub(crate) struct BookNumbers {
    index_dir: PathBuf,
    by_path: HashMap<String, u32>,
    /// The book holding each number.
    paths: BTreeMap<u32, Arc<str>>,
    /// The segments whose books are recorded; a new one is read on its own.
    segments: HashSet<SegmentId>,
    /// The next number to hand out.
    next: u32,
    /// Every number below it may have been handed out, so none is handed out again.
    reserved: u32,
    /// Changes whenever a book is recorded.
    version: u64,
}

impl BookNumbers {
    /// No book yet, and the reservation stored in `index_dir`.
    pub(crate) fn new(index_dir: &Path) -> Self {
        let reserved = read_reservation(index_dir).max(1);
        Self {
            index_dir: index_dir.to_path_buf(),
            by_path: HashMap::new(),
            paths: BTreeMap::new(),
            segments: HashSet::new(),
            next: reserved,
            reserved,
            version: 0,
        }
    }

    /// The numbers of the books `searcher` holds, and the reservation stored in `index_dir`.
    #[cfg(test)]
    pub(crate) fn load(searcher: &Searcher, index_dir: &Path) -> Result<Self> {
        let mut numbers = Self::new(index_dir);
        let unread = numbers.unread_segments(searcher);
        let found = numbers_in_segments(searcher, &unread)?;
        numbers.absorb(searcher, &unread, found);
        Ok(numbers)
    }

    /// The segments of `searcher` whose books are not recorded yet, by ordinal.
    pub(crate) fn unread_segments(&self, searcher: &Searcher) -> Vec<usize> {
        searcher
            .segment_readers()
            .iter()
            .enumerate()
            .filter(|(_, reader)| !self.segments.contains(&reader.segment_id()))
            .map(|(ordinal, _)| ordinal)
            .collect()
    }

    /// Records the books `numbers_in_segments` found in the segments `read` of `searcher`.
    pub(crate) fn absorb(
        &mut self,
        searcher: &Searcher,
        read: &[usize],
        found: Vec<(String, u32)>,
    ) {
        let readers = searcher.segment_readers();
        self.segments
            .extend(read.iter().map(|&ordinal| readers[ordinal].segment_id()));
        for (path, number) in found {
            self.record(path, number);
        }
    }

    fn record(&mut self, path: String, number: u32) {
        // Search snapshots may finish in either order. Record every alias for sorting,
        // but leave the writer's current choice alone: catch_up reconciles it against the
        // latest commit when this engine takes the exclusive index writer.
        let new_number =
            if let std::collections::btree_map::Entry::Vacant(entry) = self.paths.entry(number) {
                entry.insert(Arc::from(path.as_str()));
                true
            } else {
                false
            };
        self.by_path.entry(path).or_insert(number);
        self.next = self.next.max(number.saturating_add(1));
        if new_number {
            self.version += 1;
        }
    }

    /// The number of `file_path`, handing out a new one to a book not seen before. Called
    /// only while this engine holds the index writer, so no other writer hands out numbers.
    pub(crate) fn number_for(&mut self, file_path: &str) -> Result<u32> {
        if let Some(&number) = self.by_path.get(file_path) {
            return Ok(number);
        }
        if self.next >= self.reserved {
            // Another engine may have reserved numbers since this one last looked.
            self.next = self.next.max(read_reservation(&self.index_dir));
        }
        let number = self.next;
        if number == u32::MAX {
            anyhow::bail!("every book number of this index has been handed out");
        }
        if number >= self.reserved {
            let reserved = number.saturating_add(NUMBER_RESERVATION);
            let path = self.index_dir.join(BOOK_NUMBERS_FILE);
            atomic_replace(&path, &serde_json::to_vec(&NumbersFile { reserved })?)
                .with_context(|| format!("writing {path:?}"))?;
            self.reserved = reserved;
        }
        self.record(file_path.to_owned(), number);
        Ok(number)
    }

    /// Takes in what another writer may have done before this engine held the writer: the
    /// books of `searcher` (the last commit) and the numbers reserved on disk.
    pub(crate) fn catch_up(&mut self, searcher: &Searcher) -> Result<()> {
        // Only a newly acquired writer calls this. Previously read segments can now be
        // authoritative again (an old engine may reuse its original number for a deleted
        // path), so reconcile all live books, not only segments unseen by a search. This
        // scans book terms once per acquisition, skipping deleted postings until the
        // first live one per book/segment, without decoding stored line text.
        let read: Vec<_> = (0..searcher.segment_readers().len()).collect();
        let found = numbers_in_segments(searcher, &read)?;
        for (path, number) in &found {
            self.by_path.insert(path.clone(), *number);
        }
        self.absorb(searcher, &read, found);
        let reserved = read_reservation(&self.index_dir);
        self.next = self.next.max(reserved);
        self.reserved = self.reserved.max(reserved);
        Ok(())
    }

    /// Records `file_path` at `number`, which it must not already have another of.
    #[cfg(test)]
    pub(crate) fn force_for_tests(&mut self, file_path: &str, number: u32) {
        match self.by_path.get(file_path) {
            Some(&held) => assert_eq!(held, number, "{file_path} already holds number {held}"),
            None => self.record(file_path.to_owned(), number),
        }
    }

    /// A count that changes whenever a book is recorded.
    pub(crate) fn version(&self) -> u64 {
        self.version
    }

    /// The book holding each number.
    pub(crate) fn paths(&self) -> &BTreeMap<u32, Arc<str>> {
        &self.paths
    }
}

fn read_reservation(index_dir: &Path) -> u32 {
    std::fs::read(index_dir.join(BOOK_NUMBERS_FILE))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<NumbersFile>(&bytes).ok())
        .map_or(0, |file| file.reserved)
}

/// Every book of the segments `ordinals` of `searcher` with its number: one live document
/// of each `filePath` term, so the work is per book, not per line.
pub(crate) fn numbers_in_segments(
    searcher: &Searcher,
    ordinals: &[usize],
) -> Result<Vec<(String, u32)>> {
    let schema = searcher.schema();
    let file_path_f = schema.get_field("filePath")?;
    let mut found: HashMap<String, u32> = HashMap::new();
    for &ordinal in ordinals {
        let reader = searcher.segment_reader(ordinal as SegmentOrdinal);
        let ids = reader.fast_fields().u64("id")?;
        let inverted = reader.inverted_index(file_path_f)?;
        let mut terms = inverted.terms().stream()?;
        while terms.advance() {
            let mut postings =
                inverted.read_postings_from_terminfo(terms.value(), IndexRecordOption::Basic)?;
            let mut doc = postings.doc();
            while doc != TERMINATED && reader.is_deleted(doc) {
                doc = postings.advance();
            }
            if doc == TERMINATED {
                continue;
            }
            let Some(id) = ids.first(doc) else {
                continue;
            };
            let number = book_number_of(id);
            if number == 0 || number > u64::from(u32::MAX - 1) {
                continue;
            }
            let path = String::from_utf8_lossy(terms.key()).into_owned();
            let number = number as u32;
            // A book written twice under two numbers keeps the lower; both stay unused.
            found
                .entry(path)
                .and_modify(|seen| *seen = (*seen).min(number))
                .or_insert(number);
        }
    }
    Ok(found.into_iter().collect())
}

/// Every mapping state a process has seen gets its own epoch, so caches shared between
/// engines never confuse two of them.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_epoch() -> u64 {
    NEXT_EPOCH.fetch_add(1, Ordering::Relaxed)
}

/// Each book number's rank in the catalogue and among the generations: what a sort key is
/// made of. Books absent from the mapping come after every book in it, by number.
pub(crate) struct OrderTables {
    catalogue: RankTable,
    generation: RankTable,
    /// How many numbers the tables rank.
    known: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OrderKind {
    Catalogue,
    Generation,
}

/// Up to this book number a table is a plain array; past it, a map, so a stray number
/// read from a foreign index cannot ask for gigabytes.
const DENSE_NUMBERS: u32 = 1 << 20;

/// The rank of each known book number.
#[derive(Clone)]
enum RankTable {
    /// By number; `u32::MAX` for a number no book holds.
    Dense(Arc<[u32]>),
    Sparse(Arc<HashMap<u64, u32>>),
}

impl RankTable {
    fn of(ranked: &[u32], dense: bool, max: u32) -> (Self, u32) {
        let known = ranked.len() as u32;
        let table = if dense {
            let mut ranks = vec![u32::MAX; max as usize + 1];
            for (rank, &number) in ranked.iter().enumerate() {
                ranks[number as usize] = rank as u32;
            }
            Self::Dense(ranks.into())
        } else {
            Self::Sparse(Arc::new(
                ranked
                    .iter()
                    .enumerate()
                    .map(|(rank, &number)| (u64::from(number), rank as u32))
                    .collect(),
            ))
        };
        (table, known)
    }

    /// A number no book held when the table was built (one written since) ranks past every
    /// known one, by number.
    #[inline]
    fn rank(&self, known: u32, number: u64) -> u64 {
        let found = match self {
            Self::Dense(ranks) => ranks
                .get(number as usize)
                .copied()
                .filter(|&rank| rank != u32::MAX),
            Self::Sparse(ranks) => ranks.get(&number).copied(),
        };
        match found {
            Some(rank) => u64::from(rank),
            None => (u64::from(known) + number).min(u64::from(u32::MAX)),
        }
    }
}

impl OrderTables {
    pub(crate) fn build(
        paths: &BTreeMap<u32, Arc<str>>,
        places: &HashMap<String, BookPlace>,
    ) -> Self {
        // Number 0 is no book's: ids below `1 << 32`, which sort first as they always did.
        let mut numbers: Vec<(Option<BookPlace>, u32)> = std::iter::once((None, 0))
            .chain(
                paths
                    .iter()
                    .filter(|(&number, _)| number != 0)
                    .map(|(&number, path)| (places.get(&**path).copied(), number)),
            )
            .collect();
        let max = numbers.iter().map(|&(_, number)| number).max().unwrap_or(0);
        let dense = max < DENSE_NUMBERS;
        let numbers_of = |numbers: &[(Option<BookPlace>, u32)]| -> Vec<u32> {
            numbers.iter().map(|&(_, number)| number).collect()
        };
        numbers.sort_unstable_by_key(|(place, number)| {
            (
                *number != 0,
                place.is_none(),
                place.map_or(0, |p| p.catalogue_order),
                *number,
            )
        });
        let (catalogue, known) = RankTable::of(&numbers_of(&numbers), dense, max);
        numbers.sort_unstable_by_key(|(place, number)| {
            (
                *number != 0,
                place.is_none(),
                place.map_or(0, |p| p.generation_order),
                place.map_or(0, |p| p.catalogue_order),
                *number,
            )
        });
        let (generation, _) = RankTable::of(&numbers_of(&numbers), dense, max);
        Self {
            catalogue,
            generation,
            known,
        }
    }

    fn table(&self, kind: OrderKind) -> &RankTable {
        match kind {
            OrderKind::Catalogue => &self.catalogue,
            OrderKind::Generation => &self.generation,
        }
    }

    /// The ascending sort key of document `id`: its book's rank, then its line.
    #[inline]
    pub(crate) fn key(&self, kind: OrderKind, id: u64) -> u64 {
        key_in(self.table(kind), self.known, id)
    }

    /// The catalogue rank of book `number`.
    pub(crate) fn catalogue_rank(&self, number: u64) -> u64 {
        self.catalogue.rank(self.known, number)
    }
}

#[inline]
fn key_in(table: &RankTable, known: u32, id: u64) -> u64 {
    (table.rank(known, book_number_of(id)) << 32) | (id & ORDINAL_MASK)
}

/// Orders a `TopDocs` by [`OrderTables::key`].
pub(crate) struct BookOrderSort {
    pub tables: Arc<OrderTables>,
    pub kind: OrderKind,
}

impl SortKeyComputer for BookOrderSort {
    type SortKey = Option<u64>;
    type Child = BookOrderColumn;
    type Comparator = NaturalComparator;

    fn segment_sort_key_computer(&self, reader: &SegmentReader) -> tantivy::Result<Self::Child> {
        BookOrderColumn::open(reader, Arc::clone(&self.tables), self.kind)
    }
}

/// The sort key of each document of one segment.
pub(crate) struct BookOrderColumn {
    ids: Column<u64>,
    table: RankTable,
    known: u32,
}

impl BookOrderColumn {
    pub(crate) fn open(
        reader: &SegmentReader,
        tables: Arc<OrderTables>,
        kind: OrderKind,
    ) -> tantivy::Result<Self> {
        Ok(Self {
            ids: reader.fast_fields().u64("id")?,
            table: tables.table(kind).clone(),
            known: tables.known,
        })
    }

    #[inline]
    pub(crate) fn first(&self, doc: DocId) -> Option<u64> {
        self.ids
            .first(doc)
            .map(|id| key_in(&self.table, self.known, id))
    }
}

impl SegmentSortKeyComputer for BookOrderColumn {
    type SortKey = Option<u64>;
    type SegmentSortKey = Option<u64>;
    type SegmentComparator = NaturalComparator;

    #[inline]
    fn segment_sort_key(&mut self, doc: DocId, _score: Score) -> Option<u64> {
        self.first(doc)
    }

    fn convert_segment_sort_key(&self, sort_key: Option<u64>) -> Option<u64> {
        sort_key
    }
}

/// One book's stamps as [`BookStampsCollector`] reads them.
pub(crate) struct CollectedStamps {
    pub number: u32,
    pub documents: u32,
    /// In the order of [`BookStampsCollector::columns`]; 0 when absent or in disagreement.
    pub stamps: [u64; 4],
}

/// Per `filePath`: its number, live documents and four stamp columns, each read with the
/// semantics of the engine's fingerprint collectors (`combine_fingerprint`).
pub(crate) struct BookStampsCollector {
    pub columns: [&'static str; 4],
}

#[derive(Clone, Copy, Default)]
pub(crate) struct PartialStamps {
    documents: u32,
    number: Option<u64>,
    stamps: [Option<u64>; 4],
}

impl PartialStamps {
    fn merge(&mut self, other: &PartialStamps) {
        self.documents += other.documents;
        self.number = match (self.number, other.number) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        for (seen, value) in self.stamps.iter_mut().zip(other.stamps) {
            *seen = combine_fingerprint(*seen, value);
        }
    }
}

pub(crate) struct BookStampsSegmentCollector {
    str_col: Option<tantivy::columnar::StrColumn>,
    id_col: Column<u64>,
    stamp_cols: [Option<Column<u64>>; 4],
    books: HashMap<u64, PartialStamps>,
}

impl Collector for BookStampsCollector {
    type Fruit = Vec<(String, CollectedStamps)>;
    type Child = BookStampsSegmentCollector;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<BookStampsSegmentCollector> {
        let fast = reader.fast_fields();
        Ok(BookStampsSegmentCollector {
            str_col: fast.str("filePath")?,
            id_col: fast.u64("id")?,
            stamp_cols: self.columns.map(|name| fast.u64(name).ok()),
            books: HashMap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        per_segment: Vec<tantivy::Result<HashMap<String, PartialStamps>>>,
    ) -> tantivy::Result<Self::Fruit> {
        let mut merged: HashMap<String, PartialStamps> = HashMap::new();
        for segment in per_segment {
            for (path, partial) in segment? {
                merged.entry(path).or_default().merge(&partial);
            }
        }
        Ok(merged
            .into_iter()
            .map(|(path, partial)| {
                let book = CollectedStamps {
                    number: partial
                        .number
                        .and_then(|number| u32::try_from(number).ok())
                        .unwrap_or(0),
                    documents: partial.documents,
                    stamps: partial.stamps.map(|stamp| stamp.unwrap_or(0)),
                };
                (path, book)
            })
            .collect())
    }
}

impl SegmentCollector for BookStampsSegmentCollector {
    type Fruit = tantivy::Result<HashMap<String, PartialStamps>>;

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        let Some(str_col) = &self.str_col else {
            return;
        };
        let Some(term_ord) = str_col.term_ords(doc_id).next() else {
            return;
        };
        let book = self.books.entry(term_ord).or_default();
        book.documents += 1;
        if book.number.is_none() {
            book.number = self.id_col.first(doc_id).map(book_number_of);
        }
        for (seen, column) in book.stamps.iter_mut().zip(&self.stamp_cols) {
            let value = column.as_ref().and_then(|col| col.first(doc_id));
            *seen = combine_fingerprint(*seen, value);
        }
    }

    fn harvest(self) -> tantivy::Result<HashMap<String, PartialStamps>> {
        let Some(col) = self.str_col else {
            return Ok(HashMap::new());
        };
        let mut result = HashMap::with_capacity(self.books.len());
        let mut buf = String::new();
        for (term_ord, partial) in self.books {
            buf.clear();
            if col.ord_to_str(term_ord, &mut buf)? {
                result.insert(buf.clone(), partial);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[Option<&str>]) -> BTreeMap<u32, Arc<str>> {
        names
            .iter()
            .enumerate()
            .filter_map(|(number, name)| name.map(|name| (number as u32, Arc::from(name))))
            .collect()
    }

    fn place(catalogue_order: u32, generation_order: u32) -> BookPlace {
        BookPlace {
            catalogue_order,
            generation_order,
        }
    }

    #[test]
    fn unmapped_books_follow_every_mapped_one_by_number() {
        let paths = paths(&[None, Some("a"), Some("b"), Some("c"), Some("d")]);
        let places = HashMap::from([("c".to_owned(), place(7, 1)), ("a".to_owned(), place(9, 0))]);
        let tables = OrderTables::build(&paths, &places);
        let order = |kind| {
            let mut numbers = vec![1u32, 2, 3, 4];
            numbers.sort_by_key(|&n| tables.key(kind, document_id(n, 0)));
            numbers
        };
        assert_eq!(order(OrderKind::Catalogue), [3, 1, 2, 4]);
        // Generation first, then the catalogue.
        assert_eq!(order(OrderKind::Generation), [1, 3, 2, 4]);
        // A number newer than the tables sorts after all of them.
        assert!(
            tables.key(OrderKind::Catalogue, document_id(99, 0))
                > tables.key(OrderKind::Catalogue, document_id(4, 5))
        );
    }

    #[test]
    fn a_generation_ties_on_the_catalogue_then_the_line() {
        let paths = paths(&[None, Some("x"), Some("y")]);
        let places = HashMap::from([("x".to_owned(), place(5, 2)), ("y".to_owned(), place(3, 2))]);
        let tables = OrderTables::build(&paths, &places);
        let key = |n, line| tables.key(OrderKind::Generation, document_id(n, line));
        assert!(key(2, 9) < key(1, 0));
        assert!(key(1, 0) < key(1, 1));
    }
}
