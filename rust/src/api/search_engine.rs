use crate::frb_generated::StreamSink;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA, SINK_STATE};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tantivy::collector::{Collector, Count, FacetCollector, SegmentCollector, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::indexer::NoMergePolicy;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, EmptyQuery, FuzzyTermQuery, Occur,
    PhraseQuery, TermQuery, TermSetQuery,
};
use tantivy::query::{Query, RegexPhraseQuery};
use tantivy::schema::Value;
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{LowerCaser, TextAnalyzer, TokenStream};
use tantivy::{doc, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher};
use tantivy::{schema::*, Index};
use tantivy::{DocId, SegmentOrdinal, SegmentReader};
use tantivy_fst::Automaton;

use crate::display_highlight;
use crate::hebrew_query;
use crate::hebrew_tokenizer::HebrewTokenizer;
use crate::magic::{MagicDictionary, MAX_LEXICAL_FORMS};

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

#[derive(Clone)]
pub struct IndexCompatibility {
    pub compatible: bool,
    pub status: String,
    pub found_schema_version: Option<u32>,
    pub required_schema_version: u32,
    pub engine_version: String,
    pub metadata_path: String,
    pub reason: Option<String>,
}

pub enum ResultsOrder {
    Catalogue,
    Relevance,
}

/// Regex patterns for highlighting query matches in *displayed* book text
/// (which, unlike index terms, still carries nikud and HTML). All patterns
/// are ECMAScript-dialect strings; the Dart layer compiles them with
/// `RegExp(pattern, caseSensitive: false)` and performs no pattern
/// construction of its own.
pub struct HighlightPattern {
    /// One regex matching the full query phrase (words + separators).
    pub combined_pattern: String,
    /// Per-word regex, used to locate each word inside a combined match.
    pub word_patterns: Vec<String>,
    /// Per-word: `true` when the word has no morphological expansion option,
    /// so the UI may require token boundaries around its match.
    pub word_boundary_eligible: Vec<bool>,
}

// ── SearchEngine ───────────────────────────────────────────────────────────────

const DEFAULT_WRITER_HEAP_SIZE: usize = 50_000_000;
const INDEX_METADATA_FILE_NAME: &str = "otzaria_index_meta.json";
const INDEX_FORMAT: &str = "otzaria-search-index";
// גרסה 3: המעבר ל-HebrewTokenizer (גרשיים/גרש נשמרים בטוקנים) משנה את
// מילון הטרמים — אינדקסים ישנים חייבים בנייה מחדש.
const INDEX_SCHEMA_VERSION: u32 = 3;
const TANTIVY_INDEX_VERSION: &str = "0.26.1";

/// Upper bound on distinct dictionary terms collected for highlighting an
/// advanced (regex) query. Bounds work when a pattern (e.g. partial match)
/// expands very widely; far more matches than a snippet could ever show.
const MAX_HIGHLIGHT_TERMS: usize = 512;
const MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN: usize = 256;
const LEXICAL_FUZZY_PHRASE_SLOP: u32 = 1;

// Relevance weights for the approximate (`fuzzy`) path. `FuzzyTermQuery` and
// `TermSetQuery` are automaton queries that score a flat 1.0 (`ConstScorer`),
// so without boosting every approximate hit ties and `order_by_score` produces
// no visible ordering. These tiers make `ResultsOrder::Relevance` meaningful:
// an exact-token hit outranks a dictionary-morphology relative, which outranks
// a bare edit-distance match.
//
// The exact tier is built from TWO clauses that sum (`BooleanQuery` sums
// `Should` scores): a `ConstScoreQuery` floor (`FUZZY_BOOST_EXACT`) plus a small
// BM25 `TermQuery` (`FUZZY_BOOST_EXACT_REL`) for intra-exact ordering. A plain
// boosted `TermQuery` would NOT suffice: BM25 `idf` collapses to ~0 for a term
// present in almost every document (`ln(1 + 0.5/(doc_freq+0.5))`), so a purely
// multiplicative boost could sink an exact hit below the flat lexical tier. The
// constant floor guarantees exact > lexical regardless of `doc_freq`, while the
// BM25 add-on still ranks rarer exact matches first within the top tier.
//
// These layers are added ONLY for `ResultsOrder::Relevance` (the `rank` flag).
// Count/catalogue paths build the bare recall query so they pay nothing for
// ranking they never use. Recall is unchanged either way (the exact term is a
// subset of the fuzzy automaton match), and exact/advanced never use these.
const FUZZY_BOOST_EXACT: Score = 1000.0;
const FUZZY_BOOST_EXACT_REL: Score = 1.0;
const FUZZY_BOOST_LEXICAL: Score = 30.0;
const FUZZY_BOOST_FUZZY: Score = 1.0;

/// The eight schema fields resolved together by [`SearchEngine::all_fields`]:
/// `(title, reference, text, id, segment, isPdf, filePath, topics)`.
type SchemaFields = (Field, Field, Field, Field, Field, Field, Field, Field);

#[derive(Serialize, Deserialize)]
struct IndexMetadata {
    format: String,
    schema_version: u32,
    engine_version: String,
    tantivy_version: String,
    created_at_unix_seconds: u64,
}

#[frb(sync)]
pub fn check_index_compatibility(path: String) -> IndexCompatibility {
    check_index_compatibility_path(Path::new(&path))
}

/// Builds display-highlight regex patterns for a search query, so the app can
/// mark matches inside an opened book exactly the way the engine matched them.
///
/// Pure string computation (no index access) — safe to call synchronously.
/// Parameters mirror [`SearchEngine::search_advanced`]: `distance` is the
/// default intermediate-word allowance, `custom_spacing` is keyed
/// `"i-(i+1)"`, `alternative_words` is keyed by word position, and
/// `search_options` is keyed `"{word}_{index}"` using the same tokenization
/// as engine queries.
///
/// Returns `None` when the query contains no highlightable words.
#[frb(sync)]
pub fn generate_highlight_pattern(
    query: String,
    distance: u32,
    custom_spacing: HashMap<String, String>,
    alternative_words: HashMap<u32, Vec<String>>,
    search_options: HashMap<String, HashMap<String, bool>>,
) -> Option<HighlightPattern> {
    display_highlight::build_display_highlight(
        &query,
        distance,
        &custom_spacing,
        &alternative_words,
        &search_options,
    )
    .map(|hl| HighlightPattern {
        combined_pattern: hl.combined_pattern,
        word_patterns: hl.word_patterns,
        word_boundary_eligible: hl.word_boundary_eligible,
    })
}

fn check_index_compatibility_path(index_path: &Path) -> IndexCompatibility {
    let metadata_path = index_metadata_path(index_path);

    if !index_path.exists() {
        return compatibility(
            false,
            "missing_index",
            None,
            metadata_path,
            Some("index directory does not exist".to_string()),
        );
    }

    if !index_path.is_dir() {
        return compatibility(
            false,
            "invalid_index_path",
            None,
            metadata_path,
            Some("index path is not a directory".to_string()),
        );
    }

    if metadata_path.exists() {
        return check_sidecar_metadata(metadata_path);
    }

    check_legacy_tantivy_metadata(index_path, metadata_path)
}

fn check_sidecar_metadata(metadata_path: PathBuf) -> IndexCompatibility {
    let raw = match fs::read_to_string(&metadata_path) {
        Ok(raw) => raw,
        Err(err) => {
            return compatibility(
                false,
                "invalid_metadata",
                None,
                metadata_path,
                Some(format!("failed to read metadata: {err}")),
            )
        }
    };

    let metadata: IndexMetadata = match serde_json::from_str(&raw) {
        Ok(metadata) => metadata,
        Err(err) => {
            return compatibility(
                false,
                "invalid_metadata",
                None,
                metadata_path,
                Some(format!("failed to parse metadata: {err}")),
            )
        }
    };

    if metadata.format != INDEX_FORMAT {
        return compatibility(
            false,
            "invalid_format",
            Some(metadata.schema_version),
            metadata_path,
            Some(format!("expected format {INDEX_FORMAT}")),
        );
    }

    if metadata.schema_version < INDEX_SCHEMA_VERSION {
        return compatibility(
            false,
            "rebuild_required",
            Some(metadata.schema_version),
            metadata_path,
            Some("index schema is older than the engine requires".to_string()),
        );
    }

    if metadata.schema_version > INDEX_SCHEMA_VERSION {
        return compatibility(
            false,
            "engine_too_old",
            Some(metadata.schema_version),
            metadata_path,
            Some("index schema is newer than this engine supports".to_string()),
        );
    }

    compatibility(
        true,
        "compatible",
        Some(metadata.schema_version),
        metadata_path,
        None,
    )
}

fn check_legacy_tantivy_metadata(index_path: &Path, metadata_path: PathBuf) -> IndexCompatibility {
    let tantivy_metadata_path = index_path.join("meta.json");
    if !tantivy_metadata_path.exists() {
        return compatibility(
            false,
            "missing_metadata",
            None,
            metadata_path,
            Some("otzaria metadata and Tantivy meta.json are missing".to_string()),
        );
    }

    let raw = match fs::read_to_string(&tantivy_metadata_path) {
        Ok(raw) => raw,
        Err(err) => {
            return compatibility(
                false,
                "invalid_tantivy_metadata",
                None,
                metadata_path,
                Some(format!("failed to read Tantivy metadata: {err}")),
            )
        }
    };

    let tantivy_metadata: JsonValue = match serde_json::from_str(&raw) {
        Ok(metadata) => metadata,
        Err(err) => {
            return compatibility(
                false,
                "invalid_tantivy_metadata",
                None,
                metadata_path,
                Some(format!("failed to parse Tantivy metadata: {err}")),
            )
        }
    };

    if tantivy_schema_matches_current_version(&tantivy_metadata) {
        return compatibility(
            true,
            "legacy_compatible",
            Some(INDEX_SCHEMA_VERSION),
            metadata_path,
            Some(
                "otzaria metadata is missing, but Tantivy schema matches the current engine"
                    .to_string(),
            ),
        );
    }

    compatibility(
        false,
        "rebuild_required",
        inferred_legacy_schema_version(&tantivy_metadata),
        metadata_path,
        Some("otzaria metadata is missing and Tantivy schema is not compatible".to_string()),
    )
}

/// Compares the full on-disk schema against the engine's current one — the
/// same equality `Index::open_or_create` enforces — so a legacy index can't
/// pass the check (e.g. on the `id` field alone) and then fail to open.
fn tantivy_schema_matches_current_version(metadata: &JsonValue) -> bool {
    let Some(schema_json) = metadata.get("schema") else {
        return false;
    };
    match serde_json::from_value::<Schema>(schema_json.clone()) {
        Ok(found_schema) => found_schema == current_schema(),
        Err(_) => false,
    }
}

