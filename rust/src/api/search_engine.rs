use crate::frb_generated::StreamSink;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use log::debug;
use std::collections::HashMap;
use tantivy::collector::{Collector, Count, FacetCollector, SegmentCollector, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::indexer::NoMergePolicy;
use tantivy::query::{BooleanQuery, FuzzyTermQuery, Occur, RegexQuery, TermQuery, TermSetQuery};
use tantivy::query::{Query, RegexPhraseQuery};
use tantivy::schema::Value;
use tantivy::snippet::SnippetGenerator;
use tantivy::{doc, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher};
use tantivy::{schema::*, Index};
use tantivy::{DocId, SegmentOrdinal, SegmentReader};
use crate::api::hebrew_tokenizer::HebrewTokenizer;

// ── Public data types ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct SearchResult {
    pub title: String,
    pub reference: String,
    pub text: String,
    pub id: u64,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
}

pub struct DocumentInput {
    pub id: u64,
    pub title: String,
    pub reference: String,
    pub topics: String,
    pub text: String,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
}

pub struct HighlightConfig {
    pub highlight_prefix: String,
    pub highlight_postfix: String,
    pub max_chars: u32,
}

pub struct SearchPageResult {
    pub total_count: u32,
    pub results: Vec<SearchResult>,
}

pub struct FacetCount {
    pub path: String,
    pub count: u64,
}

pub enum ResultsOrder {
    Catalogue,
    Relevance,
}

// ── SearchEngine ───────────────────────────────────────────────────────────────

const DEFAULT_WRITER_HEAP_SIZE: usize = 50_000_000;

pub struct SearchEngine {
    schema: Schema,
    index: Index,
    index_writer: Option<IndexWriter>,
    writer_heap_size: usize,
    index_reader: IndexReader,
}

