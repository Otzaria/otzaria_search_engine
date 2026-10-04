//! The live index as a vector set's resolver: which books a filter admits, and which live
//! lines hold the keys a scan returned.
//!
//! A stored vector knows the key of the text it was embedded from and where that text was
//! when the vectors were built — a book and the ordinal of a line in it, a hint. The index
//! knows where the text is now. [`LiveResolver`] is the sidecar's `CandidateResolver` over
//! one [`Searcher`]: for each hit it finds the live lines that hold its key, and describes
//! them by the index — id, section, line hash, facets — so fusion and display never read a
//! value the vectors carried.
//!
//! # Two ways to know a line's key
//!
//! * **The `chunkKey` column**, when the index has one this build uses
//!   ([`SearchEngine::chunk_key_field`](crate::api::search_engine)): every record of a hit
//!   is tried at its hint; a book where one is not is searched once, every line of it, the
//!   nearest to the hint first; and a hit that resolves in none of its books is looked for
//!   once more, with every other unresolved hit of the search, in one pass over every
//!   column — the text moved to another book. Where the pass found each value, and that it
//!   found one nowhere, is remembered for the generation, so a value is passed over the
//!   index for at most once a generation, however many searches hit its vector.
//! * **Recomputed from the stored text**, for an index without the column — a version 4
//!   index — or with one written under another recipe: the key of the line at each hint,
//!   and then of the lines within [`RECOMPUTE_REACH`] of a hint that does not hold it, from
//!   their text and sections. No pass over the whole index: a moved text is found near where
//!   it was, or not at all. A book's other lines of a text are the lines of its `lineHash`,
//!   passed over only where their windows cannot join to the text ([`KeySpan`]), within
//!   [`MAX_FAILED_REPEATS`].
//!
//! # Every occurrence, and only the text's
//!
//! A text the library holds in several places is one vector with a record for each, in
//! one book or in several, and each record that resolves is a line of its own: up to
//! [`MAX_LINES_PER_HIT`] per hit, each line once in a search. Without grouping every one is
//! a result; grouping folds them as it folds any lines, by section or by text.
//!
//! A line is returned only once it holds the hit's key by all 128 bits. A column value is
//! the key's first 64, so a line found by its column is checked against the key recomputed
//! from its text and its neighbours' before it is returned, and one that fails — a column
//! left stale by whatever wrote the index — is dropped and counted
//! ([`LiveResolver::unverified`]). Nothing a scan returns reaches fusion, grouping, a page
//! or a group's siblings on 64 bits alone.
//!
//! # A filtered search
//!
//! A filter admits books by what the live index says of them, and the scan reads only the
//! vectors with a record in an admitted book. With the column and a view of the set
//! ([`SetView`]), a filtered search is planned first ([`LiveResolver::plan`]): the texts an
//! admitted book holds and no live record of the set places in it — moved or copied there
//! since the set was built — are looked for in it, and the vectors of those no admitted
//! book's live records reach are named to the sidecar as unreached
//! ([`CandidateResolver::unreached`]), which weighs them besides the scan of the admitted
//! books, each at its own score and none in place of one of the admitted books' hits. The
//! scan itself is the admitted books', whatever moved. See [`crate::semantic_moves`].
//!
//! # What is cached
//!
//! Per generation of the index (a commit is a new one): the books with the facets a filter
//! needs, built on the first filtered search; each book's lines, ordinal to document, for
//! the [`BOOK_CACHE`] books asked about last; where the passes over the whole column found
//! the [`MOVED_CACHE`] values they looked for last, found or not; and the plans of the
//! [`PLAN_CACHE`] filters searched with last. A book's arrivals are kept with the set's
//! view, under the book's postings and its text hash, across generations.

use crate::semantic_keys::{context_window, production_chunking, recompute_chunk_keys, KeySpan};
use crate::semantic_moves::{Arrival, Postings, SetView};
use lru::LruCache;
use otzaria_semantic_search::cancellation::CancellationToken;
use otzaria_semantic_search::semantic::chunk_key::ChunkKey;
use otzaria_semantic_search::semantic::resolve::{
    BookSet, CandidateResolver, LiveKeySource, ResolveError, ResolvedLine, SlotRef, VectorHit,
    MAX_RECORDS_PER_HIT,
};
use otzaria_semantic_search::semantic::types::{CompiledFilters, SearchFilters};
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use tantivy::columnar::Column;
use tantivy::schema::{Facet, Field, IndexRecordOption, Value};
use tantivy::{DocAddress, DocSet, Searcher, TantivyDocument, Term, TERMINATED};

/// How many books' line maps are kept between searches.
const BOOK_CACHE: usize = 64;

/// How many filters' scan plans are kept between searches, for one generation of the index.
const PLAN_CACHE: usize = 16;

#[cfg(test)]
thread_local! {
    /// Set by a test, on its own thread, to make every plan fail as an unreadable index would.
    pub(crate) static FAIL_PLANS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// How many values' places a pass over the whole column found are kept between searches,
/// for one generation of the index.
const MOVED_CACHE: usize = 1024;

/// The most places of one value a pass over the whole column keeps: far more than a hit
/// resolves to, few enough that a text the library holds thousands of times is not kept
/// whole.
const MOVED_PLACES: usize = 1024;

/// How far from its hint a hit's text is looked for, in lines, when keys are recomputed from
/// the stored text: an insertion or a deletion a few lines above it is found; a text moved
/// further, or to another book, is not.
pub(crate) const RECOMPUTE_REACH: usize = 16;

/// The most live lines one hit is resolved to: a boilerplate line can occur thousands of
/// times, lexical search still finds every one, and a semantic result needs a handful.
pub(crate) const MAX_LINES_PER_HIT: usize = MAX_RECORDS_PER_HIT;

/// How many of a hit's repeat candidates may be recomputed without the column and found not to
/// hold its key: reaching it is the only way a candidate that holds it is missed.
pub(crate) const MAX_FAILED_REPEATS: usize = 16;

/// Where a resolved line is: what the page a search shows is hydrated from. The line was
/// held to its hit's whole key before it was resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedRecord {
    pub(crate) address: DocAddress,
}

/// What a resolver remembers from one search to the next, for one generation of the index.
#[derive(Default)]
pub(crate) struct ResolverCache {
    generation: u64,
    books: Option<Arc<BookDirectory>>,
    lines: Option<LruCache<Arc<str>, Arc<BookLines>>>,
    /// Where a pass over the whole column found each value it looked for, in index order:
    /// lines whose column holds it, not yet held to any key; none for a value it found
    /// nowhere.
    moved: Option<LruCache<u64, Arc<[DocAddress]>>>,
    /// The plans of the filters searched with last.
    plans: Option<LruCache<PlanKey, Arc<ScanPlan>>>,
    /// How many books' postings plans walked in this generation.
    #[cfg(test)]
    pub(crate) walks: u64,
    /// How many passes over the whole column searches made in this generation.
    #[cfg(test)]
    pub(crate) passes: u64,
    /// How many keys the search for a text's other lines recomputed, without the column, in
    /// this generation.
    #[cfg(test)]
    pub(crate) recomputes: u64,
    /// The most keys one hit's search for a text's other lines recomputed and found not
    /// held, in this generation.
    #[cfg(test)]
    pub(crate) worst_failures: usize,
}

