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
//!   ([`SearchEngine::chunk_key_field`](crate::api::search_engine)): a hit is tried at its
//!   hint first; then, when the line moved, against every line of the book, the nearest to
//!   the hint first; and a hit that resolves in none of its books is looked for once more,
//!   with every other unresolved hit of the search, in one pass over every column — the
//!   text moved to another book. A key found nowhere is remembered for the generation.
//! * **Recomputed from the stored text**, for an index without the column — a version 4
//!   index — or with one written under another recipe: the key of the line at the hint, and
//!   then of the lines within [`RECOMPUTE_REACH`] of it, from their text and sections. No
//!   pass over the whole index: a moved text is found near where it was, or not at all.
//!
//! Either way a resolved line's key is its 64-bit column value or its full key; a result is
//! checked against the full 128 bits again before it is shown.
//!
//! # What is cached
//!
//! Per generation of the index (a commit is a new one): the books with the facets a filter
//! needs, built on the first filtered search; each book's lines, ordinal to document, for
//! the [`BOOK_CACHE`] books asked about last; and the keys found nowhere.

use crate::semantic_keys::recompute_chunk_keys;
use lru::LruCache;
use otzaria_semantic_search::cancellation::CancellationToken;
use otzaria_semantic_search::semantic::chunk_key::ChunkKey;
use otzaria_semantic_search::semantic::resolve::{
    BookSet, CandidateResolver, ResolveError, ResolvedLine, VectorHit, MAX_RECORDS_PER_HIT,
};
use otzaria_semantic_search::semantic::types::SearchFilters;
use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex, PoisonError};
use tantivy::columnar::Column;
use tantivy::schema::{Facet, Field, IndexRecordOption, Value};
use tantivy::{DocAddress, DocSet, Searcher, TantivyDocument, Term, TERMINATED};

/// How many books' line maps are kept between searches.
const BOOK_CACHE: usize = 64;

/// How far from its hint a hit's text is looked for, in lines, when keys are recomputed from
/// the stored text: an insertion or a deletion a few lines above it is found; a text moved
/// further, or to another book, is not.
pub(crate) const RECOMPUTE_REACH: usize = 16;

/// The most live lines one hit is resolved to: a boilerplate line can occur thousands of
/// times, lexical search still finds every one, and a semantic result needs a handful.
pub(crate) const MAX_LINES_PER_HIT: usize = MAX_RECORDS_PER_HIT;

/// Where a resolved line is, and the key it was resolved by: what the page a search shows
/// is hydrated from and checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ResolvedRecord {
    pub(crate) address: DocAddress,
    pub(crate) key: ChunkKey,
}

/// What a resolver remembers from one search to the next, for one generation of the index.
#[derive(Default)]
pub(crate) struct ResolverCache {
    generation: u64,
    books: Option<Arc<BookDirectory>>,
    lines: Option<LruCache<Arc<str>, Arc<BookLines>>>,
    /// Column values a pass over the whole index found nowhere.
    nowhere: HashSet<u64>,
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
    chunk_key: Option<Column<u64>>,
}