fn inferred_legacy_schema_version(metadata: &JsonValue) -> Option<u32> {
    let schema = metadata.get("schema")?.as_array()?;
    let id_field = schema.iter().find(|field| {
        field.get("name").and_then(JsonValue::as_str) == Some("id")
            && field.get("type").and_then(JsonValue::as_str) == Some("u64")
    })?;
    if id_field
        .pointer("/options/indexed")
        .and_then(JsonValue::as_bool)
        == Some(false)
    {
        Some(1)
    } else {
        None
    }
}

fn ensure_current_index_metadata(index_path: &Path) -> Result<()> {
    let compatibility = check_index_compatibility_path(index_path);
    if compatibility.compatible && compatibility.status != "compatible" {
        write_current_index_metadata(index_path)?;
    }
    Ok(())
}

fn write_current_index_metadata(index_path: &Path) -> Result<()> {
    let metadata_path = index_metadata_path(index_path);
    let serialized = serde_json::to_string_pretty(&current_index_metadata())?;
    fs::write(&metadata_path, format!("{serialized}\n")).with_context(|| {
        format!(
            "failed to write index metadata to {}",
            metadata_path.display()
        )
    })
}

fn current_index_metadata() -> IndexMetadata {
    IndexMetadata {
        format: INDEX_FORMAT.to_string(),
        schema_version: INDEX_SCHEMA_VERSION,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        tantivy_version: TANTIVY_INDEX_VERSION.to_string(),
        created_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    }
}

fn index_metadata_path(index_path: &Path) -> PathBuf {
    index_path.join(INDEX_METADATA_FILE_NAME)
}

fn compatibility(
    compatible: bool,
    status: &str,
    found_schema_version: Option<u32>,
    metadata_path: PathBuf,
    reason: Option<String>,
) -> IndexCompatibility {
    IndexCompatibility {
        compatible,
        status: status.to_string(),
        found_schema_version,
        required_schema_version: INDEX_SCHEMA_VERSION,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        metadata_path: metadata_path.display().to_string(),
        reason,
    }
}

/// The schema this engine version requires. Kept in one place so `new()` and
/// the legacy compatibility check can never drift apart.
fn current_schema() -> Schema {
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
    schema_builder.build()
}

pub struct SearchEngine {
    schema: Schema,
    index: Index,
    index_writer: Option<IndexWriter>,
    writer_heap_size: usize,
    index_reader: IndexReader,
    /// Optional lexical morphology lexicon for the approximate (`fuzzy`) path.
    /// `None` until [`SearchEngine::set_magic_dictionary_path`] loads a valid
    /// `lexical.db`; while `None`, fuzzy search behaves exactly as before.
    magic_dict: Option<MagicDictionary>,
}

impl SearchEngine {
    #[frb(sync)]
    pub fn new(path: &str) -> Self {
        debug!("new path={}", path);
        let schema = current_schema();
        let mmap_directory = MmapDirectory::open(path).expect("unable to open mmap directory");
        let index = match Index::open_or_create(mmap_directory, schema.clone()) {
            Ok(index) => index,
            Err(tantivy::TantivyError::SchemaError(err)) => panic!(
                "index at {path} was built with an incompatible schema ({err}); \
                 call check_index_compatibility before opening and rebuild the index"
            ),
            Err(err) => panic!("Failed to open index at {path}: {err}"),
        };
        index.tokenizers().register(
            "hebrew",
            TextAnalyzer::builder(HebrewTokenizer)
                .filter(LowerCaser)
                .build(),
        );
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .expect("Failed to create index reader");
        // Best-effort: if another instance/process holds the writer lock right
        // now, start without a writer; ensure_writer() retries on first write.
        let index_writer = match index.writer(DEFAULT_WRITER_HEAP_SIZE) {
            Ok(writer) => Some(writer),
            Err(err) => {
                warn!("writer unavailable at startup ({err}); will retry lazily");
                None
            }
        };

        if let Err(err) = ensure_current_index_metadata(Path::new(path)) {
            debug!("failed to ensure index metadata: {err:#}");
        }

        SearchEngine {
            schema,
            index,
            index_writer,
            writer_heap_size: DEFAULT_WRITER_HEAP_SIZE,
            index_reader,
            magic_dict: None,
        }
    }

    /// Loads a `lexical.db` morphology lexicon for the approximate (`fuzzy`)
    /// search path. Returns `true` if the file opened and has the expected
    /// schema, `false` if it is missing or unusable — in which case the engine
    /// keeps its existing fuzzy behaviour (no error is surfaced, so the app can
    /// call this unconditionally at startup). Does **not** affect exact or
    /// advanced search.
    #[frb(sync)]
    pub fn set_magic_dictionary_path(&mut self, path: String) -> bool {
        match MagicDictionary::open(Path::new(&path)) {
            Ok(dict) => {
                debug!("magic dictionary loaded from {path}");
                self.magic_dict = Some(dict);
                true
            }
            Err(err) => {
                warn!("magic dictionary unavailable at {path}: {err:#}");
                self.magic_dict = None;
                false
            }
        }
    }