/// What a scan plan is of: a generation of one set, and a filter.
#[derive(Clone, PartialEq, Eq, Hash)]
struct PlanKey {
    vectors_dir: PathBuf,
    set_generation: u64,
    filters: String,
}

/// How a filtered search scans and resolves: see [`LiveResolver::plan`].
pub(crate) struct ScanPlan {
    /// What the scan reads: the admitted books, and nothing else.
    books: BookSet,
    /// Each text an admitted book holds and no live record of the set places in it, by
    /// column value: the admitted books it is in, in name order, each with the first line
    /// of it that holds it.
    arrivals: HashMap<u64, Vec<(Arc<str>, u32)>>,
    /// The vectors of the arrivals no admitted book's live records reach, sorted: what the
    /// sidecar weighs besides the scan.
    unreached: Vec<SlotRef>,
    /// The generation of the set the slots are of.
    set_generation: u64,
}

impl ResolverCache {
    /// Forget everything of another generation.
    fn at(&mut self, generation: u64) -> &mut Self {
        if self.generation != generation {
            *self = Self {
                generation,
                ..Self::default()
            };
        }
        self
    }

    fn lines(&mut self) -> &mut LruCache<Arc<str>, Arc<BookLines>> {
        self.lines.get_or_insert_with(|| {
            LruCache::new(NonZeroUsize::new(BOOK_CACHE).expect("the cache holds books"))
        })
    }

    fn plans(&mut self) -> &mut LruCache<PlanKey, Arc<ScanPlan>> {
        self.plans.get_or_insert_with(|| {
            LruCache::new(NonZeroUsize::new(PLAN_CACHE).expect("the cache holds plans"))
        })
    }

    fn moved(&mut self) -> &mut LruCache<u64, Arc<[DocAddress]>> {
        self.moved.get_or_insert_with(|| {
            LruCache::new(NonZeroUsize::new(MOVED_CACHE).expect("the cache holds values"))
        })
    }
}

/// Every live book of one generation, and what a filter asks of it.
pub(crate) struct BookDirectory {
    books: HashMap<Arc<str>, BookInfo>,
}

#[derive(Clone)]
struct BookInfo {
    facets: Arc<[String]>,
    is_pdf: bool,
}

/// One book's live lines: the document of each ordinal, in ordinal order.
pub(crate) struct BookLines {
    name: Arc<str>,
    ordinals: Vec<u32>,
    docs: Vec<DocAddress>,
    info: BookInfo,
    /// The positions that share a value of the book's repeat column with another of its
    /// positions, by that value: what finds the other occurrences of a text the book holds
    /// more than once, which a vector set records once per book. Read on first use; see
    /// [`LiveResolver::repeats`].
    repeats: OnceLock<HashMap<u64, Box<[u32]>>>,
}

impl BookLines {
    /// The position of `ordinal` among the book's lines.
    fn position(&self, ordinal: u32) -> Option<usize> {
        let dense = self.ordinals.len() as u64 == u64::from(*self.ordinals.last()?) + 1;
        if dense {
            ((ordinal as usize) < self.docs.len()).then_some(ordinal as usize)
        } else {
            self.ordinals.binary_search(&ordinal).ok()
        }
    }

    /// Every position, nearest to `centre` first, ties to the earlier line.
    fn by_distance(&self, centre: usize) -> impl Iterator<Item = usize> + '_ {
        let len = self.docs.len();
        (0..len.max(1)).flat_map(move |step| {
            let before = centre.checked_sub(step).filter(|_| step > 0);
            let after = Some(centre + step).filter(|&at| at < len);
            before.into_iter().chain(after)
        })
    }
}

/// The columns a resolver reads, opened once per segment for one search.
struct SegmentColumns {
    id: Column<u64>,
    section: Column<u64>,
    line_hash: Column<u64>,
    /// The book's text hash, stamped on every line of it.
    text_hash: Column<u64>,
    chunk_key: Option<Column<u64>>,
}

/// The sidecar's resolver over one searcher of the live index.
pub(crate) struct LiveResolver<'a> {
    searcher: Searcher,
    /// Whether the `chunkKey` column is one this build uses; otherwise keys are recomputed.
    column: bool,
    file_path: Field,
    text: Field,
    columns: Vec<SegmentColumns>,
    cache: &'a Mutex<ResolverCache>,
    /// Every line this search resolved, by `(file_path, line_id)`, for its page.
    records: Mutex<HashMap<(String, u64), ResolvedRecord>>,
    /// Lines whose column held a hit's key and whose text did not: dropped, and counted.
    rejected: Mutex<HashSet<DocAddress>>,
    /// The plan of this search's filter, when one was made.
    plan: Option<Arc<ScanPlan>>,
    /// Why this search's filter could not be planned, when it could not: the semantic half
    /// of the search fails with it, as it would had the resolver failed to read the index.
    failed: Option<ResolveError>,
}

fn index_error(reason: impl std::fmt::Display) -> ResolveError {
    ResolveError::Index {
        reason: reason.to_string(),
    }
}

impl<'a> LiveResolver<'a> {
    /// A resolver over `searcher`, reading its `chunkKey` column when `chunk_key` names one
    /// this build uses.
    pub(crate) fn new(
        searcher: Searcher,
        chunk_key: Option<Field>,
        cache: &'a Mutex<ResolverCache>,
    ) -> Result<Self, ResolveError> {
        let schema = searcher.schema();
        let file_path = schema.get_field("filePath").map_err(index_error)?;
        let text = schema.get_field("text").map_err(index_error)?;
        let chunk_key_name = chunk_key.map(|field| schema.get_field_name(field).to_string());
        let columns = searcher
            .segment_readers()
            .iter()
            .map(|reader| {
                let fast = reader.fast_fields();
                Ok(SegmentColumns {
                    id: fast.u64("id").map_err(index_error)?,
                    section: fast.u64("sectionId").map_err(index_error)?,
                    line_hash: fast.u64("lineHash").map_err(index_error)?,
                    text_hash: fast.u64("textHash").map_err(index_error)?,
                    chunk_key: match &chunk_key_name {
                        Some(name) => fast.column_opt::<u64>(name).map_err(index_error)?,
                        None => None,
                    },
                })
            })
            .collect::<Result<_, ResolveError>>()?;
        Ok(Self {
            searcher,
            column: chunk_key.is_some(),
            file_path,
            text,
            columns,
            cache,
            records: Mutex::new(HashMap::new()),
            rejected: Mutex::new(HashSet::new()),
            plan: None,
            failed: None,
        })
    }

    fn generation_id(&self) -> u64 {
        self.searcher.generation().generation_id()
    }

    /// The searcher every address this resolver hands out belongs to.
    pub(crate) fn searcher(&self) -> &Searcher {
        &self.searcher
    }