/// The sidecar's resolver over one searcher of the live index.
pub(crate) struct LiveResolver<'a> {
    searcher: Searcher,
    /// Whether the `chunkKey` column is one this build uses; otherwise keys are recomputed.
    column: bool,
    file_path: Field,
    columns: Vec<SegmentColumns>,
    cache: &'a Mutex<ResolverCache>,
    /// Every line this search resolved, by `(file_path, line_id)`, for its page.
    records: Mutex<HashMap<(String, u64), ResolvedRecord>>,
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
            columns,
            cache,
            records: Mutex::new(HashMap::new()),
        })
    }

    fn generation_id(&self) -> u64 {
        self.searcher.generation().generation_id()
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
        key: ChunkKey,
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
            .insert(
                (book.name.to_string(), line_id),
                ResolvedRecord { address, key },
            );
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

    /// The positions of `book` that hold `key`, the nearest to `hint` first: every one the
    /// column has, or, recomputing, those within [`RECOMPUTE_REACH`] of the hint.
    fn find_in_book(
        &self,
        book: &BookLines,
        key: ChunkKey,
        hint: u32,
        cancel: &CancellationToken,
    ) -> Result<Vec<usize>, ResolveError> {
        let centre = book
            .position(hint)
            .unwrap_or_else(|| book.ordinals.partition_point(|&ordinal| ordinal < hint))
            .min(book.docs.len() - 1);
        if self.column {
            let wanted = key.column_value();
            // The hint first, and the whole book only when the line is not there.
            if self.column_value(book.docs[centre]) == Some(wanted) {
                return Ok(vec![centre]);
            }
            return Ok(book
                .by_distance(centre)
                .filter(|&position| self.column_value(book.docs[position]) == Some(wanted))
                .take(MAX_LINES_PER_HIT)
                .collect());
        }
        let at_hint = recompute_chunk_keys(&self.searcher, &book.docs, centre..centre + 1)
            .map_err(index_error)?;
        if at_hint.first().copied().flatten() == Some(key) {
            return Ok(vec![centre]);
        }
        if cancel.is_cancelled() {
            return Err(ResolveError::Cancelled);
        }
        let start = centre.saturating_sub(RECOMPUTE_REACH);
        let end = (centre + RECOMPUTE_REACH + 1).min(book.docs.len());
        let keys =
            recompute_chunk_keys(&self.searcher, &book.docs, start..end).map_err(index_error)?;
        let mut positions: Vec<usize> = keys
            .iter()
            .enumerate()
            .filter(|(_, found)| **found == Some(key))
            .map(|(offset, _)| start + offset)
            .collect();
        positions.sort_by_key(|&position| (position.abs_diff(centre), position));
        Ok(positions)
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

/// Whether `filters` admit a book, by the sidecar's own book test.
fn admits(filters: Option<&SearchFilters>, name: &str, info: &BookInfo) -> bool {
    match filters.and_then(SearchFilters::compile) {
        Some(compiled) => compiled.matches_book(name, &info.facets, info.is_pdf),
        None => true,
    }
}

impl CandidateResolver for LiveResolver<'_> {
    fn generation(&self) -> u64 {
        self.generation_id()
    }

    fn admissible_books(
        &self,
        filters: Option<&SearchFilters>,
    ) -> Result<Option<BookSet>, ResolveError> {
        let Some(compiled) = filters.and_then(SearchFilters::compile) else {
            return Ok(None);
        };
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
        for (hit_index, hit) in hits.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(ResolveError::Cancelled);
            }
            let mut emitted = 0usize;
            let mut seen_books: HashSet<&str> = HashSet::new();
            for record in &hit.records {
                if emitted == MAX_LINES_PER_HIT {
                    break;
                }
                // A book's lines are looked through once per hit, whatever records name it.
                if !seen_books.insert(&record.book) {
                    continue;
                }
                let Some(book) = self.book(&record.book)? else {
                    continue;
                };
                if !admits(filters, &book.name, &book.info) {
                    continue;
                }
                for position in self.find_in_book(&book, hit.key, record.hint, cancel)? {
                    if emitted == MAX_LINES_PER_HIT {
                        break;
                    }
                    if taken.insert(book.docs[position]) {
                        lines.push(self.describe(hit_index, hit.key, &book, position)?);
                        emitted += 1;
                    }
                }
            }
            if emitted == 0 && !hit.records.is_empty() {
                unresolved.push(hit_index);
            }
        }

        // The text moved to another book, or left every book it was in: one pass over the
        // whole column for every such hit together, and only with a column to pass over.
        if self.column && !unresolved.is_empty() {
            let generation = self.generation_id();
            let wanted: HashSet<u64> = {
                let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                let cache = cache.at(generation);
                unresolved
                    .iter()
                    .map(|&hit| hits[hit].key.column_value())
                    .filter(|value| *value != 0 && !cache.nowhere.contains(value))
                    .collect()
            };
            if !wanted.is_empty() {
                let found = self.find_everywhere(&wanted, cancel)?;
                {
                    let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
                    let cache = cache.at(generation);
                    cache.nowhere.extend(
                        wanted
                            .iter()
                            .filter(|value| !found.contains_key(*value))
                            .copied(),
                    );
                }
                for &hit_index in &unresolved {
                    let key = hits[hit_index].key;
                    let Some(addresses) = found.get(&key.column_value()) else {
                        continue;
                    };
                    let mut emitted = 0usize;
                    for &address in addresses {
                        if emitted == MAX_LINES_PER_HIT {
                            break;
                        }
                        let Some(name) = self.book_of(address)? else {
                            continue;
                        };
                        let Some(book) = self.book(&name)? else {
                            continue;
                        };
                        if !admits(filters, &book.name, &book.info) {
                            continue;
                        }
                        let Some(position) = book.docs.iter().position(|doc| *doc == address)
                        else {
                            continue;
                        };
                        if taken.insert(address) {
                            lines.push(self.describe(hit_index, key, &book, position)?);
                            emitted += 1;
                        }
                    }
                }
            }
        }
        Ok(lines)
    }
}