    /// Whether a lexical dictionary is currently loaded (i.e. approximate search
    /// will use morphological expansion).
    #[frb(sync)]
    pub fn has_magic_dictionary(&self) -> bool {
        self.magic_dict.is_some()
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
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        slop: u32,
        max_expansions: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<Vec<SearchResult>> {
        let query = self.build_query(regex_terms, facets, slop, max_expansions)?;
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        self.run_search(query, |_| Ok(None), limit, offset, &order, &hl)
    }

    /// Search and return total hit count alongside paged results in one call.
    /// Uses a tuple collector so Tantivy executes a single index pass.
    pub fn search_and_count(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        slop: u32,
        max_expansions: u32,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<SearchPageResult> {
        let query = self.build_query(regex_terms, facets, slop, max_expansions)?;
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        self.run_search_and_count(query, |_| Ok(None), limit, offset, &order, &hl)
    }

    pub fn count(
        &self,
        regex_terms: Vec<String>,
        facets: &[String],
        slop: u32,
        max_expansions: u32,
    ) -> Result<u32> {
        let query = self.build_query(regex_terms, facets.to_vec(), slop, max_expansions)?;
        self.run_count(query)
    }

    pub fn count_by_book(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<HashMap<String, u32>> {
        let query = self.build_query(regex_terms, facets, slop, max_expansions)?;
        self.run_count_by_book(query)
    }

    /// Return per-child facet counts for a given prefix (e.g. "/").
    pub fn get_facet_counts(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        facet_prefix: String,
        slop: u32,
        max_expansions: u32,
    ) -> Result<Vec<FacetCount>> {
        let query = self.build_query(regex_terms, facets, slop, max_expansions)?;
        self.run_facet_counts(query, facet_prefix)
    }

    // ── Operational API ────────────────────────────────────────────────────────

    /// Merge all segments into one. Run occasionally in the background after
    /// many upserts/deletes to reclaim disk space and improve read performance.
    /// Pending (uncommitted) changes are committed first, since only committed
    /// segments participate in manual merge maintenance.
    pub fn optimize(&mut self) -> Result<()> {
        let before_count = self.index.searchable_segment_ids()?.len();
        debug!("optimize: before={before_count}");
        if before_count <= 1 {
            debug!("optimize: skipped");
            return Ok(());
        }

        let mut writer = self.take_writer()?;
        let maintenance_result = (|| -> Result<()> {
            // Dropping the writer discards its RAM buffer; flush pending
            // changes first so optimize never silently loses documents.
            writer.commit()?;
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

    /// Number of live (committed, non-deleted) documents per distinct
    /// `filePath` across the whole index.
    ///
    /// This is read from the index itself rather than from any external state,
    /// so callers can reconstruct indexing progress directly from an index —
    /// e.g. after pointing the engine at a directory that already contains an
    /// index built elsewhere — and compare it against the current library.
    pub fn count_documents_by_file_path(&self) -> Result<HashMap<String, u32>> {
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&AllQuery, &BookCountCollector)?)
    }

    /// Distinct `filePath` values present in the index — i.e. which books have
    /// at least one live document. Convenience wrapper over
    /// [`Self::count_documents_by_file_path`].
    pub fn get_indexed_file_paths(&self) -> Result<Vec<String>> {
        Ok(self.count_documents_by_file_path()?.into_keys().collect())
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

    /// Fuzzy (Levenshtein) search on pre-tokenized plain-text terms.
    /// Low-level primitive retained for tests and the example app; the
    /// high-level `search_fuzzy` accepts a raw query string instead.
    /// Multiple terms are ANDed together; each is matched within `max_distance`
    /// edits (0 = exact, 1–2 = fuzzy).
    pub fn search_fuzzy_terms(
        &self,
        terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
        highlight: Option<HighlightConfig>,
    ) -> Result<Vec<SearchResult>> {
        let rank = matches!(order, ResultsOrder::Relevance);
        let query = self.build_fuzzy_search_query(&terms, &facets, max_distance, rank)?;
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        self.run_search(
            query,
            |s| Ok(self.build_fuzzy_highlight(s, &terms, max_distance).ok()),
            limit,
            offset,
            &order,
            &hl,
        )
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
        let query = self.build_query(regex_terms, facets, slop, max_expansions)?;
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        self.run_search_stream(
            query,
            |_| Ok(None),
            limit,
            offset,
            &order,
            &hl,
            chunk_size,
            sink,
        )
    }

    // ── High-level mode-specific search API ──────────────────────────────────────
    //
    // These are the methods the otzaria app calls through its SearchEngineGateway.
    // Each builds the query for its mode (exact = Term/PhraseQuery, advanced =
    // morphological regex, fuzzy = FuzzyTermQuery) then routes through the shared
    // `run_*` executors. Snippet-returning methods apply the default `<font>`
    // highlight, which the app's snippet parser expects.

    // -- Exact -------------------------------------------------------------------

    pub fn search_exact(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
    ) -> Result<Vec<SearchResult>> {
        let q = self.build_exact_query(&query, &facets)?;
        self.run_search(
            q,
            |_| Ok(None),
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
    }

    pub fn search_and_count_exact(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
    ) -> Result<SearchPageResult> {
        let q = self.build_exact_query(&query, &facets)?;
        self.run_search_and_count(
            q,
            |_| Ok(None),
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
    }

    pub fn search_exact_stream(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        chunk_size: u32,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let q = self.build_exact_query(&query, &facets)?;
        self.run_search_stream(
            q,
            |_| Ok(None),
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
            chunk_size,
            sink,
        )
    }

    pub fn count_exact(&self, query: String, facets: Vec<String>) -> Result<u32> {
        let q = self.build_exact_query(&query, &facets)?;
        self.run_count(q)
    }

    pub fn count_by_book_exact(
        &self,
        query: String,
        facets: Vec<String>,
    ) -> Result<HashMap<String, u32>> {
        let q = self.build_exact_query(&query, &facets)?;
        self.run_count_by_book(q)
    }

    pub fn get_facet_counts_exact(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
    ) -> Result<Vec<FacetCount>> {
        let q = self.build_exact_query(&query, &facets)?;
        self.run_facet_counts(q, facet_prefix)
    }

    // -- Advanced ----------------------------------------------------------------

    pub fn search_advanced(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
    ) -> Result<Vec<SearchResult>> {
        let (q, regex_terms) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
        )?;
        self.run_search(
            q,
            |s| self.advanced_highlight_query(s, &regex_terms),
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
    }

    pub fn search_and_count_advanced(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
    ) -> Result<SearchPageResult> {
        let (q, regex_terms) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
        )?;
        self.run_search_and_count(
            q,
            |s| self.advanced_highlight_query(s, &regex_terms),
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
    }

    pub fn search_advanced_stream(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
        chunk_size: u32,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let (q, regex_terms) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
        )?;
        self.run_search_stream(
            q,
            |s| self.advanced_highlight_query(s, &regex_terms),
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
            chunk_size,
            sink,
        )
    }

    pub fn count_advanced(
        &self,
        query: String,
        facets: Vec<String>,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
    ) -> Result<u32> {
        let (q, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
        )?;
        self.run_count(q)
    }

    pub fn count_by_book_advanced(
        &self,
        query: String,
        facets: Vec<String>,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
    ) -> Result<HashMap<String, u32>> {
        let (q, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
        )?;
        self.run_count_by_book(q)
    }

    pub fn get_facet_counts_advanced(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
    ) -> Result<Vec<FacetCount>> {
        let (q, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
        )?;
        self.run_facet_counts(q, facet_prefix)
    }

    // -- Fuzzy -------------------------------------------------------------------

    pub fn search_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
    ) -> Result<Vec<SearchResult>> {
        let token_texts = self.index_token_texts(&query)?;
        let rank = matches!(order, ResultsOrder::Relevance);
        let q = self.build_fuzzy_search_query(&token_texts, &facets, max_distance, rank)?;
        self.run_search(
            q,
            |s| {
                Ok(self
                    .build_fuzzy_highlight(s, &token_texts, max_distance)
                    .ok())
            },
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
    }

    pub fn search_and_count_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
    ) -> Result<SearchPageResult> {
        let token_texts = self.index_token_texts(&query)?;
        let rank = matches!(order, ResultsOrder::Relevance);
        let q = self.build_fuzzy_search_query(&token_texts, &facets, max_distance, rank)?;
        self.run_search_and_count(
            q,
            |s| {
                Ok(self
                    .build_fuzzy_highlight(s, &token_texts, max_distance)
                    .ok())
            },
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
    }

    pub fn search_fuzzy_stream(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
        chunk_size: u32,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let token_texts = self.index_token_texts(&query)?;
        let rank = matches!(order, ResultsOrder::Relevance);
        let q = self.build_fuzzy_search_query(&token_texts, &facets, max_distance, rank)?;
        self.run_search_stream(
            q,
            |s| {
                Ok(self
                    .build_fuzzy_highlight(s, &token_texts, max_distance)
                    .ok())
            },
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
            chunk_size,
            sink,
        )
    }

    pub fn count_fuzzy(&self, query: String, facets: Vec<String>, max_distance: u8) -> Result<u32> {
        let q = self.build_fuzzy_query(&query, &facets, max_distance)?;
        self.run_count(q)
    }

    pub fn count_by_book_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        max_distance: u8,
    ) -> Result<HashMap<String, u32>> {
        let q = self.build_fuzzy_query(&query, &facets, max_distance)?;
        self.run_count_by_book(q)
    }

    pub fn get_facet_counts_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        max_distance: u8,
    ) -> Result<Vec<FacetCount>> {
        let q = self.build_fuzzy_query(&query, &facets, max_distance)?;
        self.run_facet_counts(q, facet_prefix)
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn all_fields(&self) -> Result<SchemaFields> {
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
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<Box<dyn Query>> {
        let schema = self.index.schema();
        let text_field = schema.get_field("text")?;
        let topics_field = schema.get_field("topics")?;

        let main_query: Box<dyn Query> = match regex_terms.len() {
            0 => Box::new(EmptyQuery),
            1 => self.single_regex_term_query(&regex_terms[0], text_field, max_expansions)?,
            _ => {
                let mut phrase_query = RegexPhraseQuery::new(text_field, regex_terms);
                phrase_query.set_slop(slop);
                phrase_query.set_max_expansions(max_expansions);
                Box::new(phrase_query)
            }
        };

        if facets.is_empty() {
            return Ok(main_query);
        }
        let facet_terms: Vec<Term> = facets
            .iter()
            .map(|f| Ok(Term::from_facet(topics_field, &Facet::from_text(f)?)))
            .collect::<Result<Vec<_>>>()?;
        let facets_query = TermSetQuery::new(facet_terms);

        Ok(Box::new(BooleanQuery::new(vec![
            (Occur::Must, main_query),
            (Occur::Must, Box::new(facets_query) as Box<dyn Query>),
        ])))
    }

    /// Single regex term: materialize the matching index terms into a
    /// `TermSetQuery`, enforcing `max_expansions` the same way
    /// `RegexPhraseQuery` does for multi-term queries (error on overflow).
    /// A bare `RegexQuery` would enumerate the term dictionary without any
    /// bound, so a broad pattern (e.g. a 1-char word with prefix+suffix
    /// options) could scan a huge slice of the index unchecked.
    fn single_regex_term_query(
        &self,
        pattern: &str,
        text_field: Field,
        max_expansions: u32,
    ) -> Result<Box<dyn Query>> {
        let regex = tantivy_fst::Regex::new(pattern)
            .map_err(|e| anyhow::anyhow!("invalid regex {pattern:?}: {e}"))?;
        let searcher = self.index_reader.searcher();
        let mut matched: HashSet<String> = HashSet::new();
        for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(text_field)?;
            let mut stream = inverted.terms().search(&regex).into_stream()?;
            while stream.advance() {
                if let Ok(term) = std::str::from_utf8(stream.key()) {
                    // contains-before-insert avoids re-allocating the term
                    // string when it was already seen in an earlier segment.
                    if !matched.contains(term) {
                        matched.insert(term.to_string());
                        if matched.len() > max_expansions as usize {
                            anyhow::bail!("query exceeded max expansions {max_expansions}");
                        }
                    }
                }
            }
        }
        let terms: Vec<Term> = matched
            .into_iter()
            .map(|t| Term::from_field_text(text_field, &t))
            .collect();
        Ok(Box::new(TermSetQuery::new(terms)))
    }

    /// Tokenizes `text` with the same `"hebrew"` analyzer the `text` field is
    /// indexed with, after stripping nikud — so exact/fuzzy terms line up with
    /// the (normalized) index term dictionary, including geresh/gershayim kept
    /// inside tokens (ז"ל, תוס').
    fn index_token_texts(&self, text: &str) -> Result<Vec<String>> {
        let normalized = hebrew_query::strip_nikud(text);
        let mut analyzer = self
            .index
            .tokenizers()
            .get("hebrew")
            .context("hebrew tokenizer not registered")?;
        let mut stream = analyzer.token_stream(&normalized);
        let mut out = Vec::new();
        while let Some(token) = stream.next() {
            out.push(token.text.clone());
        }
        Ok(out)
    }

    /// Facet filter sub-query (a `TermSetQuery` over the `topics` facet field).
    fn facet_filter_query(&self, facets: &[String]) -> Result<Box<dyn Query>> {
        let topics_f = self.schema.get_field("topics")?;
        let facet_terms: Vec<Term> = facets
            .iter()
            .map(|f| Ok(Term::from_facet(topics_f, &Facet::from_text(f)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(Box::new(TermSetQuery::new(facet_terms)))
    }

    /// Exact mode: a `TermQuery` (one token) or `PhraseQuery` (several), filtered
    /// by facets. No regex — fastest path.
    fn build_exact_query(&self, query_str: &str, facets: &[String]) -> Result<Box<dyn Query>> {
        let text_f = self.schema.get_field("text")?;
        let token_texts = self.index_token_texts(query_str)?;
        let mut terms: Vec<Term> = token_texts
            .iter()
            .map(|t| Term::from_field_text(text_f, t))
            .collect();
        let main_query: Box<dyn Query> = match terms.len() {
            0 => Box::new(EmptyQuery),
            1 => Box::new(TermQuery::new(
                terms.pop().unwrap(),
                IndexRecordOption::Basic,
            )),
            _ => Box::new(PhraseQuery::new(terms)),
        };
        if facets.is_empty() {
            Ok(main_query)
        } else {
            Ok(Box::new(BooleanQuery::new(vec![
                (Occur::Must, main_query),
                (Occur::Must, self.facet_filter_query(facets)?),
            ])))
        }
    }

    /// The two summed `Should` clauses that lift an exact-token hit to the top
    /// relevance tier: a `ConstScoreQuery` floor (immune to BM25 `idf` collapse
    /// on near-ubiquitous terms) plus a small BM25 `TermQuery` add-on for
    /// intra-exact ordering. Only used on the ranked (`Relevance`) fuzzy path.
    fn exact_rank_clauses(text_f: Field, token: &str) -> Vec<(Occur, Box<dyn Query>)> {
        let term = Term::from_field_text(text_f, token);
        vec![
            (
                Occur::Should,
                Box::new(ConstScoreQuery::new(
                    Box::new(TermQuery::new(term.clone(), IndexRecordOption::Basic)),
                    FUZZY_BOOST_EXACT,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs)),
                    FUZZY_BOOST_EXACT_REL,
                )),
            ),
        ]
    }

    /// Fuzzy mode from pre-tokenized terms: one `FuzzyTermQuery` per term, ANDed,
    /// filtered by facets. `rank` adds the exact relevance tier (see
    /// [`Self::exact_rank_clauses`]); count/catalogue paths pass `false`.
    fn build_fuzzy_query_from_terms(
        &self,
        term_texts: &[String],
        facets: &[String],
        max_distance: u8,
        rank: bool,
    ) -> Result<Box<dyn Query>> {
        // Tantivy only rejects distances above 2 when the query executes
        // (InvalidArgument from FuzzyTermQuery's weight); validate upfront so
        // every fuzzy path fails fast with a clear error instead.
        anyhow::ensure!(
            max_distance <= 2,
            "fuzzy distance is limited to 2, got {max_distance}"
        );
        // Mirror exact mode: an empty query matches nothing. Without this
        // guard the clause list degenerates to just the facet filter and the
        // query returns every document in the selected facets.
        if term_texts.is_empty() {
            return Ok(Box::new(EmptyQuery));
        }
        let text_f = self.schema.get_field("text")?;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = term_texts
            .iter()
            .map(|t| {
                let term = Term::from_field_text(text_f, t);
                let fuzzy = FuzzyTermQuery::new(term, max_distance, true);
                // Bare recall is one fuzzy automaton per token. distance 0 is
                // exact already, and unranked paths (count/catalogue) need no
                // scoring, so both stay byte-identical to the bare query. On the
                // ranked path above distance 0 we add the exact tier so an exact
                // hit outranks a bare edit-distance neighbour; the exact term is
                // a subset of the fuzzy match, so recall is unchanged.
                let token_query: Box<dyn Query> = if !rank || max_distance == 0 {
                    Box::new(fuzzy)
                } else {
                    let mut should = Self::exact_rank_clauses(text_f, t);
                    should.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(Box::new(fuzzy), FUZZY_BOOST_FUZZY)),
                    ));
                    Box::new(BooleanQuery::new(should))
                };
                (Occur::Must, token_query)
            })
            .collect();
        if !facets.is_empty() {
            clauses.push((Occur::Must, self.facet_filter_query(facets)?));
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    /// Fuzzy mode from a raw query string (tokenized like the index). Used only
    /// by the count/facet paths, which never rank — hence `rank: false`.
    fn build_fuzzy_query(
        &self,
        query: &str,
        facets: &[String],
        max_distance: u8,
    ) -> Result<Box<dyn Query>> {
        let token_texts = self.index_token_texts(query)?;
        self.build_fuzzy_search_query(&token_texts, facets, max_distance, false)
    }

    /// Approximate (`fuzzy`) recall query. Routes through the lexical builder
    /// when a `MagicDictionary` is loaded, otherwise the plain fuzzy builder.
    /// This is the single decision point so every fuzzy entry point
    /// (`search_*`/`count_*`) shares identical matching logic. `rank` toggles
    /// the relevance-scoring layer: `true` only for `ResultsOrder::Relevance`
    /// searches, `false` for counts and catalogue ordering (which ignore score)
    /// so they build the bare recall query and pay nothing for unused ranking.
    fn build_fuzzy_search_query(
        &self,
        term_texts: &[String],
        facets: &[String],
        max_distance: u8,
        rank: bool,
    ) -> Result<Box<dyn Query>> {
        if self.magic_dict.is_some() && max_distance > 0 {
            self.build_lexical_fuzzy_query(term_texts, facets, max_distance, rank)
        } else {
            self.build_fuzzy_query_from_terms(term_texts, facets, max_distance, rank)
        }
    }

    /// Lexical fuzzy mode: per token, `(FuzzyTermQuery OR TermSetQuery[lexical
    /// forms])` is required (`MUST`); the inner `SHOULD` group keeps both
    /// edit-distance matches and morphological relatives. Falls back to the
    /// bare fuzzy clause for tokens the dictionary doesn't know. Facets filter
    /// as usual. Independent of exact/advanced — only the fuzzy path calls it.
    fn build_lexical_fuzzy_query(
        &self,
        term_texts: &[String],
        facets: &[String],
        max_distance: u8,
        rank: bool,
    ) -> Result<Box<dyn Query>> {
        anyhow::ensure!(
            max_distance <= 2,
            "fuzzy distance is limited to 2, got {max_distance}"
        );
        if term_texts.is_empty() {
            return Ok(Box::new(EmptyQuery));
        }
        let dict = self
            .magic_dict
            .as_ref()
            .context("lexical fuzzy query requires a loaded magic dictionary")?;
        let text_f = self.schema.get_field("text")?;

        if term_texts.len() > 1 {
            let patterns = self.lexical_fuzzy_phrase_patterns(dict, term_texts, max_distance)?;
            let mut phrase_query = RegexPhraseQuery::new(text_f, patterns);
            phrase_query.set_slop(LEXICAL_FUZZY_PHRASE_SLOP);
            phrase_query
                .set_max_expansions((MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN * term_texts.len()) as u32);
            let main_query: Box<dyn Query> = Box::new(phrase_query);
            return if facets.is_empty() {
                Ok(main_query)
            } else {
                Ok(Box::new(BooleanQuery::new(vec![
                    (Occur::Must, main_query),
                    (Occur::Must, self.facet_filter_query(facets)?),
                ])))
            };
        }

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(term_texts.len() + 1);
        for token in term_texts {
            let exact_term = Term::from_field_text(text_f, token);
            // Wrap the fuzzy automaton in the fuzzy-tier boost only when ranking;
            // an unranked recall query (count/catalogue) carries no boost so it
            // stays the bare `FuzzyTermQuery` it always was.
            let fuzzy_q = FuzzyTermQuery::new(exact_term, max_distance, true);
            let fuzzy: Box<dyn Query> = if rank {
                Box::new(BoostQuery::new(Box::new(fuzzy_q), FUZZY_BOOST_FUZZY))
            } else {
                Box::new(fuzzy_q)
            };
            let mut forms = dict.recall_forms(token, MAX_LEXICAL_FORMS);
            forms.retain(|f| f != token);

            // Unranked: the original recall shape — `fuzzy OR termset`, or just
            // `fuzzy` when the dictionary has no extra forms. Ranked: prepend the
            // exact tier (the exact term is a subset of the fuzzy match, so this
            // never changes recall) and boost the lexical tier. `BooleanQuery`
            // sums `Should` scores, so exact-floor + BM25 > lexical > fuzzy.
            let mut should: Vec<(Occur, Box<dyn Query>)> = if rank {
                Self::exact_rank_clauses(text_f, token)
            } else {
                Vec::with_capacity(2)
            };
            should.push((Occur::Should, fuzzy));
            if !forms.is_empty() {
                let set_terms: Vec<Term> = forms
                    .iter()
                    .map(|f| Term::from_field_text(text_f, f))
                    .collect();
                let termset: Box<dyn Query> = if rank {
                    Box::new(BoostQuery::new(
                        Box::new(TermSetQuery::new(set_terms)),
                        FUZZY_BOOST_LEXICAL,
                    ))
                } else {
                    Box::new(TermSetQuery::new(set_terms))
                };
                should.push((Occur::Should, termset));
            }

            // A single bare `FuzzyTermQuery` (no forms, unranked) needs no
            // wrapping `BooleanQuery` — keep it byte-identical to the original.
            let token_query: Box<dyn Query> = if should.len() == 1 {
                should.pop().unwrap().1
            } else {
                Box::new(BooleanQuery::new(should))
            };
            clauses.push((Occur::Must, token_query));
        }
        if !facets.is_empty() {
            clauses.push((Occur::Must, self.facet_filter_query(facets)?));
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    fn lexical_fuzzy_phrase_patterns(
        &self,
        dict: &MagicDictionary,
        term_texts: &[String],
        max_distance: u8,
    ) -> Result<Vec<String>> {
        let builder = LevenshteinAutomatonBuilder::new(max_distance, true);
        // Query-time enumeration (not highlight) — no search-scoped searcher
        // exists yet, so take a fresh one like the other query builders do.
        let searcher = self.index_reader.searcher();
        term_texts
            .iter()
            .map(|token| {
                let mut terms = Vec::new();
                let mut seen = HashSet::new();

                Self::push_limited_unique(
                    &mut terms,
                    &mut seen,
                    token.clone(),
                    MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN,
                );
                for form in dict.recall_forms(token, MAX_LEXICAL_FORMS) {
                    Self::push_limited_unique(
                        &mut terms,
                        &mut seen,
                        form,
                        MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN,
                    );
                }

                let remaining = MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN.saturating_sub(terms.len());
                if remaining > 0 {
                    let automaton = DfaWrapper(builder.build_dfa(token));
                    for fuzzy_term in self.automaton_terms(&searcher, &automaton, remaining)? {
                        Self::push_limited_unique(
                            &mut terms,
                            &mut seen,
                            fuzzy_term,
                            MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN,
                        );
                    }
                }

                Ok(Self::terms_regex_union(&terms))
            })
            .collect()
    }

    fn push_limited_unique(
        out: &mut Vec<String>,
        seen: &mut HashSet<String>,
        value: String,
        cap: usize,
    ) {
        if out.len() < cap && seen.insert(value.clone()) {
            out.push(value);
        }
    }

    fn terms_regex_union(terms: &[String]) -> String {
        if terms.len() == 1 {
            return Self::escape_regex_term(&terms[0]);
        }
        let escaped = terms
            .iter()
            .map(|term| Self::escape_regex_term(term))
            .collect::<Vec<_>>();
        format!("(?:{})", escaped.join("|"))
    }

    fn escape_regex_term(term: &str) -> String {
        let mut out = String::with_capacity(term.len());
        for ch in term.chars() {
            if matches!(
                ch,
                '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
            ) {
                out.push('\\');
            }
            out.push(ch);
        }
        out
    }

    /// Advanced mode: ports the Dart morphological query builder to produce regex
    /// terms + slop + max_expansions, then reuses `build_query`. Also returns the
    /// regex patterns so callers can materialize concrete terms for highlighting.
    fn build_advanced_query(
        &self,
        query: &str,
        distance: u32,
        custom_spacing: &HashMap<String, String>,
        alternative_words: &HashMap<u32, Vec<String>>,
        search_options: &HashMap<String, HashMap<String, bool>>,
        facets: Vec<String>,
    ) -> Result<(Box<dyn Query>, Vec<String>)> {
        let prepared = hebrew_query::prepare_advanced_query(
            query,
            distance,
            custom_spacing,
            alternative_words,
            search_options,
        );
        let regex_terms = prepared.regex_terms.clone();
        let query = self.build_query(
            prepared.regex_terms,
            facets,
            prepared.slop,
            prepared.max_expansions,
        )?;
        Ok((query, regex_terms))
    }

    // ── Shared query executors (take a prebuilt query) ───────────────────────────

    fn run_search<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
    ) -> Result<Vec<SearchResult>>
    where
        F: FnOnce(&Searcher) -> Result<Option<Box<dyn Query>>>,
    {
        let searcher = self.index_reader.searcher();
        let addresses = Self::collect_addresses(&searcher, &*query, limit, offset, order)?;
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        let hl_query = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = hl_query.as_deref().unwrap_or(query.as_ref());
        Self::build_results(&self.schema, &searcher, hl_q, addresses, hl)
    }

    fn run_search_and_count<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
    ) -> Result<SearchPageResult>
    where
        F: FnOnce(&Searcher) -> Result<Option<Box<dyn Query>>>,
    {
        let searcher = self.index_reader.searcher();
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
        // total_count is the full hit count regardless of this page; only the
        // snippet highlighting (and its dictionary scan) is page-dependent, so
        // skip it when this page is empty (e.g. offset past the last hit).
        if addresses.is_empty() {
            return Ok(SearchPageResult {
                total_count,
                results: Vec::new(),
            });
        }
        let hl_query = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = hl_query.as_deref().unwrap_or(query.as_ref());
        let results = Self::build_results(&self.schema, &searcher, hl_q, addresses, hl)?;
        Ok(SearchPageResult {
            total_count,
            results,
        })
    }

    fn run_count(&self, query: Box<dyn Query>) -> Result<u32> {
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&*query, &Count)? as u32)
    }

    fn run_count_by_book(&self, query: Box<dyn Query>) -> Result<HashMap<String, u32>> {
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&*query, &BookCountCollector)?)
    }

    fn run_facet_counts(
        &self,
        query: Box<dyn Query>,
        facet_prefix: String,
    ) -> Result<Vec<FacetCount>> {
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

    fn run_search_stream<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
        chunk_size: u32,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()>
    where
        F: FnOnce(&Searcher) -> Result<Option<Box<dyn Query>>>,
    {
        let searcher = self.index_reader.searcher();
        let chunk_size = (chunk_size.max(1)) as usize;
        let addresses = Self::collect_addresses(&searcher, &*query, limit, offset, order)?;
        if addresses.is_empty() {
            return Ok(());
        }
        let hl_query = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = hl_query.as_deref().unwrap_or(query.as_ref());
        // One generator for the whole stream: creating it resolves term
        // doc-frequencies, which is too expensive to repeat per chunk.
        let snippet_generator = Self::make_snippet_generator(&self.schema, &searcher, hl_q, hl)?;
        for chunk in addresses.chunks(chunk_size) {
            let results = Self::build_results_with_generator(
                &self.schema,
                &searcher,
                &snippet_generator,
                chunk.to_vec(),
                hl,
            )?;
            // If the Dart side cancelled the stream, stop early.
            if sink.add(results).is_err() {
                break;
            }
        }
        Ok(())
    }

    /// Invokes a highlight-query builder against the search's `searcher`,
    /// degrading a build failure to `None` (no highlight) instead of failing the
    /// whole search. `None` makes the caller fall back to the main query, which
    /// already exposes its terms when it is a Term/Phrase/TermSet query.
    fn resolve_highlight<F>(searcher: &Searcher, make_highlight: F) -> Option<Box<dyn Query>>
    where
        F: FnOnce(&Searcher) -> Result<Option<Box<dyn Query>>>,
    {
        make_highlight(searcher).ok().flatten()
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

    /// Materializes the concrete `text` terms that the advanced-mode regex
    /// patterns actually match in the index dictionary, returning them as a
    /// `TermSetQuery` for use as the highlight query.
    ///
    /// Regex/automaton queries (`RegexQuery`, `RegexPhraseQuery`) expose no
    /// static terms to `SnippetGenerator`, so without this their results would
    /// render with no highlighting. By streaming the term dictionary through the
    /// same FST automaton the search itself uses, we highlight every morphological
    /// variant that genuinely matched (prefixes, suffixes, alternatives), not just
    /// the literal words the user typed.
    fn build_regex_highlight_query(
        &self,
        searcher: &Searcher,
        regex_terms: &[String],
    ) -> Result<Box<dyn Query>> {
        let automatons: Vec<tantivy_fst::Regex> = regex_terms
            .iter()
            .map(|pattern| {
                tantivy_fst::Regex::new(pattern)
                    .map_err(|e| anyhow::anyhow!("invalid highlight regex {pattern:?}: {e}"))
            })
            .collect::<Result<Vec<_>>>()?;
        self.build_automaton_highlight_query(searcher, &automatons)
    }

    /// Highlight query for an advanced (regex) search, or `None` when the main
    /// query already highlights itself.
    ///
    /// A single regex term is executed as a `TermSetQuery` (see
    /// [`Self::single_regex_term_query`]) whose materialized terms ARE the terms
    /// that matched — so the main query exposes them to `SnippetGenerator`
    /// directly and no second term-dictionary scan is needed. Only the
    /// multi-term `RegexPhraseQuery`, which exposes no static terms, requires a
    /// separately-materialized highlight query.
    fn advanced_highlight_query(
        &self,
        searcher: &Searcher,
        regex_terms: &[String],
    ) -> Result<Option<Box<dyn Query>>> {
        if regex_terms.len() >= 2 {
            Ok(Some(
                self.build_regex_highlight_query(searcher, regex_terms)?,
            ))
        } else {
            Ok(None)
        }
    }

    /// Fuzzy-mode counterpart of [`Self::build_regex_highlight_query`]:
    /// materializes the `text` terms each query term matches within
    /// `max_distance` edits. `FuzzyTermQuery` is automaton-based like the regex
    /// queries and exposes no static terms to `SnippetGenerator`, so without
    /// this fuzzy results would render with no highlighting.
    fn build_fuzzy_highlight_query(
        &self,
        searcher: &Searcher,
        term_texts: &[String],
        max_distance: u8,
    ) -> Result<Box<dyn Query>> {
        // FuzzyTermQuery itself rejects distances above 2.
        anyhow::ensure!(
            max_distance <= 2,
            "fuzzy highlight distance is limited to 2, got {max_distance}"
        );
        // Same builder configuration as the search's FuzzyTermQuery
        // (transposition counts as one edit), so the highlighted terms are
        // exactly the terms the query can match.
        let builder = LevenshteinAutomatonBuilder::new(max_distance, true);
        let automatons: Vec<DfaWrapper> = term_texts
            .iter()
            .map(|t| DfaWrapper(builder.build_dfa(t)))
            .collect();
        self.build_automaton_highlight_query(searcher, &automatons)
    }

    /// Highlight query for the approximate (`fuzzy`) path, branching on whether
    /// a `MagicDictionary` is loaded — the highlight terms must mirror whatever
    /// [`Self::build_fuzzy_search_query`] matched. Takes the search's own
    /// `searcher` so highlight terms come from the same index snapshot.
    fn build_fuzzy_highlight(
        &self,
        searcher: &Searcher,
        term_texts: &[String],
        max_distance: u8,
    ) -> Result<Box<dyn Query>> {
        if self.magic_dict.is_some() && max_distance > 0 {
            self.build_lexical_fuzzy_highlight_query(searcher, term_texts, max_distance)
        } else {
            self.build_fuzzy_highlight_query(searcher, term_texts, max_distance)
        }
    }

    /// Like [`Self::build_fuzzy_highlight_query`] but also paints the lexical
    /// forms injected by [`Self::build_lexical_fuzzy_query`]. The blacklist is
    /// applied here (highlight only): hallucinated lemmas still expanded recall
    /// but are not highlighted.
    fn build_lexical_fuzzy_highlight_query(
        &self,
        searcher: &Searcher,
        term_texts: &[String],
        max_distance: u8,
    ) -> Result<Box<dyn Query>> {
        anyhow::ensure!(
            max_distance <= 2,
            "fuzzy highlight distance is limited to 2, got {max_distance}"
        );
        let dict = self
            .magic_dict
            .as_ref()
            .context("lexical fuzzy highlight requires a loaded magic dictionary")?;
        let text_f = self.schema.get_field("text")?;

        // Start from the edit-distance terms (same automatons as search)...
        let builder = LevenshteinAutomatonBuilder::new(max_distance, true);
        let automatons: Vec<DfaWrapper> = term_texts
            .iter()
            .map(|t| DfaWrapper(builder.build_dfa(t)))
            .collect();
        let mut matched = self.automaton_highlight_terms(searcher, &automatons)?;

        // ...then add the literal tokens and the (blacklist-filtered) lexical
        // forms per token. The exact token can otherwise be omitted when a broad
        // fuzzy automaton exhausts its highlight-term budget first.
        for token in term_texts {
            matched.insert(token.clone());
            for form in dict.highlight_forms(token, MAX_LEXICAL_FORMS) {
                matched.insert(form);
            }
        }

        let terms: Vec<Term> = matched
            .into_iter()
            .map(|t| Term::from_field_text(text_f, &t))
            .collect();
        Ok(Box::new(TermSetQuery::new(terms)))
    }

    fn build_automaton_highlight_query<A>(
        &self,
        searcher: &Searcher,
        automatons: &[A],
    ) -> Result<Box<dyn Query>>
    where
        A: Automaton,
        A::State: Clone,
    {
        let text_f = self.schema.get_field("text")?;
        let matched = self.automaton_highlight_terms(searcher, automatons)?;
        let terms: Vec<Term> = matched
            .into_iter()
            .map(|t| Term::from_field_text(text_f, &t))
            .collect();
        Ok(Box::new(TermSetQuery::new(terms)))
    }

    /// Collects the distinct `text`-index terms the given automatons match,
    /// bounded by [`MAX_HIGHLIGHT_TERMS`] split evenly across automatons.
    /// Shared by the regex, fuzzy, and lexical-fuzzy highlight builders.
    fn automaton_highlight_terms<A>(
        &self,
        searcher: &Searcher,
        automatons: &[A],
    ) -> Result<HashSet<String>>
    where
        A: Automaton,
        A::State: Clone,
    {
        let mut matched: HashSet<String> = HashSet::new();
        // Split the term budget evenly between automatons: a global cap would
        // let one broad first word exhaust it and leave the remaining query
        // words with no highlighting at all.
        let per_automaton_cap = (MAX_HIGHLIGHT_TERMS / automatons.len().max(1)).max(1);
        for automaton in automatons {
            for term in self.automaton_terms(searcher, automaton, per_automaton_cap)? {
                matched.insert(term);
            }
        }
        Ok(matched)
    }

    fn automaton_terms<A>(
        &self,
        searcher: &Searcher,
        automaton: &A,
        cap: usize,
    ) -> Result<Vec<String>>
    where
        A: Automaton,
        A::State: Clone,
    {
        let text_f = self.schema.get_field("text")?;
        let mut matched = Vec::new();
        let mut seen = HashSet::new();
        'segments: for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(text_f)?;
            let mut stream = inverted.terms().search(automaton).into_stream()?;
            while stream.advance() {
                if let Ok(term) = std::str::from_utf8(stream.key()) {
                    if seen.insert(term.to_string()) {
                        matched.push(term.to_string());
                        if matched.len() >= cap {
                            break 'segments;
                        }
                    }
                }
            }
        }
        Ok(matched)
    }

    /// Creates a `SnippetGenerator` for the `text` field, configured from `hl`.
    /// Creation resolves the doc-frequencies of every query term, so reuse one
    /// generator across chunks instead of recreating it per call.
    fn make_snippet_generator(
        schema: &Schema,
        searcher: &Searcher,
        query: &dyn Query,
        hl: &HighlightConfig,
    ) -> Result<SnippetGenerator> {
        let text_field = schema.get_field("text")?;
        let mut snippet_generator = SnippetGenerator::create(searcher, query, text_field)?;
        snippet_generator.set_max_num_chars(hl.max_chars as usize);
        Ok(snippet_generator)
    }

    fn build_results(
        schema: &Schema,
        searcher: &Searcher,
        query: &dyn Query,
        addresses: Vec<DocAddress>,
        hl: &HighlightConfig,
    ) -> Result<Vec<SearchResult>> {
        let snippet_generator = Self::make_snippet_generator(schema, searcher, query, hl)?;
        Self::build_results_with_generator(schema, searcher, &snippet_generator, addresses, hl)
    }

    fn build_results_with_generator(
        schema: &Schema,
        searcher: &Searcher,
        snippet_generator: &SnippetGenerator,
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

// ── DfaWrapper ─────────────────────────────────────────────────────────────────

/// Adapts a Levenshtein [`DFA`] to the [`tantivy_fst::Automaton`] trait so the
/// term dictionary can be streamed with the same automaton `FuzzyTermQuery`
/// matches with (mirrors tantivy's internal fuzzy-query wrapper, which is
/// private).
struct DfaWrapper(DFA);

impl Automaton for DfaWrapper {
    type State = u32;

    fn start(&self) -> Self::State {
        self.0.initial_state()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        match self.0.distance(*state) {
            Distance::Exact(_) => true,
            Distance::AtLeast(_) => false,
        }
    }

    fn can_match(&self, state: &Self::State) -> bool {
        *state != SINK_STATE
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        self.0.transition(*state, byte)
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
    use serde_json::json;
    use tempfile::TempDir;

    fn make_engine() -> (SearchEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = SearchEngine::new(dir.path().to_str().unwrap());
        (engine, dir)
    }

    fn dir_path_string(dir: &TempDir) -> String {
        dir.path().to_str().unwrap().to_string()
    }

    /// Writes a tiny `lexical.db` into `dir`: lemma "הלכ" with surfaces
    /// "הלכתי"/"הולכ" (folded, as the real DB stores them) and returns its path.
    fn make_lexical_db(dir: &TempDir) -> String {
        let path = dir.path().join("lexical.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE base (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE, base_id INTEGER NOT NULL REFERENCES base(id), notes TEXT);
            CREATE TABLE variant (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
            CREATE TABLE surface_variant (surface_id INTEGER NOT NULL REFERENCES surface(id), variant_id INTEGER NOT NULL REFERENCES variant(id), PRIMARY KEY (surface_id, variant_id));
            INSERT INTO base (id, value) VALUES (1, 'הלכ'), (2, 'ישנ');
            INSERT INTO surface (id, value, base_id) VALUES
                (1, 'הלכתי', 1),
                (2, 'הולכ', 1),
                (3, 'לכו', 1),
                (4, 'הולכימ', 1),
                (5, 'לישונ', 2),
                (6, 'ישנ', 2),
                (7, 'בלשונ', 2);
            "#,
        )
        .unwrap();
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn new_writes_index_metadata_sidecar() {
        let (_engine, dir) = make_engine();
        let metadata_path = index_metadata_path(dir.path());
        assert!(metadata_path.exists());

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(compatibility.compatible);
        assert_eq!(compatibility.status, "compatible");
        assert_eq!(
            compatibility.found_schema_version,
            Some(INDEX_SCHEMA_VERSION)
        );
    }

    #[test]
    fn missing_sidecar_uses_tantivy_schema_fallback() {
        let (_engine, dir) = make_engine();
        fs::remove_file(index_metadata_path(dir.path())).unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(compatibility.compatible);
        assert_eq!(compatibility.status, "legacy_compatible");
        assert_eq!(
            compatibility.found_schema_version,
            Some(INDEX_SCHEMA_VERSION)
        );
    }

    #[test]
    fn old_sidecar_schema_requires_rebuild() {
        let dir = TempDir::new().unwrap();
        let mut metadata = current_index_metadata();
        metadata.schema_version = INDEX_SCHEMA_VERSION - 1;
        fs::write(
            index_metadata_path(dir.path()),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible);
        assert_eq!(compatibility.status, "rebuild_required");
        assert_eq!(
            compatibility.found_schema_version,
            Some(INDEX_SCHEMA_VERSION - 1)
        );
    }

    #[test]
    fn future_sidecar_schema_marks_engine_too_old() {
        let dir = TempDir::new().unwrap();
        let mut metadata = current_index_metadata();
        metadata.schema_version = INDEX_SCHEMA_VERSION + 1;
        fs::write(
            index_metadata_path(dir.path()),
            serde_json::to_string_pretty(&metadata).unwrap(),
        )
        .unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible);
        assert_eq!(compatibility.status, "engine_too_old");
        assert_eq!(
            compatibility.found_schema_version,
            Some(INDEX_SCHEMA_VERSION + 1)
        );
    }

    #[test]
    fn legacy_tantivy_schema_without_indexed_id_requires_rebuild() {
        let dir = TempDir::new().unwrap();
        let tantivy_metadata = json!({
            "schema": [
                {
                    "name": "id",
                    "type": "u64",
                    "options": {
                        "indexed": false,
                        "fast": true,
                        "stored": true
                    }
                }
            ]
        });
        fs::write(dir.path().join("meta.json"), tantivy_metadata.to_string()).unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible);
        assert_eq!(compatibility.status, "rebuild_required");
        assert_eq!(compatibility.found_schema_version, Some(1));
    }

    #[test]
    fn legacy_schema_with_current_id_but_old_file_path_requires_rebuild() {
        let dir = TempDir::new().unwrap();
        {
            // `id` matches the current shape, but `filePath` lacks FAST (and
            // is tokenized) — the engine could not open this index, so the
            // full-schema check must fail it instead of passing on `id` alone.
            let mut b = Schema::builder();
            b.add_text_field("text", TEXT | STORED | FAST);
            b.add_text_field("reference", STORED);
            b.add_text_field(
                "title",
                TextOptions::default()
                    .set_indexing_options(
                        TextFieldIndexing::default()
                            .set_tokenizer("raw")
                            .set_fieldnorms(false),
                    )
                    .set_stored(),
            );
            b.add_u64_field("id", STORED | FAST | INDEXED);
            b.add_u64_field("segment", STORED);
            b.add_bool_field("isPdf", STORED);
            b.add_text_field("filePath", TEXT | STORED);
            b.add_facet_field("topics", FacetOptions::default());
            let old_schema = b.build();
            let mmap = MmapDirectory::open(dir.path()).unwrap();
            Index::open_or_create(mmap, old_schema).unwrap();
        }

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible);
        assert_eq!(compatibility.status, "rebuild_required");
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
    fn test_count_documents_by_file_path_empty_index() {
        let (engine, _dir) = make_engine();
        assert!(engine.count_documents_by_file_path().unwrap().is_empty());
        assert!(engine.get_indexed_file_paths().unwrap().is_empty());
    }

    #[test]
    fn test_count_documents_by_file_path_basic() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine.count_documents_by_file_path().unwrap();
        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
        assert_eq!(counts.len(), 2);

        let mut paths = engine.get_indexed_file_paths().unwrap();
        paths.sort();
        assert_eq!(paths, vec!["/books/a.txt", "/books/b.txt"]);
    }

    #[test]
    fn test_count_documents_by_file_path_respects_deletes() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        engine.delete_document_by_id(1).unwrap();
        engine.delete_document_by_id(3).unwrap();
        engine.commit().unwrap();

        let counts = engine.count_documents_by_file_path().unwrap();
        assert_eq!(counts.get("/books/a.txt").copied(), Some(1));
        assert_eq!(
            counts.get("/books/b.txt"),
            None,
            "a book whose documents were all deleted must not be reported"
        );

        let paths = engine.get_indexed_file_paths().unwrap();
        assert_eq!(paths, vec!["/books/a.txt"]);
    }

    #[test]
    fn test_count_documents_by_file_path_multi_segment() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);

        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine.count_documents_by_file_path().unwrap();
        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn test_count_documents_by_file_path_excludes_uncommitted() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "שלום רב", "/books/b.txt"); // not committed

        let counts = engine.count_documents_by_file_path().unwrap();
        assert_eq!(counts.len(), 1);
        assert_eq!(counts.get("/books/a.txt").copied(), Some(1));
    }