    /// Where each line this search resolved is, and the key it was resolved by, by
    /// `(file_path, line_id)`.
    pub(crate) fn records(&self) -> HashMap<(String, u64), ResolvedRecord> {
        std::mem::take(&mut *self.records.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// How many lines this search dropped because their column held a hit's key and their
    /// text, recomputed, did not: each line once, however many hits found it.
    pub(crate) fn unverified(&self) -> u32 {
        let rejected = self.rejected.lock().unwrap_or_else(PoisonError::into_inner);
        rejected.len().min(u32::MAX as usize) as u32
    }

    /// Whether the line at `position` of `book` holds `key` by all 128 bits, recomputed from
    /// the text this searcher reads: its own and its neighbours' within the recipe's window.
    fn holds(
        &self,
        book: &BookLines,
        position: usize,
        key: ChunkKey,
    ) -> Result<bool, ResolveError> {
        let found = recompute_chunk_keys(&self.searcher, &book.docs, position..position + 1)
            .map_err(index_error)?
            .pop()
            .flatten();
        Ok(found == Some(key))
    }

    /// Whether the line at `position` of `book`, whose column holds `key`'s value, holds
    /// `key` itself: checked once per line and search, and remembered when it does not.
    fn verified(
        &self,
        book: &BookLines,
        position: usize,
        key: ChunkKey,
    ) -> Result<bool, ResolveError> {
        let address = book.docs[position];
        if self
            .rejected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&address)
        {
            return Ok(false);
        }
        if self.holds(book, position, key)? {
            return Ok(true);
        }
        log::debug!(
            "The chunkKey column of line {} of {} holds the value of a key its text does not \
             have; the line is not shown for that vector",
            book.ordinals[position],
            book.name
        );
        self.rejected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(address);
        Ok(false)
    }

    /// The live books, built once per generation.
    fn directory(&self) -> Result<Arc<BookDirectory>, ResolveError> {
        let generation = self.generation_id();
        if let Some(books) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at(generation)
            .books
            .clone()
        {
            return Ok(books);
        }
        let mut books: HashMap<Arc<str>, BookInfo> = HashMap::new();
        for (segment_ord, reader) in self.searcher.segment_readers().iter().enumerate() {
            let paths = reader
                .fast_fields()
                .str("filePath")
                .map_err(index_error)?
                .ok_or_else(|| index_error("the index has no filePath column"))?;
            let mut seen = vec![false; paths.num_terms()];
            let mut name = String::new();
            for doc in reader.doc_ids_alive() {
                let Some(ord) = paths.term_ords(doc).next() else {
                    continue;
                };
                if std::mem::replace(&mut seen[ord as usize], true) {
                    continue;
                }
                name.clear();
                paths.ord_to_str(ord, &mut name).map_err(index_error)?;
                if books.contains_key(name.as_str()) {
                    continue;
                }
                let info = self.book_info(DocAddress::new(segment_ord as u32, doc))?;
                books.insert(Arc::from(name.as_str()), info);
            }
        }
        let directory = Arc::new(BookDirectory { books });
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at(generation)
            .books = Some(Arc::clone(&directory));
        Ok(directory)
    }

    /// What one of a book's documents says of the book: its facets, sorted, and whether it
    /// is a PDF.
    fn book_info(&self, address: DocAddress) -> Result<BookInfo, ResolveError> {
        let reader = self.searcher.segment_reader(address.segment_ord);
        let facet_reader = reader.facet_reader("topics").map_err(index_error)?;
        let mut facet = Facet::default();
        let mut facets = Vec::new();
        for ord in facet_reader.facet_ords(address.doc_id) {
            facet_reader
                .facet_from_ord(ord, &mut facet)
                .map_err(index_error)?;
            facets.push(facet.to_string());
        }
        facets.sort();
        facets.dedup();
        let document: TantivyDocument = self.searcher.doc(address).map_err(index_error)?;
        let is_pdf_field = self
            .searcher
            .schema()
            .get_field("isPdf")
            .map_err(index_error)?;
        let is_pdf = document
            .get_first(is_pdf_field)
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        Ok(BookInfo {
            facets: facets.into(),
            is_pdf,
        })
    }

    /// A book's live lines, from its postings and its ids; `None` for a book the index does
    /// not hold.
    fn book(&self, name: &str) -> Result<Option<Arc<BookLines>>, ResolveError> {
        let generation = self.generation_id();
        if let Some(lines) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at(generation)
            .lines()
            .get(name)
        {
            return Ok(Some(Arc::clone(lines)));
        }
        let term = Term::from_field_text(self.file_path, name);
        let mut found: Vec<(u32, DocAddress)> = Vec::new();
        for (segment_ord, reader) in self.searcher.segment_readers().iter().enumerate() {
            let inverted = reader.inverted_index(self.file_path).map_err(index_error)?;
            let Some(mut postings) = inverted
                .read_postings(&term, IndexRecordOption::Basic)
                .map_err(index_error)?
            else {
                continue;
            };
            let ids = &self.columns[segment_ord].id;
            let mut doc = postings.doc();
            while doc != TERMINATED {
                if !reader.is_deleted(doc) {
                    if let Some(id) = ids.first(doc) {
                        // Ids compose `(catalogue_order + 1) << 32` and `ordinal + 1`.
                        let ordinal = (id & 0xFFFF_FFFF).saturating_sub(1) as u32;
                        found.push((ordinal, DocAddress::new(segment_ord as u32, doc)));
                    }
                }
                doc = postings.advance();
            }
        }
        if found.is_empty() {
            return Ok(None);
        }
        found.sort_unstable_by_key(|&(ordinal, address)| (ordinal, address));
        found.dedup_by_key(|(ordinal, _)| *ordinal);
        let info = self.book_info(found[0].1)?;
        let name: Arc<str> = Arc::from(name);
        let lines = Arc::new(BookLines {
            name: Arc::clone(&name),
            ordinals: found.iter().map(|&(ordinal, _)| ordinal).collect(),
            docs: found.iter().map(|&(_, address)| address).collect(),
            info,
            repeats: OnceLock::new(),
        });
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at(generation)
            .lines()
            .put(name, Arc::clone(&lines));
        Ok(Some(lines))
    }

    fn column_value(&self, address: DocAddress) -> Option<u64> {
        self.columns[address.segment_ord as usize]
            .chunk_key
            .as_ref()?
            .first(address.doc_id)
    }

    /// The line at `position` of `book`, as the sidecar describes a resolved line.
    fn describe(
        &self,
        hit: usize,
        book: &BookLines,
        position: usize,
    ) -> Result<ResolvedLine, ResolveError> {
        let address = book.docs[position];
        let columns = &self.columns[address.segment_ord as usize];
        let line_id = columns
            .id
            .first(address.doc_id)
            .ok_or_else(|| index_error(format!("the document at {address:?} has no id")))?;
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert((book.name.to_string(), line_id), ResolvedRecord { address });
        Ok(ResolvedLine {
            hit: hit as u32,
            line_id,
            file_path: book.name.to_string(),
            section_id: columns.section.first(address.doc_id).unwrap_or_default(),
            line_hash: columns.line_hash.first(address.doc_id).unwrap_or_default(),
            segment: u64::from(book.ordinals[position]),
            is_pdf: book.info.is_pdf,
            facets: Arc::clone(&book.info.facets),
            title: String::new(),
            reference: String::new(),
        })
    }

    /// The value a line's repeats in its book are found by: its `chunkKey` column value with
    /// the column, the key's own first 64 bits; without one its `lineHash`, which every line
    /// of one text shares once the text has letters enough to have one. `None` for a line
    /// with neither. Only a candidate either way: a repeat is held to the whole key before
    /// it is a line of the hit's.
    fn repeat_value(&self, address: DocAddress) -> Option<u64> {
        let columns = &self.columns[address.segment_ord as usize];
        let value = if self.column {
            columns.chunk_key.as_ref()?.first(address.doc_id)
        } else {
            columns.line_hash.first(address.doc_id)
        }?;
        (value != 0).then_some(value)
    }

    /// `book`'s repeat map: every value of its repeat column that more than one of its
    /// lines holds, with those lines' positions in order. Read once per book and
    /// generation, on the first hit resolved in it.
    fn repeats<'b>(&self, book: &'b BookLines) -> &'b HashMap<u64, Box<[u32]>> {
        book.repeats.get_or_init(|| {
            let mut values: Vec<(u64, u32)> = book
                .docs
                .iter()
                .enumerate()
                .filter_map(|(position, &address)| {
                    Some((self.repeat_value(address)?, position as u32))
                })
                .collect();
            values.sort_unstable();
            values
                .chunk_by(|a, b| a.0 == b.0)
                .filter(|run| run.len() > 1)
                .map(|run| {
                    (
                        run[0].0,
                        run.iter().map(|&(_, position)| position).collect(),
                    )
                })
                .collect()
        })
    }

    /// The other lines of `book` that hold `key` as its line at `position` does, in order,
    /// at most `limit` and none `taken`: a text the book holds more than once is recorded
    /// once, at its first line. Also the candidates put off, for [`Self::put_off_repeats`].
    /// With more than `limit` holders, those found need not be the first in book order.
    ///
    /// Without the column the candidates are the lines of its `lineHash`, so a repeat whose
    /// short line is itself cut differently is found only with the column. After the first
    /// that fails, its [`KeySpan`] passes over those that cannot hold the key and puts off
    /// those whose windows begin otherwise; each failure spends one of `budget`.
    fn repeats_of(
        &self,
        book: &BookLines,
        position: usize,
        key: ChunkKey,
        limit: usize,
        taken: &HashSet<DocAddress>,
        budget: &mut usize,
    ) -> Result<(Vec<usize>, Vec<usize>), ResolveError> {
        let Some(value) = self.repeat_value(book.docs[position]) else {
            return Ok((Vec::new(), Vec::new()));
        };
        let Some(positions) = self.repeats(book).get(&value) else {
            return Ok((Vec::new(), Vec::new()));
        };
        // The line's span, read once a candidate has failed.
        let mut span: Option<KeySpan> = None;
        let mut found = Vec::new();
        let mut later = Vec::new();
        for &other in positions.iter() {
            if found.len() == limit {
                break;
            }
            let other = other as usize;
            if other == position || taken.contains(&book.docs[other]) {
                continue;
            }
            if self.column {
                if self.verified(book, other, key)? {
                    found.push(other);
                }
                continue;
            }
            if let Some(span) = &span {
                if let Some(window) = self.window_hashes(book, other) {
                    if !span.admits(&window) {
                        continue;
                    }
                    if !span.leads(&window) {
                        later.push(other);
                        continue;
                    }
                }
            }
            match self.holds_within(book, other, key, budget)? {
                Some(true) => found.push(other),
                Some(false) if span.is_none() => span = Some(self.key_span(book, position)?),
                Some(false) => {}
                None => break,
            }
        }
        Ok((found, later))
    }

    /// The lines of `later`, candidates [`Self::repeats_of`] put off, that hold `key` and are
    /// not `taken`, at most `limit`, while `budget` lasts.
    fn put_off_repeats(
        &self,
        book: &BookLines,
        later: &[usize],
        key: ChunkKey,
        limit: usize,
        taken: &HashSet<DocAddress>,
        budget: &mut usize,
    ) -> Result<Vec<usize>, ResolveError> {
        let mut found = Vec::new();
        for &other in later {
            if found.len() == limit {
                break;
            }
            if taken.contains(&book.docs[other]) {
                continue;
            }
            match self.holds_within(book, other, key, budget)? {
                Some(true) => found.push(other),
                Some(false) => {}
                None => break,
            }
        }
        Ok(found)
    }

    /// Whether the line at `other` of `book` holds `key`, recomputed while `budget` lasts, and
    /// `None` once it is spent.
    fn holds_within(
        &self,
        book: &BookLines,
        other: usize,
        key: ChunkKey,
        budget: &mut usize,
    ) -> Result<Option<bool>, ResolveError> {
        if *budget == 0 {
            return Ok(None);
        }
        #[cfg(test)]
        {
            self.cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .at(self.generation_id())
                .recomputes += 1;
        }
        let holds = self.holds(book, other, key)?;
        if !holds {
            *budget -= 1;
        }
        Ok(Some(holds))
    }

    /// The stored text of the line at `address`.
    fn stored_text(&self, address: DocAddress) -> Result<String, ResolveError> {
        let document: TantivyDocument = self.searcher.doc(address).map_err(index_error)?;
        Ok(document
            .get_first(self.text)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string())
    }

    fn section_of(&self, address: DocAddress) -> Option<u64> {
        self.columns[address.segment_ord as usize]
            .section
            .first(address.doc_id)
    }

    fn line_hash_of(&self, address: DocAddress) -> Option<u64> {
        self.columns[address.segment_ord as usize]
            .line_hash
            .first(address.doc_id)
    }

    /// The span of the key the line at `position` of `book` holds, from its window's stored
    /// texts: at most five documents.
    fn key_span(&self, book: &BookLines, position: usize) -> Result<KeySpan, ResolveError> {
        let address = book.docs[position];
        let own = self.stored_text(address)?;
        if own.trim().chars().count() >= production_chunking().min_meaningful_chars {
            return Ok(KeySpan::ANY);
        }
        let (Some(window), Some(own_hash)) = (
            context_window(book.docs.len(), position, |at| {
                self.section_of(book.docs[at])
            }),
            self.line_hash_of(address),
        ) else {
            return Ok(KeySpan::ANY);
        };
        let mut texts = Vec::with_capacity(5);
        for at in window {
            texts.push(if at == position {
                own.clone()
            } else {
                self.stored_text(book.docs[at])?
            });
        }
        let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
        Ok(KeySpan::joined(&texts, own_hash))
    }

    /// The `lineHash`es of the window the line at `position` of `book` is keyed over were it
    /// short, from the columns alone; `None` when they cannot say.
    fn window_hashes(&self, book: &BookLines, position: usize) -> Option<Vec<u64>> {
        context_window(book.docs.len(), position, |at| {
            self.section_of(book.docs[at])
        })?
        .map(|at| self.line_hash_of(book.docs[at]))
        .collect()
    }

    /// The position of `book` the record at `hint` names, when that line holds `key` by
    /// all 128 bits: the first look for every record, which a line that has not moved
    /// passes at the cost of one key.
    fn at_hint(
        &self,
        book: &BookLines,
        key: ChunkKey,
        hint: u32,
    ) -> Result<Option<usize>, ResolveError> {
        let Some(position) = book.position(hint) else {
            return Ok(None);
        };
        let holds = if self.column {
            self.column_value(book.docs[position]) == Some(key.column_value())
                && self.verified(book, position, key)?
        } else {
            self.holds(book, position, key)?
        };
        Ok(holds.then_some(position))
    }

    /// The positions of `book` that hold `key` by all 128 bits and are not `taken`, the
    /// nearest to `hint` first and at most `limit`: every line of the book whose column
    /// holds it, or, recomputing, those within [`RECOMPUTE_REACH`] of the hint. What a
    /// record whose line is not at its hint is looked for by.
    fn search_book(
        &self,
        book: &BookLines,
        key: ChunkKey,
        hint: u32,
        limit: usize,
        taken: &HashSet<DocAddress>,
        cancel: &CancellationToken,
    ) -> Result<Vec<usize>, ResolveError> {
        if cancel.is_cancelled() {
            return Err(ResolveError::Cancelled);
        }
        let centre = book
            .position(hint)
            .unwrap_or_else(|| book.ordinals.partition_point(|&ordinal| ordinal < hint))
            .min(book.docs.len() - 1);
        let mut found = Vec::new();
        if self.column {
            let wanted = key.column_value();
            for position in book.by_distance(centre) {
                if found.len() == limit {
                    break;
                }
                let address = book.docs[position];
                if !taken.contains(&address)
                    && self.column_value(address) == Some(wanted)
                    && self.verified(book, position, key)?
                {
                    found.push(position);
                }
            }
            return Ok(found);
        }
        let start = centre.saturating_sub(RECOMPUTE_REACH);
        let end = (centre + RECOMPUTE_REACH + 1).min(book.docs.len());
        let keys =
            recompute_chunk_keys(&self.searcher, &book.docs, start..end).map_err(index_error)?;
        found.extend(
            keys.iter()
                .enumerate()
                .filter(|(offset, line)| {
                    **line == Some(key) && !taken.contains(&book.docs[start + offset])
                })
                .map(|(offset, _)| start + offset),
        );
        found.sort_by_key(|&position| (position.abs_diff(centre), position));
        found.truncate(limit);
        Ok(found)
    }

    /// One pass over every `chunkKey` column for the values `wanted` holds: where each
    /// is, in index order.
    fn find_everywhere(
        &self,
        wanted: &HashSet<u64>,
        cancel: &CancellationToken,
    ) -> Result<HashMap<u64, Vec<DocAddress>>, ResolveError> {
        let mut found: HashMap<u64, Vec<DocAddress>> = HashMap::new();
        for (segment_ord, reader) in self.searcher.segment_readers().iter().enumerate() {
            let Some(column) = &self.columns[segment_ord].chunk_key else {
                continue;
            };
            for (checked, doc) in reader.doc_ids_alive().enumerate() {
                if checked % 65_536 == 0 && cancel.is_cancelled() {
                    return Err(ResolveError::Cancelled);
                }
                if let Some(value) = column.first(doc) {
                    if value != 0 && wanted.contains(&value) {
                        found
                            .entry(value)
                            .or_default()
                            .push(DocAddress::new(segment_ord as u32, doc));
                    }
                }
            }
        }
        Ok(found)
    }

    /// The book a document belongs to, and its position among the book's lines.
    fn locate(&self, address: DocAddress) -> Result<Option<(Arc<BookLines>, usize)>, ResolveError> {
        let Some(name) = self.book_of(address)? else {
            return Ok(None);
        };
        let Some(book) = self.book(&name)? else {
            return Ok(None);
        };
        let ordinal = self.columns[address.segment_ord as usize]
            .id
            .first(address.doc_id)
            .map(|id| (id & 0xFFFF_FFFF).saturating_sub(1) as u32);
        let position = match ordinal.and_then(|ordinal| book.position(ordinal)) {
            Some(position) if book.docs[position] == address => Some(position),
            // A book that holds an ordinal twice keeps one document of it.
            _ => book.docs.iter().position(|doc| *doc == address),
        };
        Ok(position.map(|position| (book, position)))
    }

    /// The book a document belongs to, by its `filePath`.
    fn book_of(&self, address: DocAddress) -> Result<Option<String>, ResolveError> {
        let reader = self.searcher.segment_reader(address.segment_ord);
        let paths = reader
            .fast_fields()
            .str("filePath")
            .map_err(index_error)?
            .ok_or_else(|| index_error("the index has no filePath column"))?;
        let Some(ord) = paths.term_ords(address.doc_id).next() else {
            return Ok(None);
        };
        let mut name = String::new();
        paths.ord_to_str(ord, &mut name).map_err(index_error)?;
        Ok(Some(name))
    }
}