impl SearchEngine {
    #[frb(sync)]
    pub fn new(path: &str) -> Self {
        debug!("new path={}", path);
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field(
            "text",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("hebrew")
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                )
                .set_stored()
                .set_fast(None),
        );
        schema_builder.add_text_field("reference", STORED);
        schema_builder.add_text_field(
            "title",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("raw")
                        .set_fieldnorms(false),
                )
                .set_stored(),
        );
        // INDEXED is required for delete_term / upsert by id to work.
        schema_builder.add_u64_field("id", STORED | FAST | INDEXED);
        schema_builder.add_u64_field("segment", STORED);
        schema_builder.add_bool_field("isPdf", STORED);
        schema_builder.add_text_field("filePath", STRING | FAST | STORED);
        schema_builder.add_facet_field("topics", FacetOptions::default());

        let schema = schema_builder.build();
        let mmap_directory = MmapDirectory::open(path).expect("unable to open mmap directory");
        let index =
            Index::open_or_create(mmap_directory, schema.clone()).expect("Failed to create index");
        index.tokenizers().register("hebrew", HebrewTokenizer);
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .expect("Failed to create index reader");
        let index_writer = index
            .writer(DEFAULT_WRITER_HEAP_SIZE)
            .expect("Failed to create index writer");

        SearchEngine {
            schema,
            index,
            index_writer: Some(index_writer),
            writer_heap_size: DEFAULT_WRITER_HEAP_SIZE,
            index_reader,
        }
    }

    // ── Write API ──────────────────────────────────────────────────────────────

    /// Add a single document. Does not commit.
    pub fn add_document(
        &mut self,
        _id: u64,
        _title: &str,
        _reference: &str,
        _topics: &str,
        _text: &str,
        _segment: u64,
        _is_pdf: bool,
        _file_path: &str,
    ) -> Result<()> {
        let (title_f, reference_f, text_f, id_f, segment_f, is_pdf_f, file_path_f, topics_f) =
            self.all_fields()?;
        let topics_facet = Facet::from_text(_topics)?;
        self.writer_mut()?.add_document(doc!(
            title_f     => _title,
            reference_f => _reference,
            text_f      => _text,
            id_f        => _id,
            segment_f   => _segment,
            is_pdf_f    => _is_pdf,
            file_path_f => _file_path,
            topics_f    => topics_facet
        ))?;
        Ok(())
    }

    /// Add many documents in a single FFI call. Does not commit.
    /// For initial bulk loads – no duplicate checking.
    pub fn add_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        let (title_f, reference_f, text_f, id_f, segment_f, is_pdf_f, file_path_f, topics_f) =
            self.all_fields()?;
        let writer = self.writer_mut()?;
        for doc in docs {
            let topics_facet = Facet::from_text(&doc.topics)?;
            writer.add_document(doc!(
                title_f     => doc.title,
                reference_f => doc.reference,
                text_f      => doc.text,
                id_f        => doc.id,
                segment_f   => doc.segment,
                is_pdf_f    => doc.is_pdf,
                file_path_f => doc.file_path,
                topics_f    => topics_facet
            ))?;
        }
        Ok(())
    }

    /// Delete then re-insert a single document by id. Does not commit.
    pub fn upsert_document(
        &mut self,
        _id: u64,
        _title: &str,
        _reference: &str,
        _topics: &str,
        _text: &str,
        _segment: u64,
        _is_pdf: bool,
        _file_path: &str,
    ) -> Result<()> {
        self.delete_document_by_id(_id)?;
        self.add_document(
            _id, _title, _reference, _topics, _text, _segment, _is_pdf, _file_path,
        )
    }

    /// Upsert many documents in a single FFI call. Does not commit.
    pub fn upsert_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        let (title_f, reference_f, text_f, id_f, segment_f, is_pdf_f, file_path_f, topics_f) =
            self.all_fields()?;
        let writer = self.writer_mut()?;
        for doc in docs {
            writer.delete_term(Term::from_field_u64(id_f, doc.id));
            let topics_facet = Facet::from_text(&doc.topics)?;
            writer.add_document(doc!(
                title_f     => doc.title,
                reference_f => doc.reference,
                text_f      => doc.text,
                id_f        => doc.id,
                segment_f   => doc.segment,
                is_pdf_f    => doc.is_pdf,
                file_path_f => doc.file_path,
                topics_f    => topics_facet
            ))?;
        }
        Ok(())
    }

    /// Delete a document by its numeric id. Does not commit.
    pub fn delete_document_by_id(&mut self, id: u64) -> Result<()> {
        let id_f = self.schema.get_field("id").unwrap();
        self.writer_mut()?
            .delete_term(Term::from_field_u64(id_f, id));
        Ok(())
    }

    /// Delete all documents matching a title. Does not commit.
    /// Kept for backward compatibility – prefer delete_document_by_id.
    pub fn remove_documents_by_title(&mut self, title: &str) -> Result<()> {
        let title_field = self.schema.get_field("title")?;
        self.writer_mut()?
            .delete_term(Term::from_field_text(title_field, title));
        Ok(())
    }

    /// Delete all documents. Does not commit.
    pub fn clear(&mut self) -> Result<()> {
        self.writer_mut()?.delete_all_documents()?;
        Ok(())
    }

    /// Flush pending writes to disk and refresh the reader.
    pub fn commit(&mut self) -> Result<()> {
        self.writer_mut()?.commit()?;
        self.index_reader.reload()?;
        Ok(())
    }

    /// Discard all pending writes since the last commit.
    pub fn rollback(&mut self) -> Result<()> {
        self.writer_mut()?.rollback()?;
        Ok(())
    }

    // ── Search API ─────────────────────────────────────────────────────────────

    pub fn search(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        slop: u32,
        max_expansions: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<Vec<SearchResult>> {
        let query = Self::build_query(&self.index, regex_terms, facets, slop, max_expansions)?;
        let searcher = self.index_reader.searcher();
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        let addresses = Self::collect_addresses(&searcher, &*query, limit, offset, &order)?;
        Self::build_results(&self.schema, &searcher, &*query, addresses, &hl)
    }

    /// Search and return total hit count alongside paged results in one call.
    /// Uses a tuple collector so Tantivy executes a single index pass.
    pub fn search_and_count(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        slop: u32,
        max_expansions: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<SearchPageResult> {
        let query = Self::build_query(&self.index, regex_terms, facets, slop, max_expansions)?;
        let searcher = self.index_reader.searcher();
        let hl = highlight.unwrap_or_else(HighlightConfig::default);

        // Tuple collector: single index pass for both count and top-docs.
        let (addresses, total_count): (Vec<DocAddress>, u32) = match order {
            ResultsOrder::Catalogue => {
                let top_collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_fast_field::<u64>("id", Order::Asc);
                let (top_docs, count) = searcher.search(&*query, &(top_collector, Count))?;
                let addrs = top_docs.into_iter().map(|(_, addr)| addr).collect();
                (addrs, count as u32)
            }
            ResultsOrder::Relevance => {
                let top_collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_score();
                let (top_docs, count) = searcher.search(&*query, &(top_collector, Count))?;
                let addrs = top_docs.into_iter().map(|(_, addr)| addr).collect();
                (addrs, count as u32)
            }
        };

        let results = Self::build_results(&self.schema, &searcher, &*query, addresses, &hl)?;
        Ok(SearchPageResult {
            total_count,
            results,
        })
    }

    pub fn count(
        &mut self,
        regex_terms: Vec<String>,
        facets: &Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<u32> {
        let query = Self::build_query(
            &self.index,
            regex_terms,
            facets.clone(),
            slop,
            max_expansions,
        )?;
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&*query, &Count)? as u32)
    }

    pub fn count_by_book(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<HashMap<String, u32>> {
        let query = Self::build_query(&self.index, regex_terms, facets, slop, max_expansions)?;
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&*query, &BookCountCollector)?)
    }

    /// Return per-child facet counts for a given prefix (e.g. "/").
    pub fn get_facet_counts(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        facet_prefix: String,
        slop: u32,
        max_expansions: u32,
    ) -> Result<Vec<FacetCount>> {
        let query = Self::build_query(&self.index, regex_terms, facets, slop, max_expansions)?;
        let searcher = self.index_reader.searcher();
        let mut facet_collector = FacetCollector::for_field("topics");
        facet_collector.add_facet(&facet_prefix);
        let facet_counts = searcher.search(&*query, &facet_collector)?;
        // FacetCounts::get<T> requires Facet: From<T>; &str satisfies this.
        let results = facet_counts
            .get(facet_prefix.as_str())
            .map(|(f, count)| FacetCount {
                path: f.to_string(),
                count,
            })
            .collect();
        Ok(results)
    }

    // ── Operational API ────────────────────────────────────────────────────────

    /// Merge all segments into one. Run occasionally in the background after
    /// many upserts/deletes to reclaim disk space and improve read performance.
    /// Run only after `commit()`, because only committed segments participate in
    /// manual merge maintenance.
    pub fn optimize(&mut self) -> Result<()> {
        let before_count = self.index.searchable_segment_ids()?.len();
        debug!("optimize: before={before_count}");
        if before_count <= 1 {
            debug!("optimize: skipped");
            return Ok(());
        }

        let writer = self.take_writer()?;
        let maintenance_result = (|| -> Result<()> {
            writer.wait_merging_threads()?;
            self.optimize_committed_segments()
        })();
        let restore_result = self.restore_writer();

        if let Err(restore_err) = restore_result {
            return match maintenance_result {
                Ok(_) => Err(restore_err),
                Err(maintenance_err) => Err(restore_err.context(format!(
                    "optimize maintenance also failed: {maintenance_err:#}"
                ))),
            };
        }

        maintenance_result?;
        self.index_reader.reload()?;
        let after_count = self.index.searchable_segment_ids()?.len();
        debug!("optimize: after={after_count}");
        Ok(())
    }

    pub fn get_document_count(&self) -> u64 {
        self.index_reader.searcher().num_docs()
    }

    pub fn get_segment_count(&self) -> Result<u32> {
        Ok(self.index.searchable_segment_ids()?.len() as u32)
    }

    /// Fetch a single document by its numeric id. Returns None if not found.
    /// The `text` field contains the raw stored text (no snippet/highlight).
    pub fn get_document_by_id(&self, id: u64) -> Result<Option<SearchResult>> {
        let id_f = self.schema.get_field("id")?;
        let term = Term::from_field_u64(id_f, id);
        let query = TermQuery::new(term, IndexRecordOption::Basic);
        let searcher = self.index_reader.searcher();

        let top_docs = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, addr)) = top_docs.into_iter().next() else {
            return Ok(None);
        };

        let doc = searcher.doc::<TantivyDocument>(addr)?;
        let title_f = self.schema.get_field("title")?;
        let reference_f = self.schema.get_field("reference")?;
        let text_f = self.schema.get_field("text")?;
        let segment_f = self.schema.get_field("segment")?;
        let is_pdf_f = self.schema.get_field("isPdf")?;
        let file_path_f = self.schema.get_field("filePath")?;

        Ok(Some(SearchResult {
            title: doc
                .get_first(title_f)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            reference: doc
                .get_first(reference_f)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            text: doc
                .get_first(text_f)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            id,
            segment: doc
                .get_first(segment_f)
                .and_then(|v| v.as_u64())
                .unwrap_or_default(),
            is_pdf: doc
                .get_first(is_pdf_f)
                .and_then(|v| v.as_bool())
                .unwrap_or_default(),
            file_path: doc
                .get_first(file_path_f)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        }))
    }

    /// Fuzzy (Levenshtein) search on plain text terms.
    /// Unlike `search()` which requires regex patterns, this accepts plain words
    /// and matches terms within `max_distance` edits (0 = exact, 1–2 = fuzzy).
    /// Multiple terms are ANDed together; each term is matched fuzzily.
    pub fn search_fuzzy(
        &mut self,
        terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<Vec<SearchResult>> {
        let searcher = self.index_reader.searcher();
        let text_f = self.schema.get_field("text")?;
        let topics_f = self.schema.get_field("topics")?;
        let hl = highlight.unwrap_or_else(HighlightConfig::default);

        // Build a fuzzy sub-query per term, ANDed together.
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = terms
            .iter()
            .map(|t| {
                let term = Term::from_field_text(text_f, t);
                let fq: Box<dyn Query> = Box::new(FuzzyTermQuery::new(term, max_distance, true));
                (Occur::Must, fq)
            })
            .collect();

        // Add facet filter (same as regular search).
        let facet_terms: Vec<Term> = facets
            .iter()
            .map(|f| Term::from_facet(topics_f, &Facet::from_text(f).unwrap()))
            .collect();
        clauses.push((Occur::Must, Box::new(TermSetQuery::new(facet_terms))));

        let query: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
        let addresses = Self::collect_addresses(&searcher, &*query, limit, offset, &order)?;
        Self::build_results(&self.schema, &searcher, &*query, addresses, &hl)
    }

    /// Stream search results in chunks of `chunk_size` documents.
    ///
    /// The TopDocs phase (scoring and ranking) completes upfront – this is
    /// inherent to how Tantivy's collectors work and cannot be avoided without
    /// a custom collector. What IS incremental is the stored-document retrieval
    /// and snippet generation: the Dart side receives the first chunk of results
    /// as soon as those are ready, without waiting for all snippets to be built.
    /// This is useful when `limit` is large and snippet generation is the
    /// bottleneck. For typical limits (≤ 200) the difference is negligible.
    pub fn search_stream(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        slop: u32,
        max_expansions: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
        chunk_size: u32,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let query = Self::build_query(&self.index, regex_terms, facets, slop, max_expansions)?;
        let searcher = self.index_reader.searcher();
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        let chunk_size = (chunk_size.max(1)) as usize;

        let addresses = Self::collect_addresses(&searcher, &*query, limit, offset, &order)?;

        for chunk in addresses.chunks(chunk_size) {
            let results =
                Self::build_results(&self.schema, &searcher, &*query, chunk.to_vec(), &hl)?;
            // If the Dart side cancelled the stream, stop early.
            if sink.add(results).is_err() {
                break;
            }
        }
        Ok(())
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn all_fields(&self) -> Result<(Field, Field, Field, Field, Field, Field, Field, Field)> {
        Ok((
            self.schema.get_field("title")?,
            self.schema.get_field("reference")?,
            self.schema.get_field("text")?,
            self.schema.get_field("id")?,
            self.schema.get_field("segment")?,
            self.schema.get_field("isPdf")?,
            self.schema.get_field("filePath")?,
            self.schema.get_field("topics")?,
        ))
    }

    fn ensure_writer(&mut self) -> Result<()> {
        if self.index_writer.is_none() {
            debug!("writer: reopening lazily");
            self.index_writer = Some(self.open_writer()?);
        }
        Ok(())
    }

    fn writer_mut(&mut self) -> Result<&mut IndexWriter> {
        self.ensure_writer()?;
        self.index_writer
            .as_mut()
            .context("index writer is not available")
    }

    fn take_writer(&mut self) -> Result<IndexWriter> {
        self.ensure_writer()?;
        self.index_writer
            .take()
            .context("index writer is not available")
    }

    fn open_writer(&self) -> Result<IndexWriter> {
        Ok(self.index.writer(self.writer_heap_size)?)
    }

    fn open_writer_no_merge(&self) -> Result<IndexWriter> {
        let writer = self.open_writer()?;
        writer.set_merge_policy(Box::new(NoMergePolicy));
        Ok(writer)
    }

    fn optimize_committed_segments(&self) -> Result<()> {
        let mut maintenance_writer = self.open_writer_no_merge()?;
        let segment_ids = self.index.searchable_segment_ids()?;
        debug!("optimize: merging {} segments", segment_ids.len());

        let merge_result = if segment_ids.len() > 1 {
            maintenance_writer.merge(&segment_ids).wait().map(|_| ())
        } else {
            Ok(())
        };
        let wait_result = maintenance_writer.wait_merging_threads();

        merge_result?;
        wait_result?;
        Ok(())
    }

    fn restore_writer(&mut self) -> Result<()> {
        self.index_writer = Some(self.open_writer()?);
        Ok(())
    }

    fn build_query(
        index: &Index,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<Box<dyn Query>> {
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let topics_field = schema.get_field("topics").unwrap();

        let main_query: Box<dyn Query> = if regex_terms.len() == 1 {
            Box::new(RegexQuery::from_pattern(&regex_terms[0], text_field)?)
        } else {
            let mut phrase_query = RegexPhraseQuery::new(text_field, regex_terms);
            phrase_query.set_slop(slop);
            phrase_query.set_max_expansions(max_expansions);
            Box::new(phrase_query)
        };

        let facet_terms: Vec<Term> = facets
            .iter()
            .map(|f| Term::from_facet(topics_field, &Facet::from_text(f).unwrap()))
            .collect();
        let facets_query = TermSetQuery::new(facet_terms);

        Ok(Box::new(BooleanQuery::new(vec![
            (Occur::Must, main_query),
            (Occur::Must, Box::new(facets_query) as Box<dyn Query>),
        ])))
    }

    fn collect_addresses(
        searcher: &Searcher,
        query: &dyn Query,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
    ) -> Result<Vec<DocAddress>> {
        let addresses = match order {
            ResultsOrder::Catalogue => {
                // and_offset is set on TopDocs before calling order_by_fast_field,
                // which consumes self and preserves the offset configuration.
                let collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_fast_field::<u64>("id", Order::Asc);
                searcher
                    .search(query, &collector)?
                    .into_iter()
                    .map(|(_, addr)| addr)
                    .collect()
            }
            ResultsOrder::Relevance => {
                let collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_score();
                searcher
                    .search(query, &collector)?
                    .into_iter()
                    .map(|(_, addr)| addr)
                    .collect()
            }
        };
        Ok(addresses)
    }

    fn build_results(
        schema: &Schema,
        searcher: &Searcher,
        query: &dyn Query,
        addresses: Vec<DocAddress>,
        hl: &HighlightConfig,
    ) -> Result<Vec<SearchResult>> {
        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        let text_field = schema.get_field("text")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;

        let mut snippet_generator = SnippetGenerator::create(searcher, query, text_field)?;
        snippet_generator.set_max_num_chars(hl.max_chars as usize);

        let mut results = Vec::with_capacity(addresses.len());
        for doc_address in addresses {
            let retrieved_doc = match searcher.doc::<TantivyDocument>(doc_address) {
                Ok(d) => d,
                Err(_) => continue,
            };

            let title = retrieved_doc
                .get_first(title_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let reference = retrieved_doc
                .get_first(reference_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let text = retrieved_doc
                .get_first(text_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let id = retrieved_doc
                .get_first(id_field)
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let segment = retrieved_doc
                .get_first(segment_field)
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let is_pdf = retrieved_doc
                .get_first(is_pdf_field)
                .and_then(|v| v.as_bool())
                .unwrap_or_default();
            let file_path = retrieved_doc
                .get_first(file_path_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let mut snippet = snippet_generator.snippet(&text);
            snippet.set_snippet_prefix_postfix(&hl.highlight_prefix, &hl.highlight_postfix);
            let snippet_html = snippet.to_html();
            let result_text = if snippet_html.is_empty() {
                text
            } else {
                snippet_html
            };

            results.push(SearchResult {
                title,
                reference,
                text: result_text,
                id,
                segment,
                is_pdf,
                file_path,
            });
        }
        Ok(results)
    }
}

impl HighlightConfig {
    fn default() -> Self {
        HighlightConfig {
            highlight_prefix: "<font color=red>".to_string(),
            highlight_postfix: "</font>".to_string(),
            max_chars: 800,
        }
    }
}

// ── BookCountCollector ─────────────────────────────────────────────────────────

/// Counts matching documents grouped by `filePath` fast field.
/// Per-segment counts use term ordinals; strings are decoded only in harvest().
struct BookCountCollector;

struct BookCountSegmentCollector {
    str_col: Option<tantivy::columnar::StrColumn>,
    counts: HashMap<u64, u32>,
}

impl Collector for BookCountCollector {
    type Fruit = HashMap<String, u32>;
    type Child = BookCountSegmentCollector;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<BookCountSegmentCollector> {
        let str_col = reader.fast_fields().str("filePath")?;
        Ok(BookCountSegmentCollector {
            str_col,
            counts: HashMap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        per_segment: Vec<tantivy::Result<HashMap<String, u32>>>,
    ) -> tantivy::Result<HashMap<String, u32>> {
        let mut merged: HashMap<String, u32> = HashMap::new();
        for seg_result in per_segment {
            for (path, count) in seg_result? {
                *merged.entry(path).or_insert(0) += count;
            }
        }
        Ok(merged)
    }
}

impl SegmentCollector for BookCountSegmentCollector {
    type Fruit = tantivy::Result<HashMap<String, u32>>;

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        if let Some(col) = &self.str_col {
            if let Some(term_ord) = col.term_ords(doc_id).next() {
                *self.counts.entry(term_ord).or_insert(0) += 1;
            }
        }
    }

    fn harvest(self) -> tantivy::Result<HashMap<String, u32>> {
        let Some(col) = self.str_col else {
            return Ok(HashMap::new());
        };
        let mut result = HashMap::with_capacity(self.counts.len());
        let mut buf = String::new();
        for (term_ord, count) in self.counts {
            buf.clear();
            if col.ord_to_str(term_ord, &mut buf)? {
                result.insert(buf.clone(), count);
            }
        }
        Ok(result)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_engine() -> (SearchEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = SearchEngine::new(dir.path().to_str().unwrap());
        (engine, dir)
    }

    fn add(engine: &mut SearchEngine, id: u64, text: &str, file_path: &str) {
        engine
            .add_document(id, "title", "ref", "/root", text, 0, false, file_path)
            .unwrap();
    }

    fn disable_auto_merge(engine: &SearchEngine) {
        engine
            .index_writer
            .as_ref()
            .unwrap()
            .set_merge_policy(Box::new(NoMergePolicy));
    }

    fn search_ids(engine: &mut SearchEngine, term: &str) -> Vec<u64> {
        engine
            .search(
                vec![term.to_string()],
                vec!["/root".to_string()],
                100,
                0,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap()
            .into_iter()
            .map(|result| result.id)
            .collect()
    }

    #[test]
    fn test_count_by_book_basic() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn test_count_by_book_empty_result() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book(vec!["ביי".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        assert!(counts.is_empty());
    }

    #[test]
    fn test_count_by_book_no_cross_contamination() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום ביי", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book(vec!["עולם".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        assert_eq!(counts.get("/books/a.txt").copied(), Some(1));
        assert_eq!(counts.get("/books/b.txt"), None);
    }

    #[test]
    fn test_count_by_book_multi_segment() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();

        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn test_delete_document_by_id() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        engine.commit().unwrap();

        assert_eq!(
            engine
                .count(vec!["שלום".to_string()], &vec!["/root".to_string()], 0, 100)
                .unwrap(),
            2
        );

        engine.delete_document_by_id(1).unwrap();
        engine.commit().unwrap();

        assert_eq!(
            engine
                .count(vec!["שלום".to_string()], &vec!["/root".to_string()], 0, 100)
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_upsert_document() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "טקסט ישן", "/books/a.txt");
        engine.commit().unwrap();

        engine
            .upsert_document(
                1,
                "title",
                "ref",
                "/root",
                "טקסט חדש",
                0,
                false,
                "/books/a.txt",
            )
            .unwrap();
        engine.commit().unwrap();

        // Should have only one doc with id=1
        assert_eq!(
            engine
                .count(vec!["טקסט".to_string()], &vec!["/root".to_string()], 0, 100)
                .unwrap(),
            1
        );
        assert_eq!(
            engine
                .count(vec!["ישן".to_string()], &vec!["/root".to_string()], 0, 100)
                .unwrap(),
            0
        );
        assert_eq!(
            engine
                .count(vec!["חדש".to_string()], &vec!["/root".to_string()], 0, 100)
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_rollback() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();

        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        engine.rollback().unwrap();
        engine.commit().unwrap();

        // doc 2 should not be present
        assert_eq!(
            engine
                .count(vec!["שלום".to_string()], &vec!["/root".to_string()], 0, 100)
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_get_document_count() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        add(&mut engine, 2, "עולם", "/books/b.txt");
        engine.commit().unwrap();
        assert_eq!(engine.get_document_count(), 2);
    }

    #[test]
    fn test_get_document_by_id_found() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 42, "תורה ומצוות", "/books/a.txt");
        engine.commit().unwrap();

        let result = engine.get_document_by_id(42).unwrap();
        assert!(result.is_some());
        let doc = result.unwrap();
        assert_eq!(doc.id, 42);
        assert_eq!(doc.text, "תורה ומצוות");
    }

    #[test]
    fn test_get_document_by_id_not_found() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        let result = engine.get_document_by_id(999).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_search_fuzzy() {
        let (mut engine, _dir) = make_engine();
        // "שלום" exact match; "שלם" is one edit away (deletion); "ביי" is unrelated
        add(&mut engine, 1, "שלום", "/books/a.txt");
        add(&mut engine, 2, "שלם", "/books/b.txt");
        add(&mut engine, 3, "ביי", "/books/c.txt");
        engine.commit().unwrap();

        // distance=0: only exact match
        let exact = engine
            .search_fuzzy(
                vec!["שלום".to_string()],
                vec!["/root".to_string()],
                10,
                0,
                0,
                ResultsOrder::Relevance,
                None,
            )
            .unwrap();
        let exact_texts: Vec<&str> = exact.iter().map(|r| r.text.as_str()).collect();
        assert!(
            exact_texts.contains(&"שלום"),
            "distance=0 must return exact match"
        );
        assert!(
            !exact_texts.contains(&"שלם"),
            "distance=0 must not return near-match"
        );

        // distance=1: must return both "שלום" and the near-match "שלם"
        let fuzzy = engine
            .search_fuzzy(
                vec!["שלום".to_string()],
                vec!["/root".to_string()],
                10,
                0,
                1,
                ResultsOrder::Relevance,
                None,
            )
            .unwrap();
        let fuzzy_texts: Vec<&str> = fuzzy.iter().map(|r| r.text.as_str()).collect();
        assert!(
            fuzzy_texts.contains(&"שלום"),
            "distance=1 must return exact match"
        );
        assert!(
            fuzzy_texts.contains(&"שלם"),
            "distance=1 must return near-match one edit away"
        );
        assert!(
            !fuzzy_texts.contains(&"ביי"),
            "unrelated term must not appear"
        );
    }

    #[test]
    fn test_search_and_count() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "ביי", "/books/b.txt");
        engine.commit().unwrap();

        let page = engine
            .search_and_count(
                vec!["שלום".to_string()],
                vec!["/root".to_string()],
                1,
                0,
                0,
                100,
                ResultsOrder::Relevance,
                None,
            )
            .unwrap();

        assert_eq!(
            page.total_count, 2,
            "total_count should reflect all hits, not just page size"
        );
        assert_eq!(
            page.results.len(),
            1,
            "results should be limited by limit param"
        );
    }

    #[test]
    fn test_search_offset() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/b.txt");
        add(&mut engine, 3, "שלום חבר", "/books/c.txt");
        engine.commit().unwrap();

        let page1 = engine
            .search(
                vec!["שלום".to_string()],
                vec!["/root".to_string()],
                2,
                0,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap();
        let page2 = engine
            .search(
                vec!["שלום".to_string()],
                vec!["/root".to_string()],
                2,
                2,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap();

        assert_eq!(page1.len(), 2);
        assert_eq!(page2.len(), 1);
        // Pages must not overlap
        let ids1: Vec<u64> = page1.iter().map(|r| r.id).collect();
        let ids2: Vec<u64> = page2.iter().map(|r| r.id).collect();
        assert!(ids1.iter().all(|id| !ids2.contains(id)));
    }

    #[test]
    fn test_optimize_reduces_segments_many_commits() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);

        for id in 1..=12 {
            let text = format!("שלום {id}");
            let file_path = format!("/books/{id}.txt");
            add(&mut engine, id, &text, &file_path);
            engine.commit().unwrap();
        }

        let before = engine.get_segment_count().unwrap();
        assert!(before > 1, "test setup should create multiple segments");

        engine.optimize().unwrap();

        let after = engine.get_segment_count().unwrap();

        assert!(
            after <= before,
            "optimize should not increase segment count"
        );
        assert_eq!(
            after, 1,
            "after optimize there should be exactly one segment"
        );
        assert_eq!(engine.get_document_count(), 12);
    }

    #[test]
    fn test_optimize_preserves_search_results() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);

        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "שלום רב", "/books/b.txt");
        engine.commit().unwrap();
        add(&mut engine, 3, "ביי", "/books/c.txt");
        engine.commit().unwrap();
        add(&mut engine, 4, "שלום חבר", "/books/d.txt");
        engine.commit().unwrap();

        let before_ids = search_ids(&mut engine, "שלום");
        engine.optimize().unwrap();
        let after_ids = search_ids(&mut engine, "שלום");

        assert_eq!(
            before_ids, after_ids,
            "optimize must preserve search results"
        );
    }

    #[test]
    fn test_optimize_preserves_upsert_and_delete_afterwards() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);

        add(&mut engine, 1, "טקסט ישן", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "למחיקה", "/books/b.txt");
        engine.commit().unwrap();

        engine.optimize().unwrap();

        engine
            .upsert_document(
                1,
                "title",
                "ref",
                "/root",
                "טקסט חדש",
                0,
                false,
                "/books/a.txt",
            )
            .unwrap();
        engine.delete_document_by_id(2).unwrap();
        engine.commit().unwrap();

        assert_eq!(search_ids(&mut engine, "ישן"), Vec::<u64>::new());
        assert_eq!(search_ids(&mut engine, "חדש"), vec![1]);
        assert!(engine.get_document_by_id(2).unwrap().is_none());
    }

    #[test]
    fn test_optimize_noop_when_single_segment() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        let before = engine.get_segment_count().unwrap();
        engine.optimize().unwrap();
        let after = engine.get_segment_count().unwrap();

        assert_eq!(before, 1);
        assert_eq!(after, 1);

        add(&mut engine, 2, "עולם", "/books/b.txt");
        engine.commit().unwrap();
        assert_eq!(search_ids(&mut engine, "עולם"), vec![2]);
    }

    #[test]
    fn test_writer_reopens_after_transient_reopen_failure() {
        let (mut engine, _dir) = make_engine();

        engine.index_writer = None;
        let competing_writer: IndexWriter<TantivyDocument> =
            engine.index.writer(DEFAULT_WRITER_HEAP_SIZE).unwrap();

        let err = engine
            .add_document(1, "title", "ref", "/root", "שלום", 0, false, "/books/a.txt")
            .unwrap_err();
        assert!(
            err.to_string().contains("Failed to acquire index lock")
                || err.to_string().contains("LockFailure"),
            "unexpected error: {err:#}"
        );
        assert!(engine.index_writer.is_none());

        drop(competing_writer);

        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        assert_eq!(search_ids(&mut engine, "שלום"), vec![1]);
    }

    #[test]
    fn test_clear_reopens_after_transient_reopen_failure() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        engine.index_writer = None;
        let competing_writer: IndexWriter<TantivyDocument> =
            engine.index.writer(DEFAULT_WRITER_HEAP_SIZE).unwrap();

        let err = engine.clear().unwrap_err();
        assert!(
            err.to_string().contains("Failed to acquire index lock")
                || err.to_string().contains("LockFailure"),
            "unexpected error: {err:#}"
        );
        assert!(engine.index_writer.is_none());

        drop(competing_writer);

        engine.clear().unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.get_document_count(), 0);
        assert_eq!(search_ids(&mut engine, "שלום"), Vec::<u64>::new());
    }
}