    #[test]
    fn test_count_documents_by_file_path_from_reopened_index() {
        // The motivating scenario: a fresh engine instance opens a directory
        // that already contains an index, and reconstructs which books are
        // indexed from the index itself (no external state).
        let dir = TempDir::new().unwrap();
        {
            let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
            add(&mut engine, 1, "שלום עולם", "/books/a.txt");
            add(&mut engine, 2, "שלום רב", "/books/a.txt");
            add(&mut engine, 3, "שלום חבר", "/books/b.txt");
            engine.commit().unwrap();
        }

        let reopened = SearchEngine::new(dir.path().to_str().unwrap());
        let counts = reopened.count_documents_by_file_path().unwrap();
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
                .count(vec!["שלום".to_string()], &["/root".to_string()], 0, 100)
                .unwrap(),
            2
        );

        engine.delete_document_by_id(1).unwrap();
        engine.commit().unwrap();

        assert_eq!(
            engine
                .count(vec!["שלום".to_string()], &["/root".to_string()], 0, 100)
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
                .count(vec!["טקסט".to_string()], &["/root".to_string()], 0, 100)
                .unwrap(),
            1
        );
        assert_eq!(
            engine
                .count(vec!["ישן".to_string()], &["/root".to_string()], 0, 100)
                .unwrap(),
            0
        );
        assert_eq!(
            engine
                .count(vec!["חדש".to_string()], &["/root".to_string()], 0, 100)
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
                .count(vec!["שלום".to_string()], &["/root".to_string()], 0, 100)
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
            .search_fuzzy_terms(
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
            exact_texts.contains(&"<font color=red>שלום</font>"),
            "distance=0 must return the exact match, highlighted"
        );
        assert!(
            !exact_texts.iter().any(|t| t.contains("שלם")),
            "distance=0 must not return near-match"
        );

        // distance=1: must return both "שלום" and the near-match "שלם"
        let fuzzy = engine
            .search_fuzzy_terms(
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
            fuzzy_texts.contains(&"<font color=red>שלום</font>"),
            "distance=1 must return exact match, highlighted"
        );
        assert!(
            fuzzy_texts.contains(&"<font color=red>שלם</font>"),
            "distance=1 must return near-match one edit away, highlighted"
        );
        assert!(
            !fuzzy_texts.iter().any(|t| t.contains("ביי")),
            "unrelated term must not appear"
        );
    }