/// How much of the live index a vector set covers: see [`LiveResolver::coverage`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Coverage {
    /// Live lines the recipe embeds: the lines a vector could exist for.
    pub(crate) keyed_lines: u64,
    /// Those whose key the set holds.
    pub(crate) covered_lines: u64,
    /// Books with a keyed line.
    pub(crate) books_live: u32,
    /// Books with a covered line.
    pub(crate) books_covered: u32,
}

impl LiveResolver<'_> {
    /// Fail the semantic half of this search with `error`, which planning it met: the
    /// sidecar is told when it asks which books the filter admits, and the search falls back
    /// to its lexical results with the reason, as for any resolver that cannot read the index.
    pub(crate) fn fail(&mut self, error: ResolveError) {
        self.failed = Some(error);
    }

    /// Whether this resolver reads keys from the `chunkKey` column.
    pub(crate) fn has_column(&self) -> bool {
        self.column
    }

    /// The live lines the recipe embeds, and how many of them `keys` holds: the column
    /// values of every key of a vector set, sorted. From the column, one pass over it;
    /// without one, every book's keys recomputed from its stored text, which reads the
    /// whole store.
    pub(crate) fn coverage(
        &self,
        keys: &[u64],
        cancel: &CancellationToken,
    ) -> Result<Coverage, ResolveError> {
        // Per book: whether a line is keyed, and whether one is covered.
        let mut books: HashMap<String, (bool, bool)> = HashMap::new();
        let mut coverage = Coverage::default();
        let mut count = |book: &mut (bool, bool), value: u64| {
            coverage.keyed_lines += 1;
            book.0 = true;
            if keys.binary_search(&value).is_ok() {
                coverage.covered_lines += 1;
                book.1 = true;
            }
        };
        if self.column {
            for (segment_ord, reader) in self.searcher.segment_readers().iter().enumerate() {
                let Some(column) = &self.columns[segment_ord].chunk_key else {
                    continue;
                };
                let paths = reader
                    .fast_fields()
                    .str("filePath")
                    .map_err(index_error)?
                    .ok_or_else(|| index_error("the index has no filePath column"))?;
                let mut by_ord = vec![(false, false); paths.num_terms()];
                for (checked, doc) in reader.doc_ids_alive().enumerate() {
                    if checked % 65_536 == 0 && cancel.is_cancelled() {
                        return Err(ResolveError::Cancelled);
                    }
                    let (Some(value), Some(ord)) = (column.first(doc), paths.term_ords(doc).next())
                    else {
                        continue;
                    };
                    if value != 0 {
                        count(&mut by_ord[ord as usize], value);
                    }
                }
                let mut name = String::new();
                for (ord, (keyed, covered)) in by_ord.into_iter().enumerate() {
                    if keyed {
                        name.clear();
                        paths
                            .ord_to_str(ord as u64, &mut name)
                            .map_err(index_error)?;
                        let book = books.entry(name.clone()).or_default();
                        book.0 = true;
                        book.1 |= covered;
                    }
                }
            }
        } else {
            for name in self.directory()?.books.keys() {
                if cancel.is_cancelled() {
                    return Err(ResolveError::Cancelled);
                }
                let Some(lines) = self.book(name)? else {
                    continue;
                };
                let book_keys =
                    recompute_chunk_keys(&self.searcher, &lines.docs, 0..lines.docs.len())
                        .map_err(index_error)?;
                let book = books.entry(name.to_string()).or_default();
                for key in book_keys.into_iter().flatten() {
                    count(book, key.column_value());
                }
            }
        }
        coverage.books_live = books.values().filter(|(keyed, _)| *keyed).count() as u32;
        coverage.books_covered = books.values().filter(|(_, covered)| *covered).count() as u32;
        Ok(coverage)
    }
}

impl LiveResolver<'_> {
    /// Plan the scan of a search filtered by `filters`, over the set `view` describes, and
    /// keep the plan for this search's [`CandidateResolver::admissible_books`] and
    /// [`CandidateResolver::resolve`]: `None` without the column, or for filters that admit
    /// every book, and the search scans and resolves as it did. Kept for the generation of
    /// the index and of the set, per filter.
    ///
    /// What it reads the first time: the directory of books; each admitted book's postings
    /// as the segments that hold it, and for a book whose arrivals the view does not know for
    /// those, one pass over them for its text hash and its column, and the set's live
    /// records of it; when an admitted book has arrivals not looked up before, one pass over
    /// the set's slots for all of them; and when any admitted book has arrivals, the set's
    /// live records of every admitted book, for which of them those reach already.
    pub(crate) fn plan(
        &mut self,
        filters: &SearchFilters,
        view: &SetView,
        cancel: &CancellationToken,
    ) -> Result<Option<Arc<ScanPlan>>, ResolveError> {
        if !self.column {
            return Ok(None);
        }
        let Some(compiled) = filters.compile() else {
            return Ok(None);
        };
        #[cfg(test)]
        if FAIL_PLANS.with(std::cell::Cell::get) {
            return Err(index_error("a test made planning fail"));
        }
        let generation = self.generation_id();
        let key = PlanKey {
            vectors_dir: view.dir().to_path_buf(),
            set_generation: view.generation(),
            filters: format!("{filters:?}"),
        };
        if let Some(plan) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at(generation)
            .plans()
            .get(&key)
        {
            self.plan = Some(Arc::clone(plan));
            return Ok(self.plan.clone());
        }

        let directory = self.directory()?;
        // In name order, not the directory's: an arrival is looked for in its books in this
        // order, and when they hold more of its lines than a hit resolves to, the order is
        // which of them come back.
        let mut admitted: Vec<&Arc<str>> = directory
            .books
            .iter()
            .filter(|(name, info)| compiled.matches_book(name, &info.facets, info.is_pdf))
            .map(|(name, _)| name)
            .collect();
        admitted.sort();

        // Each admitted book's arrivals: known for its postings or its text, or read now —
        // and those read now are looked up in the set together, in one pass over it.
        let mut slots: HashMap<u64, Vec<SlotRef>> = HashMap::new();
        let mut arrivals: HashMap<u64, Vec<(Arc<str>, u32)>> = HashMap::new();
        let mut unknown: Vec<(&Arc<str>, Unread)> = Vec::new();
        for name in &admitted {
            if cancel.is_cancelled() {
                return Err(ResolveError::Cancelled);
            }
            // A PDF's lines are keyed 0: it holds no text of the set's.
            if directory.books[*name].is_pdf {
                continue;
            }
            match self.arrivals(name, view)? {
                BookArrivals::Known(known) => {
                    for arrival in known.iter() {
                        let value = arrival.slot.key.column_value();
                        slots.entry(value).or_default().push(arrival.slot);
                        arrivals
                            .entry(value)
                            .or_default()
                            .push((Arc::clone(name), arrival.ordinal));
                    }
                }
                BookArrivals::Read(Unread {
                    postings,
                    text_hash,
                    values,
                }) => {
                    if !values.is_empty() {
                        unknown.push((
                            name,
                            Unread {
                                postings,
                                text_hash,
                                values,
                            },
                        ));
                    } else {
                        view.remember_arrivals(
                            Arc::clone(name),
                            postings,
                            text_hash,
                            Arc::from([]),
                        );
                    }
                }
            }
        }
        if !unknown.is_empty() {
            // A text the set holds no live vector of is nothing to look for.
            let wanted: HashSet<u64> = unknown
                .iter()
                .flat_map(|(_, unread)| unread.values.iter().map(|(value, _)| *value))
                .collect();
            let held = view
                .live_slots(&wanted, cancel)
                .ok_or(ResolveError::Cancelled)?;
            for (
                name,
                Unread {
                    postings,
                    text_hash,
                    values,
                },
            ) in unknown
            {
                let mut found: Vec<Arrival> = values
                    .iter()
                    .filter_map(|(value, ordinal)| {
                        Some(held.get(value)?.iter().map(|slot| Arrival {
                            slot: *slot,
                            ordinal: *ordinal,
                        }))
                    })
                    .flatten()
                    .collect();
                found.sort_unstable();
                for arrival in &found {
                    let value = arrival.slot.key.column_value();
                    slots.entry(value).or_default().push(arrival.slot);
                    arrivals
                        .entry(value)
                        .or_default()
                        .push((Arc::clone(name), arrival.ordinal));
                }
                view.remember_arrivals(Arc::clone(name), postings, text_hash, found.into());
            }
        }
        // A text with two live slots — which a set does not hold — is one arrival of a book,
        // and the books came in name order.
        for books in arrivals.values_mut() {
            books.dedup_by(|a, b| a.0 == b.0);
        }

        // An arrival some admitted book's live records reach is scanned already; the vectors
        // of the others are weighed besides the scan.
        let mut unreached: Vec<SlotRef> = slots.into_values().flatten().collect();
        if !unreached.is_empty() {
            let admitted_names: HashSet<&str> = admitted.iter().map(|name| name.as_ref()).collect();
            let reached = view.reached(&unreached, &|book| admitted_names.contains(book));
            unreached.retain(|slot| !reached.contains(&slot.key));
        }
        unreached.sort_unstable();
        unreached.dedup();
        if !unreached.is_empty() {
            log::info!(
                "{} vector(s) of texts that the {} book(s) a filter admits hold, recorded in \
                 none of them, are weighed besides the scan of those books",
                unreached.len(),
                admitted.len()
            );
        }
        let plan = Arc::new(ScanPlan {
            books: admitted.iter().map(|name| name.as_ref()).collect(),
            arrivals,
            unreached,
            set_generation: view.generation(),
        });
        self.cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .at(generation)
            .plans()
            .put(key, Arc::clone(&plan));
        self.plan = Some(Arc::clone(&plan));
        Ok(Some(plan))
    }

    /// `book`'s arrivals, when the view knows them for the book's postings — which reads
    /// none of its lines — or for its text; otherwise the texts its live lines hold that no
    /// live record of the set places in it, by column value, sorted, with what they were
    /// read from. One pass over the book's postings reads its text hash and its column.
    fn arrivals(&self, book: &Arc<str>, view: &SetView) -> Result<BookArrivals, ResolveError> {
        let term = Term::from_field_text(self.file_path, book);
        let mut postings: Postings = Vec::new();
        for reader in self.searcher.segment_readers() {
            let inverted = reader.inverted_index(self.file_path).map_err(index_error)?;
            if inverted
                .get_term_info(&term)
                .map_err(index_error)?
                .is_some()
            {
                postings.push((reader.segment_id(), reader.num_deleted_docs()));
            }
        }
        // By segment, whatever order a searcher lists them in.
        postings.sort_unstable();
        if let Some(known) = view.known_arrivals(book, &postings) {
            return Ok(BookArrivals::Known(known));
        }
        #[cfg(test)]
        {
            self.cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .at(self.generation_id())
                .walks += 1;
        }
        let mut text_hash: Option<u64> = None;
        let mut one_text = true;
        // `(value, ordinal)` of every keyed live line.
        let mut live: Vec<(u64, u32)> = Vec::new();
        for (segment_ord, reader) in self.searcher.segment_readers().iter().enumerate() {
            let inverted = reader.inverted_index(self.file_path).map_err(index_error)?;
            let Some(mut postings) = inverted
                .read_postings(&term, IndexRecordOption::Basic)
                .map_err(index_error)?
            else {
                continue;
            };
            let columns = &self.columns[segment_ord];
            let mut doc = postings.doc();
            while doc != TERMINATED {
                if !reader.is_deleted(doc) {
                    let hash = columns.text_hash.first(doc).unwrap_or(0);
                    one_text &= *text_hash.get_or_insert(hash) == hash;
                    let value = columns.chunk_key.as_ref().and_then(|keys| keys.first(doc));
                    if let (Some(value), Some(id)) = (value, columns.id.first(doc)) {
                        if value != 0 {
                            // Ids compose `(catalogue_order + 1) << 32` and `ordinal + 1`.
                            live.push((value, (id & 0xFFFF_FFFF).saturating_sub(1) as u32));
                        }
                    }
                }
                doc = postings.advance();
            }
        }
        // Lines of two texts of the book — a reindex cut short — have no hash to keep by.
        let text_hash = text_hash.filter(|_| one_text).unwrap_or(0);
        if let Some(known) = view.known_arrivals_of_text(book, text_hash, &postings) {
            return Ok(BookArrivals::Known(known));
        }
        // Each value once, at the first line that holds it.
        live.sort_unstable();
        live.dedup_by_key(|(value, _)| *value);
        let mut records = Vec::new();
        view.recorded(book, &mut records);
        let mut recorded: Vec<u64> = records.iter().map(|(key, _)| key.column_value()).collect();
        recorded.sort_unstable();
        let values = live
            .into_iter()
            .filter(|(value, _)| recorded.binary_search(value).is_err())
            .collect();
        Ok(BookArrivals::Read(Unread {
            postings,
            text_hash,
            values,
        }))
    }
}