    #[test]
    fn test_set_magic_dictionary_path_reports_validity() {
        let (mut engine, dir) = make_engine();
        assert!(!engine.has_magic_dictionary());
        // Missing file → false, no error, no dictionary loaded.
        assert!(!engine
            .set_magic_dictionary_path(dir.path().join("nope.db").to_str().unwrap().to_string()));
        assert!(!engine.has_magic_dictionary());
        // Valid lexical.db → true.
        let db = make_lexical_db(&dir);
        assert!(engine.set_magic_dictionary_path(db));
        assert!(engine.has_magic_dictionary());
    }

    #[test]
    fn test_lexical_fuzzy_finds_inflection_exact_does_not() {
        let (mut engine, dir) = make_engine();
        // Only the inflected form is indexed; the lemma "הלך" is 3 edits away,
        // and no other token is within fuzzy distance 2 of it.
        add(&mut engine, 1, "הלכתי", "/books/a.txt");
        add(&mut engine, 2, "מזרח", "/books/b.txt");
        engine.commit().unwrap();

        // Exact "הלך" must NOT leak into the inflected doc.
        let exact = engine
            .search_exact(
                "הלך".to_string(),
                vec!["/root".to_string()],
                10,
                0,
                ResultsOrder::Relevance,
            )
            .unwrap();
        assert!(
            exact.is_empty(),
            "exact search must not match the inflection"
        );

        // Fuzzy WITHOUT dictionary: "הלך"→"הלכתי" is >2 edits, still no match.
        let fuzzy_plain = engine
            .search_fuzzy(
                "הלך".to_string(),
                vec!["/root".to_string()],
                10,
                0,
                2,
                ResultsOrder::Relevance,
            )
            .unwrap();
        assert!(
            fuzzy_plain.is_empty(),
            "plain fuzzy cannot reach the inflection at distance 2, got: {:?}",
            fuzzy_plain
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>()
        );

        // Fuzzy WITH dictionary: the lexical expansion injects "הלכתי" → match.
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));
        let fuzzy_lex = engine
            .search_fuzzy(
                "הלך".to_string(),
                vec!["/root".to_string()],
                10,
                0,
                2,
                ResultsOrder::Relevance,
            )
            .unwrap();
        assert_eq!(fuzzy_lex.len(), 1, "lexical fuzzy must find the inflection");
        assert!(fuzzy_lex[0].text.contains("הלכתי"));

        // count_fuzzy must agree with search_fuzzy (same matching logic).
        let count = engine
            .count_fuzzy("הלך".to_string(), vec!["/root".to_string()], 2)
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_lexical_fuzzy_distance_zero_stays_exact() {
        let (mut engine, dir) = make_engine();
        add(&mut engine, 1, "הלכתי", "/books/a.txt");
        engine.commit().unwrap();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

        let fuzzy_zero = engine
            .search_fuzzy(
                "הלך".to_string(),
                vec!["/root".to_string()],
                10,
                0,
                0,
                ResultsOrder::Relevance,
            )
            .unwrap();
        assert!(
            fuzzy_zero.is_empty(),
            "max_distance=0 must not inject lexical expansions"
        );

        let count = engine
            .count_fuzzy("הלך".to_string(), vec!["/root".to_string()], 0)
            .unwrap();
        assert_eq!(count, 0);
    }

    fn fuzzy_ids(
        engine: &mut SearchEngine,
        query: &str,
        max_distance: u8,
        order: ResultsOrder,
    ) -> Vec<u64> {
        engine
            .search_fuzzy(
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                max_distance,
                order,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect()
    }

    #[test]
    fn test_lexical_fuzzy_relevance_tiers_exact_morphology_fuzzy() {
        let (mut engine, dir) = make_engine();
        add(&mut engine, 10, "הלך", "/books/a.txt"); // exact query token
        add(&mut engine, 5, "הלכתי", "/books/b.txt"); // dictionary surface form (distance 3)
        add(&mut engine, 1, "הלכה", "/books/c.txt"); // bare edit-distance neighbour (distance 1)
        engine.commit().unwrap();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

        // Boosting must not change recall: all three are still matched.
        let count = engine
            .count_fuzzy("הלך".to_string(), vec!["/root".to_string()], 2)
            .unwrap();
        assert_eq!(count, 3, "ranking boosts must not change the recall set");

        // Relevance now tiers them: exact > morphology > edit-distance.
        let by_relevance = fuzzy_ids(&mut engine, "הלך", 2, ResultsOrder::Relevance);
        assert_eq!(
            by_relevance,
            vec![10, 5, 1],
            "relevance must rank exact, then dictionary form, then fuzzy"
        );

        // Catalogue ignores score and stays ordered by the catalogue id.
        let by_catalogue = fuzzy_ids(&mut engine, "הלך", 2, ResultsOrder::Catalogue);
        assert_eq!(
            by_catalogue,
            vec![1, 5, 10],
            "catalogue order must be unaffected by ranking"
        );
    }

    #[test]
    fn test_lexical_fuzzy_multi_word_relevance_differs_from_catalogue() {
        // The multi-word path is a `RegexPhraseQuery`, which (unlike the flat
        // single-token automaton) already scores by phrase frequency — so
        // relevance ordering is meaningful there without extra boosting.
        let (mut engine, dir) = make_engine();
        add(&mut engine, 1, "הלך מזרח", "/books/a.txt"); // phrase once
        add(&mut engine, 2, "הלך מזרח הלך מזרח", "/books/b.txt"); // phrase twice
        engine.commit().unwrap();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

        let by_catalogue = fuzzy_ids(&mut engine, "הלך מזרח", 2, ResultsOrder::Catalogue);
        assert_eq!(by_catalogue, vec![1, 2], "catalogue follows id order");

        let by_relevance = fuzzy_ids(&mut engine, "הלך מזרח", 2, ResultsOrder::Relevance);
        assert_eq!(
            by_relevance,
            vec![2, 1],
            "relevance must place the higher-frequency phrase first"
        );
    }

    #[test]
    fn test_lexical_fuzzy_exact_floor_survives_common_term() {
        // A near-ubiquitous exact term has BM25 idf ≈ 0, so a purely
        // multiplicative boost would sink it below the flat lexical tier. The
        // constant floor must keep exact hits on top regardless of doc frequency.
        let (mut engine, dir) = make_engine();
        for id in 1..=100u64 {
            add(&mut engine, id, "הלך", "/books/a.txt"); // exact in ~99% of docs
        }
        add(&mut engine, 1000, "הלכתי", "/books/b.txt"); // lone dictionary form
        engine.commit().unwrap();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

        let by_relevance: Vec<u64> = engine
            .search_fuzzy(
                "הלך".to_string(),
                vec!["/root".to_string()],
                200,
                0,
                2,
                ResultsOrder::Relevance,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(by_relevance.len(), 101, "all docs must be recalled");
        assert_eq!(
            by_relevance.last(),
            Some(&1000),
            "the lone lexical form must rank below every exact hit despite idf≈0"
        );
    }

    #[test]
    fn test_plain_fuzzy_relevance_ranks_exact_first() {
        // No dictionary loaded — exercises the plain fuzzy builder.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 9, "כתבה", "/books/a.txt"); // edit-distance neighbour (distance 1)
        add(&mut engine, 2, "כתב", "/books/b.txt"); // exact query token
        engine.commit().unwrap();

        let by_relevance = fuzzy_ids(&mut engine, "כתב", 2, ResultsOrder::Relevance);
        assert_eq!(
            by_relevance,
            vec![2, 9],
            "exact match must outrank a bare fuzzy neighbour"
        );

        // distance 0 stays a pure exact match — recall is byte-identical.
        let zero = fuzzy_ids(&mut engine, "כתב", 0, ResultsOrder::Relevance);
        assert_eq!(zero, vec![2], "distance 0 must match only the exact token");
    }

    #[test]
    fn test_lexical_fuzzy_multi_word_requires_phrase() {
        let (mut engine, dir) = make_engine();
        add(&mut engine, 1, "הלכתי לישון", "/books/a.txt");
        add(
            &mut engine,
            2,
            "הלכתי ואז דיברתי הרבה לפני לישון",
            "/books/b.txt",
        );
        add(&mut engine, 3, "לישון הלכתי", "/books/c.txt");
        add(&mut engine, 4, "הלכתי", "/books/d.txt");
        add(&mut engine, 5, "לכו ונכהו בלשון", "/books/e.txt");
        engine.commit().unwrap();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

        let got = ids(engine
            .search_fuzzy(
                "הלכתי לישון".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                2,
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(
            got,
            vec![1, 5],
            "multi-token lexical fuzzy search should preserve order while allowing one intervening token"
        );

        let count = engine
            .count_fuzzy("הלכתי לישון".to_string(), vec!["/root".to_string()], 2)
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn test_lexical_fuzzy_highlights_literal_second_token() {
        let (mut engine, dir) = make_engine();
        add(&mut engine, 1, "הולכים לישון", "/books/a.txt");
        for (idx, term) in one_edit_insertions("לישון").into_iter().enumerate() {
            add(
                &mut engine,
                idx as u64 + 2,
                &format!("רעש {term}"),
                "/books/noise.txt",
            );
        }
        engine.commit().unwrap();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

        let results = engine
            .search_fuzzy(
                "הלכתי לישון".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                2,
                ResultsOrder::Catalogue,
            )
            .unwrap();
        let hit = results.iter().find(|result| result.id == 1).unwrap();
        assert!(
            hit.text.contains("<font color=red>לישון</font>"),
            "the literal second query token must remain highlighted, got: {}",
            hit.text
        );
    }

    #[test]
    fn test_lexical_fuzzy_multi_word_allows_expansions_per_token() {
        let (mut engine, dir) = make_engine();
        add(&mut engine, 1, "הלכתי לישון", "/books/a.txt");

        let mut next_id = 2u64;
        for term in one_edit_insertions("הלכתי")
            .into_iter()
            .chain(one_edit_insertions("לישון"))
        {
            add(&mut engine, next_id, &term, "/books/noise.txt");
            next_id += 1;
        }
        engine.commit().unwrap();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));

        let got = ids(engine
            .search_fuzzy(
                "הלכתי לישון".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                1,
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(got, vec![1]);
    }

    fn one_edit_insertions(token: &str) -> Vec<String> {
        const LETTERS: &[char] = &[
            'א', 'ב', 'ג', 'ד', 'ה', 'ו', 'ז', 'ח', 'ט', 'י', 'כ', 'ל', 'מ', 'נ', 'ס', 'ע', 'פ',
            'צ', 'ק', 'ר', 'ש', 'ת',
        ];

        let chars: Vec<char> = token.chars().collect();
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for position in 0..=chars.len() {
            for letter in LETTERS {
                let mut variant = chars.clone();
                variant.insert(position, *letter);
                let variant: String = variant.into_iter().collect();
                if variant != token && seen.insert(variant.clone()) {
                    out.push(variant);
                }
            }
        }
        out
    }

    #[test]
    fn test_new_tolerates_held_writer_lock() {
        let (mut first, dir) = make_engine();
        add(&mut first, 1, "ספר", "/books/a.txt");
        first.commit().unwrap();

        // While `first` holds the writer lock, a second engine must still open
        // (no panic) and serve reads.
        let mut second = SearchEngine::new(dir.path().to_str().unwrap());
        assert_eq!(search_ids(&mut second, "ספר"), vec![1]);

        // Once the lock is released, writes recover lazily via ensure_writer.
        drop(first);
        add(&mut second, 2, "תורה", "/books/b.txt");
        second.commit().unwrap();
        assert_eq!(search_ids(&mut second, "תורה"), vec![2]);
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
    fn test_optimize_commits_pending_documents() {
        let (mut engine, _dir) = make_engine();
        disable_auto_merge(&engine);
        // Two committed segments so optimize doesn't take the early-skip path.
        add(&mut engine, 1, "ספר", "/books/a.txt");
        engine.commit().unwrap();
        add(&mut engine, 2, "ספר", "/books/b.txt");
        engine.commit().unwrap();

        // A pending document must survive optimize, not vanish with the
        // discarded writer buffer.
        add(&mut engine, 3, "ספר", "/books/c.txt");
        engine.optimize().unwrap();

        assert_eq!(search_ids(&mut engine, "ספר"), vec![1, 2, 3]);
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

    // ── High-level mode-specific API ─────────────────────────────────────────────

    fn ids(results: Vec<SearchResult>) -> Vec<u64> {
        let mut v: Vec<u64> = results.into_iter().map(|r| r.id).collect();
        v.sort();
        v
    }

    #[test]
    fn test_search_exact_single_and_phrase() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/b.txt");
        engine.commit().unwrap();

        // Single token matches both docs containing the word.
        let got = ids(engine
            .search_exact(
                "שלום".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(got, vec![1, 2]);

        // Phrase matches only the doc with those adjacent words.
        let got = ids(engine
            .search_exact(
                "שלום עולם".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn test_search_exact_strips_query_nikud() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt"); // indexed without nikud
        engine.commit().unwrap();

        // Query carries nikud; exact mode strips it before tokenizing.
        let got = ids(engine
            .search_exact(
                "שָׁלוֹם".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn test_search_advanced_grammatical_prefix() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        add(&mut engine, 2, "הספר", "/books/b.txt");
        add(&mut engine, 3, "מטבע", "/books/c.txt");
        engine.commit().unwrap();

        let mut word_opts = HashMap::new();
        word_opts.insert("קידומות דקדוקיות".to_string(), true);
        let mut options = HashMap::new();
        options.insert("ספר_0".to_string(), word_opts);

        let got = ids(engine
            .search_advanced(
                "ספר".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(
            got,
            vec![1, 2],
            "grammatical prefix should match ספר and הספר"
        );
    }

    #[test]
    fn test_search_advanced_heavy_option_combo_compiles_and_runs() {
        // typo + grammatical prefix + grammatical suffix produces the largest
        // morphological regex. The length budget must keep it under tantivy-fst's
        // DFA state limit so the search compiles and returns instead of erroring.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        add(&mut engine, 2, "הספרים", "/books/b.txt");
        engine.commit().unwrap();

        let mut word_opts = HashMap::new();
        word_opts.insert(hebrew_query::OPT_TYPO.to_string(), true);
        word_opts.insert("קידומות דקדוקיות".to_string(), true);
        word_opts.insert("סיומות דקדוקיות".to_string(), true);
        let mut options = HashMap::new();
        options.insert("ספר_0".to_string(), word_opts);

        let result = engine.search_advanced(
            "ספר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
        );
        let got = ids(result.expect("heavy option combo must compile and run"));
        assert!(
            got.contains(&1),
            "the base word ספר should still match, got {got:?}"
        );
    }

    #[test]
    fn test_single_term_respects_max_expansions() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        add(&mut engine, 2, "הספר", "/books/b.txt");
        engine.commit().unwrap();

        // Two index terms match; a cap of 1 must error like RegexPhraseQuery.
        let too_narrow = engine.search(
            vec![".*ספר".to_string()],
            vec![],
            100,
            0,
            0,
            1,
            ResultsOrder::Catalogue,
            None,
        );
        assert!(too_narrow.is_err(), "exceeding max_expansions should error");

        let ok = ids(engine
            .search(
                vec![".*ספר".to_string()],
                vec![],
                100,
                0,
                0,
                10,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap());
        assert_eq!(ok, vec![1, 2]);
    }

    #[test]
    fn test_search_advanced_strips_query_nikud() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר תורה", "/books/a.txt");
        engine.commit().unwrap();

        // Pasted vocalized text must still match the nikud-free index terms.
        let got = ids(engine
            .search_advanced(
                "סֵפֶר".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(got, vec![1], "vocalized advanced query should match");
    }

    #[test]
    fn test_search_advanced_empty_query_returns_no_results() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        engine.commit().unwrap();

        // Empty and punctuation-only queries produce zero regex terms; they must
        // return no results instead of panicking inside RegexPhraseQuery.
        for query in ["", "?!"] {
            let results = engine
                .search_advanced(
                    query.to_string(),
                    vec!["/root".to_string()],
                    100,
                    0,
                    0,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    ResultsOrder::Catalogue,
                )
                .unwrap();
            assert!(results.is_empty(), "query {query:?} should match nothing");
        }
    }

    #[test]
    fn test_search_skips_empty_facets() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        engine.commit().unwrap();

        let got = ids(engine
            .search(
                vec!["ספר".to_string()],
                vec![],
                100,
                0,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap());
        assert_eq!(got, vec![1], "empty facet list should not filter anything");
    }

    #[test]
    fn test_search_rejects_invalid_facet() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        engine.commit().unwrap();

        let result = engine.search(
            vec!["ספר".to_string()],
            vec!["not-a-facet".to_string()],
            100,
            0,
            0,
            100,
            ResultsOrder::Catalogue,
            None,
        );
        assert!(result.is_err(), "malformed facet should error, not panic");
    }

    #[test]
    fn test_search_advanced_highlights_morphological_variant() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        add(&mut engine, 2, "הספר", "/books/b.txt");
        engine.commit().unwrap();

        let mut word_opts = HashMap::new();
        word_opts.insert("קידומות דקדוקיות".to_string(), true);
        let mut options = HashMap::new();
        options.insert("ספר_0".to_string(), word_opts);

        let results = engine
            .search_advanced(
                "ספר".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
            )
            .unwrap();

        // The query matched the prefixed variant "הספר" via regex; highlighting
        // must wrap the variant that actually matched, not just the literal "ספר".
        let variant = results
            .iter()
            .find(|r| r.id == 2)
            .expect("הספר document should be in results");
        assert_eq!(
            variant.text, "<font color=red>הספר</font>",
            "morphological variant should be highlighted"
        );
    }

    #[test]
    fn test_search_advanced_alternative_words() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "מלך", "/books/a.txt");
        add(&mut engine, 2, "שר", "/books/b.txt");
        add(&mut engine, 3, "עיר", "/books/c.txt");
        engine.commit().unwrap();

        let mut alts = HashMap::new();
        alts.insert(0u32, vec!["מלך".to_string()]);
        let got = ids(engine
            .search_advanced(
                "שר".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                alts,
                HashMap::new(),
                ResultsOrder::Catalogue,
            )
            .unwrap());
        assert_eq!(got, vec![1, 2], "alternatives should OR שר with מלך");
    }

    #[test]
    fn advanced_search_matches_gershayim_tokens_end_to_end() {
        // HebrewTokenizer מפצל על גרשיים (ז"ל → ז, ל) ושומר רק גרש סופי
        // (תוס'); split_query_words חייב לפצל את השאילתה באותה צורה — אחרת
        // ז"ל לעולם לא יימצא. כולל נורמליזציה של ״ עברי בשאילתה.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "הרב פלוני ז\"ל אמר", "/books/a.txt");
        add(&mut engine, 2, "כתב הרמב\u{05F4}ם בהלכות", "/books/b.txt");
        add(&mut engine, 3, "דברי תוס' שם", "/books/c.txt");
        engine.commit().unwrap();

        let advanced_ids = |engine: &mut SearchEngine, query: &str| {
            ids(engine
                .search_advanced(
                    query.to_string(),
                    vec!["/root".to_string()],
                    100,
                    0,
                    0,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    ResultsOrder::Catalogue,
                )
                .unwrap())
        };

        assert_eq!(advanced_ids(&mut engine, "ז\"ל"), vec![1]);
        assert_eq!(
            advanced_ids(&mut engine, "ז\u{05F4}ל"),
            vec![1],
            "גרשיים עבריים בשאילתה"
        );
        assert_eq!(
            advanced_ids(&mut engine, "הרמב\"ם"),
            vec![2],
            "״ עברי באינדקס, \" בשאילתה"
        );
        assert_eq!(advanced_ids(&mut engine, "תוס'"), vec![3], "גרש סופי");
    }

    #[test]
    fn test_search_fuzzy_high_level() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        add(&mut engine, 2, "שלם", "/books/b.txt");
        add(&mut engine, 3, "ביי", "/books/c.txt");
        engine.commit().unwrap();

        let texts: Vec<String> = engine
            .search_fuzzy(
                "שלום".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                1,
                ResultsOrder::Relevance,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect();
        // Fuzzy matching is automaton-based, so highlighting must come from the
        // materialized highlight query: both the exact match and the variant one
        // edit away are wrapped, not just the literal word the user typed.
        assert!(texts.contains(&"<font color=red>שלום</font>".to_string()));
        assert!(texts.contains(&"<font color=red>שלם</font>".to_string()));
        assert!(!texts.iter().any(|t| t.contains("ביי")));
    }

    #[test]
    fn test_search_fuzzy_invalid_distance_errors() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        // Tantivy supports edit distances 0–2; anything above must surface as
        // an error from every fuzzy entry point, never a panic.
        let result = engine.search_fuzzy(
            "שלום".to_string(),
            vec!["/root".to_string()],
            10,
            0,
            3,
            ResultsOrder::Relevance,
        );
        assert!(result.is_err(), "distance > 2 should error, not panic");

        let count = engine.count_fuzzy("שלום".to_string(), vec!["/root".to_string()], 3);
        assert!(count.is_err(), "distance > 2 should error, not panic");
    }

    #[test]
    fn test_search_fuzzy_highlights_near_match_in_context() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "אמר שלם לחברו", "/books/a.txt");
        engine.commit().unwrap();

        let results = engine
            .search_fuzzy(
                "שלום".to_string(),
                vec!["/root".to_string()],
                10,
                0,
                1,
                ResultsOrder::Relevance,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].text, "אמר <font color=red>שלם</font> לחברו",
            "the fuzzy-matched variant must be highlighted inside the snippet"
        );
    }

    #[test]
    fn test_search_fuzzy_empty_query_returns_no_results() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום", "/books/a.txt");
        engine.commit().unwrap();

        // Mirror exact mode: empty/punctuation-only fuzzy queries match
        // nothing instead of returning every document in the facets.
        for query in ["", "?!"] {
            let results = engine
                .search_fuzzy(
                    query.to_string(),
                    vec!["/root".to_string()],
                    100,
                    0,
                    1,
                    ResultsOrder::Relevance,
                )
                .unwrap();
            assert!(results.is_empty(), "query {query:?} should match nothing");
        }
    }

    #[test]
    fn test_high_level_counts() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "ביי", "/books/b.txt");
        engine.commit().unwrap();

        assert_eq!(
            engine
                .count_exact("שלום".to_string(), vec!["/root".to_string()])
                .unwrap(),
            2
        );

        let by_book = engine
            .count_by_book_exact("שלום".to_string(), vec!["/root".to_string()])
            .unwrap();
        assert_eq!(by_book.get("/books/a.txt").copied(), Some(2));

        let page = engine
            .search_and_count_exact(
                "שלום".to_string(),
                vec!["/root".to_string()],
                1,
                0,
                ResultsOrder::Relevance,
            )
            .unwrap();
        assert_eq!(page.total_count, 2);
        assert_eq!(page.results.len(), 1);
    }
}