/// A book's arrivals as [`LiveResolver::arrivals`] finds them.
enum BookArrivals {
    /// Known to the view: the live slots of their vectors, and their lines.
    Known(Arc<[Arrival]>),
    /// Read now, not yet looked up in the set.
    Read(Unread),
}

/// A book's arrivals as read from its lines, before the set is asked about them.
struct Unread {
    /// What they were read from.
    postings: Postings,
    text_hash: u64,
    /// Their column values, sorted, each with the first line of the book that holds it.
    values: Vec<(u64, u32)>,
}

/// The live index as compaction asks it: the key each live line of a book holds now, from
/// the column. Compaction re-anchors a record on the nearest live line that holds its key,
/// and prunes one whose book no longer holds it.
pub(crate) struct LiveKeys<'a> {
    resolver: &'a LiveResolver<'a>,
    /// The library version the index holds, which the caller knows and the index does not.
    library_version: u32,
}

impl<'a> LiveKeys<'a> {
    /// The live index as compaction asks it, or `None` without the column: recomputing every
    /// book's keys would read the whole store, and keys this cannot give would prune every
    /// record as stale.
    pub(crate) fn new(resolver: &'a LiveResolver<'a>, library_version: u32) -> Option<Self> {
        resolver.has_column().then_some(Self {
            resolver,
            library_version,
        })
    }
}

impl LiveKeySource for LiveKeys<'_> {
    fn library_version(&self) -> u32 {
        self.library_version
    }

    fn book_keys(&self, book: &str, out: &mut Vec<(u32, u64)>) -> Result<bool, ResolveError> {
        out.clear();
        let Some(lines) = self.resolver.book(book)? else {
            return Ok(false);
        };
        out.extend(
            lines
                .ordinals
                .iter()
                .zip(&lines.docs)
                .map(|(&ordinal, &address)| {
                    (ordinal, self.resolver.column_value(address).unwrap_or(0))
                }),
        );
        Ok(true)
    }
}

/// Whether `compiled` filters admit a book, by the sidecar's own book test: every book
/// without any.
fn admits(compiled: Option<&CompiledFilters<'_>>, name: &str, info: &BookInfo) -> bool {
    compiled.is_none_or(|compiled| compiled.matches_book(name, &info.facets, info.is_pdf))
}

impl CandidateResolver for LiveResolver<'_> {
    /// The index's generation, with the top bit set for a planned search: a filtered search
    /// that could not be planned answers without the vectors it would have weighed besides,
    /// and the query cache must not hand that answer to one that was.
    fn generation(&self) -> u64 {
        let generation = self.generation_id() & !(1 << 63);
        if self.plan.is_some() {
            generation | 1 << 63
        } else {
            generation
        }
    }

    fn admissible_books(
        &self,
        filters: Option<&SearchFilters>,
    ) -> Result<Option<BookSet>, ResolveError> {
        if let Some(error) = &self.failed {
            return Err(error.clone());
        }
        let Some(compiled) = filters.and_then(SearchFilters::compile) else {
            return Ok(None);
        };
        // A planned search scans the books its plan admitted.
        if let Some(plan) = &self.plan {
            return Ok(Some(plan.books.clone()));
        }
        let directory = self.directory()?;
        Ok(Some(
            directory
                .books
                .iter()
                .filter(|(name, info)| compiled.matches_book(name, &info.facets, info.is_pdf))
                .map(|(name, _)| name.to_string())
                .collect(),
        ))
    }

    /// The vectors of the texts that moved into the admitted books and that no admitted
    /// book's live records reach, as the plan found them, when the set the sidecar scans is
    /// the generation the plan was made for; none otherwise. Each comes back as a hit whose
    /// records name only books the filter does not admit, and is resolved in the admitted
    /// books it arrived in alone.
    fn unreached(
        &self,
        _filters: Option<&SearchFilters>,
        set_generation: u64,
        _cancel: &CancellationToken,
    ) -> Result<Vec<SlotRef>, ResolveError> {
        Ok(match &self.plan {
            Some(plan) if plan.set_generation == set_generation => plan.unreached.clone(),
            _ => Vec::new(),
        })
    }

    fn resolve(
        &self,
        hits: &[VectorHit],
        filters: Option<&SearchFilters>,
        cancel: &CancellationToken,
    ) -> Result<Vec<ResolvedLine>, ResolveError> {
        let mut lines = Vec::new();
        // A line is returned once, for the first — best-scored — hit that resolves to it.
        let mut taken: HashSet<DocAddress> = HashSet::new();
        let mut unresolved: Vec<usize> = Vec::new();
        let compiled = filters.and_then(SearchFilters::compile);
        // Under a filter a record's book is admitted or not by the directory, before its
        // lines are read: a vector weighed besides the scan — a text that moved into an
        // admitted book — has records only in books the filter does not admit, and is
        // resolved in the admitted books it arrived in alone.
        let directory = match compiled {
            Some(_) => Some(self.directory()?),
            None => None,
        };
        for (hit_index, hit) in hits.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(ResolveError::Cancelled);
            }
            // Two passes, so that the cap goes to every book before it goes to any book's
            // second line: first one line for each record — at its hint, or, for a record
            // whose line moved, the nearest line of its book that holds the key — and one
            // for each admitted book the text arrived in under a plan; then, in that order,
            // the other lines of those books that hold it, which the set records once per
            // book, with what is left of the cap.
            let mut emitted = 0usize;
            let mut found: Vec<(Arc<BookLines>, usize)> = Vec::new();
            let mut missed: Vec<(Arc<BookLines>, u32)> = Vec::new();
            for record in &hit.records {
                if emitted == MAX_LINES_PER_HIT {
                    break;
                }
                if let (Some(compiled), Some(directory)) = (&compiled, &directory) {
                    let admitted = directory
                        .books
                        .get(record.book.as_ref())
                        .is_some_and(|info| {
                            compiled.matches_book(&record.book, &info.facets, info.is_pdf)
                        });
                    if !admitted {
                        continue;
                    }
                }
                let Some(book) = self.book(&record.book)? else {
                    continue;
                };
                if !admits(compiled.as_ref(), &book.name, &book.info) {
                    continue;
                }
                let Some(position) = self.at_hint(&book, hit.key, record.hint)? else {
                    missed.push((book, record.hint));
                    continue;
                };
                if !taken.insert(book.docs[position]) {
                    continue;
                }
                lines.push(self.describe(hit_index, &book, position)?);
                emitted += 1;
                found.push((book, position));
            }
            // A record whose line moved is looked for in its book — once per book with the
            // column, which reads every line of it; around each such hint without.
            let mut searched: HashSet<&str> = HashSet::new();
            for (book, hint) in &missed {
                if emitted == MAX_LINES_PER_HIT {
                    break;
                }
                if self.column && !searched.insert(&book.name) {
                    continue;
                }
                if let Some(&position) = self
                    .search_book(book, hit.key, *hint, 1, &taken, cancel)?
                    .first()
                {
                    taken.insert(book.docs[position]);
                    lines.push(self.describe(hit_index, book, position)?);
                    emitted += 1;
                    found.push((Arc::clone(book), position));
                }
            }
            // Under a plan, the admitted books the text arrived in since the set was built,
            // which the set does not record it in: the first line of each that holds it,
            // looked for first where the plan found it.
            if let Some(arrived) = self
                .plan
                .as_ref()
                .and_then(|plan| plan.arrivals.get(&hit.key.column_value()))
            {
                for (name, ordinal) in arrived {
                    if emitted == MAX_LINES_PER_HIT {
                        break;
                    }
                    let Some(book) = self.book(name)? else {
                        continue;
                    };
                    if let Some(&position) = self
                        .search_book(&book, hit.key, *ordinal, 1, &taken, cancel)?
                        .first()
                    {
                        taken.insert(book.docs[position]);
                        lines.push(self.describe(hit_index, &book, position)?);
                        emitted += 1;
                        found.push((book, position));
                    }
                }
            }
            // Then each book's other lines of the text, in the order its first was found.
            // The candidates each book put off come after every book's others, so that one
            // book's cannot spend the budget another's likelier ones need.
            let mut budget = MAX_FAILED_REPEATS;
            let mut room = MAX_LINES_PER_HIT - emitted;
            let mut repeats: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
            for (book, position) in &found {
                if room == 0 {
                    break;
                }
                let (held, later) =
                    self.repeats_of(book, *position, hit.key, room, &taken, &mut budget)?;
                taken.extend(held.iter().map(|&other| book.docs[other]));
                room -= held.len();
                repeats.push((held, later));
            }
            for ((book, _), (held, later)) in found.iter().zip(&mut repeats) {
                if room == 0 {
                    break;
                }
                let more = self.put_off_repeats(book, later, hit.key, room, &taken, &mut budget)?;
                taken.extend(more.iter().map(|&other| book.docs[other]));
                room -= more.len();
                held.extend(more);
            }
            for ((book, _), (mut held, _)) in found.iter().zip(repeats) {
                held.sort_unstable();
                for other in held {
                    lines.push(self.describe(hit_index, book, other)?);
                    emitted += 1;
                }
            }
            #[cfg(test)]
            {
                let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                let cache = cache.at(self.generation_id());
                cache.worst_failures = cache.worst_failures.max(MAX_FAILED_REPEATS - budget);
            }
            if emitted == 0 && !hit.records.is_empty() {
                unresolved.push(hit_index);
            }
        }

        // The text moved to another book, or left every book it was in: one pass over the
        // whole column for every such hit together, and only with a column to pass over. A
        // planned search needs none: every admitted book a text is in now either records it
        // or has it among its arrivals. Where the pass found a value is kept for the
        // generation, found or not, so it is passed for at most once.
        if self.column && self.plan.is_none() && !unresolved.is_empty() {
            let generation = self.generation_id();
            let mut found: HashMap<u64, Arc<[DocAddress]>> = HashMap::new();
            let wanted: HashSet<u64> = {
                let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                let moved = cache.at(generation).moved();
                let mut wanted = HashSet::new();
                for &hit in &unresolved {
                    let value = hits[hit].key.column_value();
                    if value == 0 || found.contains_key(&value) {
                        continue;
                    }
                    match moved.get(&value) {
                        Some(places) => {
                            found.insert(value, Arc::clone(places));
                        }
                        None => {
                            wanted.insert(value);
                        }
                    }
                }
                wanted
            };
            if !wanted.is_empty() {
                #[cfg(test)]
                {
                    self.cache
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .at(generation)
                        .passes += 1;
                }
                let mut passed = self.find_everywhere(&wanted, cancel)?;
                let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                let moved = cache.at(generation).moved();
                for value in wanted {
                    let mut places = passed.remove(&value).unwrap_or_default();
                    places.truncate(MOVED_PLACES);
                    let places: Arc<[DocAddress]> = places.into();
                    moved.put(value, Arc::clone(&places));
                    found.insert(value, places);
                }
            }
            for &hit_index in &unresolved {
                let key = hits[hit_index].key;
                let Some(places) = found.get(&key.column_value()) else {
                    continue;
                };
                // As for records: the first line of each book that holds the key, in index
                // order, and then the books' other lines, by book and line, while the cap
                // lasts. Each is held to the whole key, being found by its column value.
                let mut emitted = 0usize;
                let mut books: HashSet<Arc<str>> = HashSet::new();
                let mut others: Vec<(Arc<BookLines>, usize)> = Vec::new();
                for &address in places.iter() {
                    if emitted == MAX_LINES_PER_HIT {
                        break;
                    }
                    if taken.contains(&address) {
                        continue;
                    }
                    let Some((book, position)) = self.locate(address)? else {
                        continue;
                    };
                    if !admits(compiled.as_ref(), &book.name, &book.info) {
                        continue;
                    }
                    if books.contains(&book.name) {
                        others.push((book, position));
                        continue;
                    }
                    if self.verified(&book, position, key)? {
                        taken.insert(address);
                        lines.push(self.describe(hit_index, &book, position)?);
                        emitted += 1;
                        books.insert(Arc::clone(&book.name));
                    }
                }
                others.sort_by(|a, b| (&a.0.name, a.1).cmp(&(&b.0.name, b.1)));
                for (book, position) in others {
                    if emitted == MAX_LINES_PER_HIT {
                        break;
                    }
                    let address = book.docs[position];
                    if !taken.contains(&address) && self.verified(&book, position, key)? {
                        taken.insert(address);
                        lines.push(self.describe(hit_index, &book, position)?);
                        emitted += 1;
                    }
                }
            }
        }
        Ok(lines)
    }
}
