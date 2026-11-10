#[cfg(feature = "flutter-bridge")]
use crate::frb_generated::StreamSink;

#[cfg(test)]
#[path = "../note_snippet_tests.rs"]
mod note_snippet_tests;
use anyhow::{Context, Result};
use flutter_rust_bridge_macros::frb;
use log::{debug, error, info, warn};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tantivy::collector::{Collector, Count, FacetCollector, SegmentCollector, TopDocs};
use tantivy::directory::{Directory, Lock};
use tantivy::index::{SegmentId, SegmentMeta};
use tantivy::indexer::{MergeCandidate, MergePolicy, NoMergePolicy};
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, ConstScoreQuery, DfaWrapper, EmptyQuery, FuzzyTermQuery,
    Occur, TermQuery, TermSetQuery,
};
use tantivy::query::{Query, RegexPhraseQuery};
use tantivy::schema::Value;
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{LowerCaser, RemoveLongFilter, TextAnalyzer, TokenStream};
use tantivy::{doc, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher};
use tantivy::{schema::*, Index};
use tantivy::{DocId, SegmentOrdinal, SegmentReader};
use tantivy_fst::Automaton;

use crate::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookResult,
};
use crate::book_order::{
    self, numbers_in_segments, order_file_stamp, BookNumbers, BookOrderColumn, BookOrderSort,
    BookPlace, OrderKind, OrderMapping, OrderTables,
};
use crate::cross_line;
use crate::derived_columns::{section_offset, SectionIdColumn, SECTION_OFFSET_FIELD};
use crate::display_highlight;
use crate::doc_store::{load_document, segment_to_store, DocumentLoader};
use crate::gap_phrase::{GapVerifiedPhraseQuery, Proximity, TermListPhraseQuery, WordWindows};
use crate::hebrew_query;
use crate::hebrew_query::VocalizedFlags;
use crate::hebrew_tokenizer::HebrewTokenizer;
use crate::highlight_cache::{DisplayPattern, HighlightCache};
use crate::highlight_matcher as display_highlight_matcher;
use crate::id_lookup::IdSetQuery;
use crate::index_directory::{atomic_replace, IndexDirectory};
use crate::lexical_cancellation::LexicalCancellations;
#[cfg(any(feature = "flutter-bridge", test))]
use crate::lexical_cancellation::LexicalRequest;
#[cfg(any(feature = "flutter-bridge", test))]
use crate::lexical_cancellation::LexicalSearchCancelled;
use crate::lexicons::{
    AcronymLexicon, TranslationLexicon, MAX_ACRONYM_EXPANSIONS, MAX_TRANSLATION_EXPANSIONS,
};
use crate::magic::{MagicDictionary, MAX_LEXICAL_FORMS};
use crate::note_spans::{self, DisplayText, NoteSpans};
use crate::page_cache::{self, PageCache, PageKey};
use crate::quote_expansion::{self, AutomatonQuery, QuoteTransparent};
use crate::search_cancellation::{self, SearchCancellation, SearchCheckpoint};
use crate::section_scope::{SectionFilteredQuery, SectionIdsCollector};
use crate::semantic_keys::{self, ChunkKeyRecipe};
use crate::smart_query;
use crate::term_automaton::{self, TermDfa};
use otzaria_semantic_search::semantic::chunk_key::LineRef;

#[cfg(feature = "semantic-integration")]
use crate::semantic_errors::{self, SidecarCall};
#[cfg(feature = "semantic-integration")]
use crate::semantic_session::{Additions, GroupKey, LineKey, SessionKey};
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::config::profiles::{
    FusionStrategy, QueryTypeAlphas, RankingProfile, SearchProfile,
};
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::errors::SemanticSearchError;
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::hybrid::coordinator::{HybridCoordinator, HybridSearchParams};
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::hybrid::hebrew_normalizer::HebrewNormalizer;
use otzaria_semantic_search::hybrid::ranking::quoted_phrases;
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::semantic::embedding::EmbeddingDeployment;
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::semantic::engine::{SemanticConfig, SemanticEngine};
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::semantic::official_index::{
    LocalModel, OfficialIndexConfig, OfficialSemanticIndex,
};
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::semantic::types::{
    is_foundational, HybridResultItem, FOUNDATIONAL_FACET,
};
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::semantic::types::{
    BookForIndexing as SidecarBookForIndexing, BookLine as SidecarBookLine,
    ContentFingerprint as SidecarContentFingerprint, GroupingMode as SidecarGroupingMode,
    LexicalCandidate as SidecarLexicalCandidate, ResultSource as SidecarResultSource,
    SearchFilters as SidecarSearchFilters, SearchMode as SidecarSearchMode,
};
#[cfg(feature = "semantic-integration")]
use otzaria_semantic_search::semantic::versioning::{ModelIdentity, ModelPackage, TextIdentity};
use std::sync::{PoisonError, RwLock};

#[cfg(test)]
#[path = "search_concurrency_tests.rs"]
mod search_concurrency_tests;

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
    /// כמה תוצאות גולמיות מאוחדות בכרטיס הזה (1 = ללא איחוד). ראו
    /// [`ResultGrouping`]: בחיפוש מקובץ התוצאה היא נציג הקבוצה, והמונה
    /// הוא "נמצאו X תוצאות בטווח". כש-`truncated` דלוק המונה הוא תחתית —
    /// קבוצה שפונתה מהתקרה וחזרה מאבדת את חבריה המוקדמים.
    pub merged_count: u32,
    /// שאר חברות הקבוצה (ללא הנציג), עד [`MERGED_SIBLINGS_CAP`] — מיקומים
    /// בלבד, בלי snippet, כדי שהאפליקציה תוכל להציג "הרחב" ולקפוץ אליהן.
    /// `merged_count` עשוי לעלות על `merged.len() + 1` כשהקבוצה גדולה מהתקרה.
    pub merged: Vec<MergedSibling>,
    /// Whether `text` is the line this document was indexed from. Always
    /// [`TextStatus::Ok`] for text stored in the index; see [`TextStorage`].
    pub text_status: TextStatus,
    /// The phrase continues from the end of this line onto the start of the next
    /// line (the document with `id + 1`): `text` is the end of this line and the start
    /// of the next, joined at a `<br>` line break, with the phrase painted across it.
    /// Such a hit is counted once, for this line.
    pub continues_to_next_line: bool,
}

/// חברת קבוצה מאוחדת: מיקום בלבד (בלי טקסט/הדגשה) — מספיק כדי להציג
/// שורת-משנה בכרטיס מקובץ ולפתוח את הספר במקום הנכון.
#[derive(Clone)]
pub struct MergedSibling {
    pub title: String,
    pub reference: String,
    pub id: u64,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
}

/// מצב איחוד תוצאות. `None` בפרמטר `grouping` = ההתנהגות הקיימת (שטוח).
///
/// - `SameSection` — כל התוצאות שתחת אותה כותרת באותו ספר (אותו `sectionId`;
///   ב-PDF: אותו עמוד) מתאחדות לכרטיס אחד עם מונה — "נמצאו X תוצאות בטווח".
/// - `IdenticalText` — שורות שגוף הטקסט העברי שלהן זהה (חתימת `lineHash`)
///   מתאחדות גם חוצה-ספרים — אותה משנה במהדורות שונות, מדרש מצוטט וכו'.
///   שורה קצרה מדי לחתימה (ראו [`line_dedup_hash`]) לעולם אינה מתאחדת.
///
/// הנציג של קבוצה הוא התוצאה הטובה ביותר לפי סדר המיון הנוכחי, והקבוצות
/// ממוינות לפי הנציג; `limit`/`offset` נספרים בקבוצות (לא בתוצאות גולמיות).
#[derive(Clone, Copy)]
pub enum ResultGrouping {
    SameSection,
    IdenticalText,
}

/// Where a document's display text is kept.
///
/// - `InIndex` — in the index's doc store (PDF, file-backed and personal books,
///   attached databases, empty-book markers). The default everywhere. (Not
///   `Index`: Dart enums already have an `index` member.)
/// - `LibraryDb` — only in the inverted index; the display text is read from the
///   library database configured with [`configure_line_source`] when a result is
///   built. Only for an official book whose text came from that database's rows,
///   through [`SearchEngine::add_text_book`]: `file_path` must be `id:<bookId>`, and
///   a document's `segment` is the 0-based position of its row among the book's
///   rows ordered by `lineIndex`. Each line keeps a check of its exact text
///   (`lineCheck`), which the row read back must match. The lines the call lists in
///   `stored_lines` are stored as for `InIndex` instead. A personal book
///   (`uid:<bookId>`) is read the same way from the user-books database configured
///   with [`configure_user_books_line_source`], and a book of an attached database
///   (`db:<slug>:<bookId>`) from the one configured with
///   [`configure_attached_line_source`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStorage {
    LibraryDb,
    InIndex,
}

/// Whether a result's text is the line its document was indexed from.
///
/// - `Ok` — it is (always, for text stored in the index).
/// - `Stale` — the library database no longer matches the index (the row changed
///   in any way, moved or disappeared). `text` is the database's current line,
///   HTML-escaped and unhighlighted (empty when the row is gone). Reindexing the
///   book fixes it.
/// - `Unavailable` — the line source is unconfigured, suspended, busy (another
///   connection is writing the database) or unreadable. `text` is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextStatus {
    Ok,
    Stale,
    Unavailable,
}

/// State of the process-wide line source ([`configure_line_source`]).
pub struct LineSourceStatus {
    /// A database path has been configured.
    pub configured: bool,
    /// A connection is currently open (opened lazily by the first result window).
    pub open: bool,
    /// Outstanding [`suspend_line_source`] calls.
    pub suspend_depth: u32,
    /// Whether this build can call SQLite: always true when SQLite is bundled; in
    /// the app build, true once Dart has run [`sqlite_host_entry_address`] through
    /// `sqlite3_auto_extension` and opened a connection.
    pub host_api_ready: bool,
    /// Bumped whenever cached knowledge of the database is discarded (a new path,
    /// or the last [`resume_line_source`]).
    pub generation: u64,
    /// Books asked for as [`TextStorage::LibraryDb`] that were stored
    /// [`TextStorage::InIndex`] instead, because the library database could not be
    /// read or its row count differed from the book's lines. Only grows within a
    /// process.
    pub library_fallbacks: u64,
    /// A user-books database path has been configured
    /// ([`configure_user_books_line_source`]).
    pub user_books_configured: bool,
    /// A connection to the user-books database is currently open.
    pub user_books_open: bool,
    /// `generation` of the user-books source.
    pub user_books_generation: u64,
    /// `library_fallbacks` of personal (`uid:`) books.
    pub user_books_fallbacks: u64,
}

pub struct DocumentInput {
    /// The 0-based position of the line in its book. The engine composes the document id
    /// from it and the book's number: `(bookNumber << 32) + ordinal + 1`.
    pub ordinal: u32,
    pub title: String,
    pub reference: String,
    pub topics: String,
    pub text: String,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
    /// Book-level **canonical** fingerprint for the `contentHash` column —
    /// [`compute_book_fingerprint`]: the raw text *plus* the metadata baked
    /// into the index (title, topics, extra facets). Recorded once for the book,
    /// so [`SearchEngine::get_book_fingerprints`] can compare an index
    /// against the current library source, including metadata-only changes.
    /// Never compare it against [`compute_content_fingerprint`] (text only) —
    /// that is `text_hash` below. `None`/`0` means "no fingerprint recorded"
    /// (e.g. PDF books).
    pub content_hash: Option<u64>,
    /// Text-only fingerprint ([`compute_content_fingerprint`] over the raw
    /// book text) for the `textHash` column — unlike `content_hash` it does
    /// not shift when metadata changes, so
    /// [`SearchEngine::get_book_text_fingerprints`] detects content drift
    /// only. `None`/`0` means "no fingerprint recorded".
    pub text_hash: Option<u64>,
    /// An opaque hash of the book's source (the library database's `book.contentHash`),
    /// stored as given for [`SearchEngine::get_book_stamps`]. `None`/`0`: none.
    pub source_hash: Option<u64>,
    /// הטקסט המנוקד של השורה (נרמול [`normalize_vocalized_text_for_indexing`])
    /// עבור השדה `textVocalized`. `None` לשורה ללא ניקוד/טעמים — השורה
    /// פשוט לא תשתתף בחיפוש מנוקד. מסלול [`SearchEngine::add_text_book`]
    /// מחשב זאת בעצמו; השדה קיים לצינורות שמוסיפים מסמכים מוכנים.
    pub text_vocalized: Option<String>,
    /// The section (heading block) of the line, as the 0-based ordinal of the line that
    /// opens it. `None`: the line is a section of its own, so "under the same heading"
    /// behaves as "in the same line" for documents given no section.
    pub section_ordinal: Option<u32>,
    /// נתיבי facet נוספים לצד `topics` — ממדי סינון (מחבר/תקופה/ספר-יסוד),
    /// למשל `"/author/רש\"י"`, `"/era/ראשונים"`, `"/base"`. ראו
    /// [`FACET_DIMENSION_ROOTS`] לסמנטיקת הסינון (OR בתוך ממד, AND בין
    /// ממדים). `None` = אין.
    pub extra_facets: Option<Vec<String>>,
    /// Where the display text is kept: `None` or [`TextStorage::InIndex`].
    /// `LibraryDb` is refused here — only [`SearchEngine::add_text_book`] can tie a
    /// line to its library row.
    pub text_storage: Option<TextStorage>,
}

/// One book's place for [`SearchEngine::set_book_order`]: by `catalogue_order`, and by
/// `generation_order` then `catalogue_order` (low first).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookOrderEntry {
    pub file_path: String,
    pub catalogue_order: u32,
    pub generation_order: u32,
}

/// What the index records about one book: [`SearchEngine::get_book_stamps`]. A stamp is 0
/// when the book was written without it, or when its documents disagree on it (a partial
/// rewrite): either way it proves nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookStamps {
    pub file_path: String,
    /// The high half of the book's document ids, assigned by the engine.
    pub book_number: u32,
    /// Live documents of the book.
    pub documents: u32,
    /// [`compute_book_fingerprint`]: text and metadata.
    pub content_hash: u64,
    /// [`compute_content_fingerprint`] of the book's raw text.
    pub text_hash: u64,
    /// [`compute_book_metadata_fingerprint`].
    pub metadata_hash: u64,
    /// The caller's `source_hash`, stored as given.
    pub source_hash: u64,
}

/// One extracted PDF page for [`SearchEngine::add_pdf_book`]: the page's
/// display reference (built by the app from the PDF outline), the raw
/// extracted page text, and the zero-based page index (stored as the
/// document `segment`).
pub struct PdfPageInput {
    pub reference: String,
    pub text: String,
    pub page_index: u32,
}

/// Internal page representation: the byte API borrows valid UTF-8 instead of
/// allocating and copying the entire book into per-page Strings. Only malformed
/// slices need owned replacement text. The existing API moves its Strings here.
struct PdfPageText<'a> {
    reference: Cow<'a, str>,
    text: Cow<'a, str>,
    page_index: u32,
}

/// Validate the complete buffer before decoding any page or touching the index.
fn pdf_pages_from_bytes<'a>(
    bytes: &'a [u8],
    ends: &[u32],
    page_indexes: &[u32],
) -> Result<Vec<PdfPageText<'a>>> {
    let (pairs, remainder) = ends.as_chunks::<2>();
    anyhow::ensure!(
        remainder.is_empty() && pairs.len() == page_indexes.len(),
        "add_pdf_book_bytes: {} ends for {} pages",
        ends.len(),
        page_indexes.len()
    );
    let mut start = 0usize;
    for &end in ends {
        let end = end as usize;
        anyhow::ensure!(
            (start..=bytes.len()).contains(&end),
            "add_pdf_book_bytes: ends out of order or past the buffer"
        );
        start = end;
    }
    anyhow::ensure!(
        start == bytes.len(),
        "add_pdf_book_bytes: ends do not cover the complete buffer"
    );
    start = 0;
    let mut take = |end: u32| {
        let end = end as usize;
        // All ranges were checked above; lossy decoding allocates only on error.
        let text = String::from_utf8_lossy(&bytes[start..end]);
        start = end;
        text
    };
    Ok(page_indexes
        .iter()
        .zip(pairs)
        .map(|(&page_index, &[reference_end, text_end])| PdfPageText {
            reference: take(reference_end),
            text: take(text_end),
            page_index,
        })
        .collect())
}

pub struct HighlightConfig {
    pub highlight_prefix: String,
    pub highlight_postfix: String,
    pub max_chars: u32,
}

pub struct SearchPageResult {
    pub total_count: u32,
    pub results: Vec<SearchResult>,
    /// `true` when a broad single-word query overflowed its collection budget
    /// and only the highest-priority term expansions were served, so both
    /// `total_count` and `results` are partial (see
    /// [`SearchEngine::single_regex_term_query`]). The regex, advanced and
    /// *vocalized* exact/fuzzy paths can degrade this way (a vocalized word
    /// materializes a term set like an advanced word); the mark-free
    /// exact/fuzzy paths never do this. A *grouped* search on any path can
    /// also set it when the group cap overflows
    /// ([`GROUP_COLLECTOR_MAX_GROUPS`]) — then `group_count` is a lower
    /// bound while `total_count` stays exact.
    pub truncated: bool,
    /// מספר הקבוצות הכולל כשהחיפוש רץ עם `grouping` — זה המספר שדפדוף
    /// (limit/offset) נספר בו. `None` בחיפוש שטוח. `total_count` נשאר
    /// תמיד ספירת התוצאות הגולמיות.
    pub group_count: Option<u32>,
}

/// One event of a combined stream search (`search_*_stream_with_counts`).
///
/// The first event carries the counts computed in the *same* index pass as
/// the ranked results (`total_count` + `book_counts`, with empty `results`);
/// every following event is a snippet-built results chunk (`None` counts).
/// One user search previously cost three full query executions — stream,
/// total count, and count-by-book — this collapses them into one.
pub struct SearchStreamUpdate {
    /// Full hit count of the query; `Some` only on the first event.
    pub total_count: Option<u32>,
    /// Live-document count per distinct `filePath`; `Some` only on the first
    /// event. Sums to `total_count`.
    pub book_counts: Option<HashMap<String, u32>>,
    /// The results chunk (empty on the first, counts-bearing event).
    pub results: Vec<SearchResult>,
    /// `true` when a broad single-word query overflowed its collection budget
    /// and only the highest-priority term expansions were served, so both the
    /// counts and the results are partial (see [`SearchEngine::single_regex_term_query`]).
    /// Meaningful only on the first, counts-bearing event; always `false` on
    /// result chunks and on the mark-free exact/fuzzy paths, which never
    /// degrade this way (the vocalized exact/fuzzy paths can, like the
    /// advanced path). A *grouped* search on any path can also set it when
    /// the group cap overflows ([`GROUP_COLLECTOR_MAX_GROUPS`]) — then
    /// `group_count` is a lower bound while `total_count`/`book_counts`
    /// stay exact. The UI surfaces this as a "results may be partial —
    /// narrow the search" warning.
    pub truncated: bool,
    /// מספר הקבוצות הכולל כשהחיפוש רץ עם `grouping`; `Some` רק באירוע
    /// הראשון (נושא-הספירות), כמו `total_count`. `total_count` ו-
    /// `book_counts` נשארים ספירות גולמיות גם בחיפוש מקובץ.
    pub group_count: Option<u32>,
}

pub struct FacetCount {
    pub path: String,
    pub count: u64,
}

/// A total hit count paired with the single-word truncation flag — the
/// status-bearing return of [`SearchEngine::count_with_status`] and its
/// advanced/exact/fuzzy variants. `truncated` carries the same meaning as
/// [`SearchStreamUpdate::truncated`]: `true` when a broad single-word query
/// overflowed its collection budget, so `count` undercounts the true total.
pub struct CountResult {
    pub count: u32,
    pub truncated: bool,
}

/// Per-`filePath` live-document counts paired with the truncation flag — the
/// status-bearing return of [`SearchEngine::count_by_book_with_status`] and
/// its advanced/exact/fuzzy variants. When `truncated`, the per-book counts
/// are partial.
pub struct BookCountResult {
    pub counts: HashMap<String, u32>,
    pub truncated: bool,
}

/// Per-child facet counts paired with the truncation flag — the status-bearing
/// return of [`SearchEngine::get_facet_counts_with_status`] and its
/// advanced/exact/fuzzy variants. When `truncated`, the facet counts are
/// partial.
pub struct FacetCountsResult {
    pub counts: Vec<FacetCount>,
    pub truncated: bool,
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

// ── Semantic sidecar FFI API ─────────────────────────────────────────────────
//
// The lexical query mode and the retrieval mode deliberately have separate
// types. The former says how Tantivy interprets the text; the latter says which
// retrieval paths the sidecar may use. Keeping that boundary here prevents a
// caller from accidentally treating e.g. a fuzzy query as a semantic mode.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticLexicalMode {
    Exact,
    Fuzzy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticRetrievalMode {
    Hybrid,
    SemanticOnly,
    LexicalOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticGroupingMode {
    SameSection,
    IdenticalText,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticExecutedMode {
    Disabled,
    Hybrid,
    SemanticOnly,
    LexicalOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticResultSource {
    Lexical,
    Semantic,
    Both,
}

/// Configuration for a semantic session whose vectors are built on this
/// device, by [`SearchEngine::configure_semantic`] and
/// [`SearchEngine::semantic_index_books`].
///
/// **Development and testing scaffolding.** The application never builds the
/// library's vectors: the build machine embeds the library into an artifact,
/// and the application opens it with [`SearchEngine::open_semantic_artifact`]
/// and embeds only the query. This path is for the test suites, and for trying
/// a model out before a build machine embeds a library with it.
///
/// The model itself is loaded lazily when indexing begins, so configuration is
/// cheap; searches report a degraded state until indexing has loaded the model
/// and produced vectors, instead of making the lexical engine unusable.
///
/// Every field but `root_dir` and `onnx_runtime_path` describes how the vectors
/// are produced, and the sidecar records each one in its manifest as part of the
/// index's identity (the model file by its checksum, once it has loaded). An index
/// built under one value is reported as needing a full re-index under another,
/// rather than having vectors that cannot be compared mixed into it.
/// Nothing here is read from the model file, so the values must be the ones the
/// model was built for. The model the application knows is the Meivin ONNX
/// model: its INT8 graph by default, or the full-precision
/// `seforim-embed-round2-fp32.onnx` published beside it, which differs only in
/// `model_path` and `"fp32"`:
///
/// | field | Meivin ONNX |
/// | --- | --- |
/// | `model_path` | `seforim-embed-round2-int8.onnx` |
/// | `embedding_dim` | 256 |
/// | `pooling` | `"in-graph"` |
/// | `max_tokens` | 256 |
/// | `model_quantization` | `"int8"` |
/// | `embedding_text_version` | 2 |
pub struct SemanticConfigInput {
    pub root_dir: String,
    /// The model file: an ONNX graph, a path ending in `.onnx` (in any letter
    /// case), with its `tokenizer.json` in the same directory. ONNX Runtime is
    /// the only embedding backend there is. GGUF models, which llama.cpp served
    /// until it was removed, are not supported, and nor is any other file: a
    /// path that does not end in `.onnx` is refused by its name when the model
    /// loads, which indexing does, as `ModelInvalid` with `field` `model_path`.
    /// An ONNX graph on a build without the ONNX backend fails to load as
    /// `BackendNotInBuild`.
    ///
    /// An ONNX graph also needs the ONNX Runtime shared library, which this
    /// library does not link but loads when the model loads, from the first of
    /// three places that is set: [`Self::onnx_runtime_path`]; else the file named
    /// by the `OTZARIA_ONNX_RUNTIME` environment variable; else the platform's
    /// `onnxruntime.dll` / `libonnxruntime.so` / `libonnxruntime.dylib` beside
    /// the graph. Without one that loads, loading the model fails with "ONNX
    /// Runtime could not be loaded", which says what each place held. That is
    /// not `BackendUnavailable`: the backend is in the build, and the fix is the
    /// library, not a rebuild. Lexical search is unaffected either way.
    pub model_path: String,
    pub model_id: String,
    pub embedding_dim: u32,
    /// How the model's output becomes one vector per text. `"in-graph"`, the
    /// pooling the ONNX backend performs, means the graph itself emits the
    /// finished sentence vector (the Meivin graph pools, projects and normalizes
    /// inside), so nothing is pooled outside it. Spellings are matched exactly,
    /// and a pooling the backend does not perform, such as `"mean"`, is refused
    /// like an unknown one.
    ///
    /// Identity because the same weights pooled two ways produce two unrelated
    /// vector spaces.
    pub pooling: String,
    /// The token cap per embedded text, counted the way the model's backend
    /// counts it: the whole sequence. 256 for the Meivin graph, where `[CLS]`,
    /// `[SEP]` and the role-prefix token all count. Longer texts are truncated,
    /// and a cap that leaves no room for content is refused. So is a cap above
    /// 65,536, past the context of any ONNX sentence encoder, when configuring:
    /// a negative Dart value arrives here as a cap in the billions, and the
    /// load-time probe of that many tokens could not even be allocated.
    ///
    /// Identity because a different cap cuts every long text somewhere else, and
    /// so changes its vector. The manifest records the value requested here, not
    /// one a backend may clamp it to.
    pub max_tokens: u32,
    /// The precision of the model's weights: `"int8"` for the Meivin INT8 graph
    /// the application uses, and `"fp32"` for the full-precision graph published
    /// beside it, which is a different identity. Not the precision the vectors
    /// are stored at. Must not be empty.
    ///
    /// Identity because two quantizations of one model produce different
    /// vectors. The model file's checksum catches such a swap as well, but only
    /// once indexing has loaded the model; this label is compared the moment an
    /// index is opened.
    pub model_quantization: String,
    /// Which text each chunk, and each query, carries to the model. Version 1
    /// embeds the line itself, or a short line together with its neighbours.
    /// Version 2 prefixes `"[PASSAGE] "` to that same text and `"[QUERY] "` to
    /// the query: the Meivin model's learned role tokens, which it was trained to
    /// see. A version the sidecar does not implement is refused.
    ///
    /// Identity because it changes the string that is embedded. It is folded
    /// into the chunking identity that the manifest and every book record carry.
    pub embedding_text_version: u32,
    /// The ONNX Runtime shared library to load an ONNX model on, for an
    /// application that ships the runtime itself: on macOS inside its signed
    /// bundle, which is where a Hardened Runtime application can load it from.
    /// An absolute path; a relative one is resolved against the process's
    /// current directory.
    ///
    /// Passed, it is the first place the runtime is looked for and the only
    /// one: a path that names no file is `OnnxRuntimeMissing` and one that does
    /// not load `OnnxRuntimeUnusable`, never a fall-back to
    /// `OTZARIA_ONNX_RUNTIME` or to the file beside the graph, which would run a
    /// runtime the application did not choose. `None` looks at those two, in
    /// that order (see [`Self::model_path`]). An empty path names nothing and is
    /// refused as `InvalidInput`.
    ///
    /// Not identity: where the runtime lives decides no vector, the manifest does
    /// not record it, and moving it invalidates nothing. It is an input of the
    /// session all the same, compared as the others are, because ONNX Runtime is
    /// loaded once per process and cannot be replaced: a repeat call naming
    /// another runtime is a `SessionConflict`, where a no-op would leave the
    /// caller believing the runtime it named is the one in use. After
    /// [`SearchEngine::disable_semantic`], a session that names a runtime other
    /// than the one this process already loaded is refused when its model loads,
    /// as `OnnxRuntimeUnusable`, until the process restarts.
    pub onnx_runtime_path: Option<String>,
}

/// What [`SearchEngine::open_semantic_artifact`] opens: the vector set installed on this
/// device, built on the build machine from the library's text, and the model this device
/// embeds queries with.
///
/// The set states the identity its vectors were built under, and opening compares every
/// field of it with this installation's: the line recipe this build indexes with, the
/// model's published identity file, and the store format this build reads. Nothing in
/// this struct is a value to type in.
///
/// A stored vector is addressed by the key of the text it was embedded from, not by a line
/// id, so the set needs no stamp in the lexical index and goes on serving it whatever is
/// committed to it: every hit is tied to the lines that hold its text today when it is
/// searched.
///
/// The application's installation, and where each input points:
///
/// ```text
/// <root>/
/// ├── otzaria/                  the data folder
/// │   ├── seforim.db
/// │   └── <model>/              the model package
/// │       ├── seforim-embed-round2-int8.onnx      model_path
/// │       ├── tokenizer.json
/// │       └── model.json        the identity file: model_identity_json
/// ├── index/                    the lexical index
/// └── vectors/                  the vector set: vectors_dir
/// ```
///
/// ONNX Runtime either ships with the application, which passes its path as
/// `onnx_runtime_path` (on macOS from inside the signed bundle), or sits in `<model>/`
/// beside the graph under the platform's file name, where it is found without one; it is
/// then the build for that machine's operating system and architecture. Neither the
/// identity file nor a runtime in that folder is part of the model package's checksum.
pub struct SemanticArtifactInput {
    /// The vector set's directory, `<root>/vectors`, where a release is installed: its
    /// `CURRENT` generation, and the segments it names.
    pub vectors_dir: String,
    /// The model queries are embedded with: an `.onnx` graph, with its
    /// `tokenizer.json` beside it, as for [`SemanticConfigInput::model_path`],
    /// and needing the ONNX Runtime library described there.
    pub model_path: String,
    /// The text of the model's identity file: the JSON `ModelIdentity` the
    /// vectors were built with, such as the sidecar's
    /// `config/models/meivin-round2-onnx/model.json` for the Meivin model. Text
    /// rather than a path, so an application can ship it as an asset.
    ///
    /// It describes the model family, every package of it the set may be queried
    /// with among `query_packages`. Every field is compared: the family and recipe
    /// fields with the set's; the graph at `model_path` must be one of
    /// `query_packages`, by its checksum; and `tokenizer_checksum` is compared with
    /// the tokenizer beside it once the model has loaded. So an identity file that
    /// describes other weights is refused rather than trusted.
    pub model_identity_json: String,
    /// The ONNX Runtime library the application ships, as for
    /// [`SemanticConfigInput::onnx_runtime_path`]: the first place looked and,
    /// once passed, the only one; `None` for `OTZARIA_ONNX_RUNTIME` and then the
    /// file beside the graph. Opening loads the model, so a runtime that is
    /// missing or does not load is refused here, by kind. No identity field reads
    /// it, so it makes no set the wrong one; it is part of what a repeat call is
    /// compared on, since the process keeps the first runtime it loads.
    pub onnx_runtime_path: Option<String>,
    /// How many threads a search scans the set with: `None` for the sidecar's
    /// default, half the cores and at most eight. `0` is refused.
    pub scan_threads: Option<u32>,
}

/// A release of the library's vectors, as [`SearchEngine::install_semantic_vectors`]
/// installs it: the segment and the release manifest published beside it.
pub struct SemanticVectorsInstallInput {
    /// The vector set to install into, `<root>/vectors`: created when it does not exist.
    pub vectors_dir: String,
    /// The release's segment, as downloaded: `.oxv`, or `.oxv.zst` compressed with zstd.
    /// Inside the set's `incoming/` folder it is moved into the set; anywhere else it is
    /// read and left where it is.
    pub segment_path: String,
    /// The release manifest published beside the segment (`release.json`), as published:
    /// its bytes are what the published digest names.
    pub manifest_json: String,
    /// The SHA-256 of `manifest_json` as the release publishes it outside the manifest.
    /// Without it an install detects damage and the wrong release, not one deliberately
    /// rebuilt to match.
    pub published_manifest_sha256: Option<String>,
    /// The model identity this installation queries with, as for
    /// [`SemanticArtifactInput::model_identity_json`]: a release it would not open is not
    /// installed.
    pub model_identity_json: String,
}

/// What a segment is to the set it joins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticVectorsPackageKind {
    /// Every vector of one library version; replaces whatever the set held.
    Base,
    /// What changed from one library version to the next.
    Delta,
    /// A device's own merge of its set into one segment.
    Compacted,
}

/// What [`SearchEngine::install_semantic_vectors`] did.
pub struct SemanticVectorsInstallReport {
    pub kind: SemanticVectorsPackageKind,
    /// The library version the set stands at afterwards.
    pub library_version: u32,
    /// The set's live generation afterwards.
    pub generation: u64,
    pub segments: u32,
    pub slots_added: u64,
    /// Older vectors the release's tombstones deleted.
    pub tombstones_applied: u64,
    /// Older vectors of texts the release shipped again, deleted in favour of its own.
    pub duplicates_removed: u64,
    /// Records of the release whose text no older segment holds live.
    pub foreign_unresolved: u64,
    pub bytes_on_disk: u64,
    /// Whether [`SearchEngine::compact_semantic_vectors`] would compact the set now.
    pub needs_compaction: bool,
    /// A delta the set already stood at or past: nothing changed. A base always replaces
    /// the set, so it is never already applied.
    pub already_applied: bool,
}

/// When [`SearchEngine::compact_semantic_vectors`] compacts, and how. The defaults are the
/// sidecar's.
// `non_opaque` so Dart constructs it with these defaults, as `SemanticRankingOptions`.
#[frb(non_opaque)]
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticCompactionPolicy {
    /// Compact once the deltas together are this fraction of the base or more.
    #[frb(default = 0.2)]
    pub max_delta_ratio: f64,
    /// Compact once the set has more segments than this.
    #[frb(default = 4)]
    pub max_segments: u32,
    /// Compact once this fraction of the vectors is dead.
    #[frb(default = 0.05)]
    pub max_dead_ratio: f64,
    /// Refuse to start without this many times the output's size free.
    #[frb(default = 1.15)]
    pub min_free_space_factor: f64,
    /// Re-anchor every record on the live index, when it holds the set's library version
    /// and has the `chunkKey` column.
    #[frb(default = true)]
    pub refresh_hints: bool,
    /// Compact whatever the thresholds say.
    #[frb(default = false)]
    pub force: bool,
}

impl SemanticCompactionPolicy {
    /// The policy a compaction passed none runs under, read from the engine.
    #[frb(sync)]
    pub fn defaults() -> Self {
        Self {
            max_delta_ratio: 0.2,
            max_segments: 4,
            max_dead_ratio: 0.05,
            min_free_space_factor: 1.15,
            refresh_hints: true,
            force: false,
        }
    }
}

/// What [`SearchEngine::compact_semantic_vectors`] did.
pub struct SemanticCompactionReport {
    /// Whether the set was compacted; `reason` says why, or why not.
    pub compacted: bool,
    pub reason: String,
    /// The set's live generation afterwards.
    pub generation: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    pub slots_before: u64,
    pub slots_after: u64,
    /// Records whose book no longer holds their text, dropped.
    pub records_pruned: u64,
    /// Records moved to the line that holds their text now.
    pub hints_refreshed: u64,
    pub elapsed_ms: u64,
}

/// One segment of an installed vector set.
pub struct SemanticSegmentInfo {
    pub id: String,
    pub kind: SemanticVectorsPackageKind,
    pub from_library_version: u32,
    pub to_library_version: u32,
    pub slots: u64,
    pub slots_dead: u64,
    pub foreign_unresolved: u64,
    pub size: u64,
}

/// What a vector set's directory holds, as [`SearchEngine::semantic_vectors_info`] reads it.
pub struct SemanticVectorsInfo {
    /// Whether anything is installed there; every other field is empty when not.
    pub present: bool,
    /// The set's identity as 64 hex digits: what a release must name to be installed.
    pub identity_digest: String,
    /// The model family bound into the installed set's identity, empty when absent.
    pub model_family_id: String,
    pub library_version: u32,
    pub library_release_tag: String,
    pub generation: u64,
    pub segments: Vec<SemanticSegmentInfo>,
    pub slots_live: u64,
    pub slots_dead: u64,
    pub bytes_on_disk: u64,
    pub needs_compaction: bool,
    /// The live generation did not open and the one before it was opened instead.
    pub recovered_from_previous: bool,
}

/// What [`SearchEngine::verify_semantic_vectors`] read: every block of every segment of the
/// set's live generation, each against its checksum.
pub struct SemanticVectorsVerification {
    pub generation: u64,
    pub segments: u32,
    pub bytes_checked: u64,
    pub elapsed_ms: u64,
}

/// How much of the live index a vector set covers, as [`SearchEngine::semantic_coverage`]
/// counts it.
pub struct SemanticCoverage {
    /// Live lines the recipe embeds: the lines a vector could exist for.
    pub live_keyed_lines: u64,
    /// Those whose text the set holds a vector for.
    pub covered_lines: u64,
    /// Books with a line the recipe embeds.
    pub books_live: u32,
    /// Books with a covered line.
    pub books_covered: u32,
    /// The library version the set stands at.
    pub vectors_library_version: u32,
    /// `covered_lines / live_keyed_lines`, or 0 when no line is keyed.
    pub ratio: f64,
}

/// What stopped the semantic path, as a value an application can switch on to choose a
/// message and an action. A [`SemanticError`] carries one when a semantic call fails,
/// [`SemanticStatus::error_kind`] when a session cannot serve, and
/// [`SemanticSearchResponse::fallback_kind`] when a search fell back to lexical results.
///
/// The message beside it is unchanged, and is still the one for a developer to read: it
/// names every path, value and field involved. The kind is what to branch on, never the
/// message. It is decided from the type of the failure, the sidecar's typed errors and this
/// crate's own, and never by reading a message, so a reworded error cannot move a failure
/// from one kind to another. Where the type alone cannot settle it, a fact does: whether the
/// artifact's `manifest.json` exists (missing, or damaged), and whether there is a file where
/// ONNX Runtime is looked for (missing, or unusable). A failure that still cannot be placed
/// precisely gets the broad kind that is true of it, `Internal` last.
///
/// More kinds will be added. A Dart `switch` over this enum therefore needs a default
/// branch, and a kind the application does not know yet is best handled as `internal`.
///
/// | kind | means | the application should | reported by |
/// | --- | --- | --- | --- |
/// | `NotConfigured` | no semantic session is open: none was opened, or `disable_semantic` closed it | open the vectors; lexical search is unaffected | status, search fallback |
/// | `FeatureNotInBuild` | this library was built without semantic support | hide semantic search; no file or setting changes it | status, search fallback |
/// | `ArtifactMissing` | there is no vector set at `vectors_dir`: no directory, or nothing ever installed in it (no `CURRENT` or `PREVIOUS`) | download and install the vectors | `open_semantic_artifact`, `verify_semantic_vectors`, `semantic_coverage` |
/// | `ArtifactCorrupt` | the vectors are damaged: a set whose pointers, metadata or segments do not open or fail their checksums, or a release whose segment is not the one its manifest describes | install the release again, which repairs the set, downloading it again if it is gone; on a FAT or exFAT drive on Windows, close the session first | `open_semantic_artifact`, installing, verifying, `semantic_vectors_info` |
/// | `ArtifactIncompatible` | sound vectors built for something else: lines made by another line recipe, another model or chunking, a store format this build does not read, or a delta that does not follow the installed set; `field` names the first field that disagreed. With `field` `segment_id`, installing: the release is a version the set has installed, published again with other bytes — a sound release, and a sound set, which keeps what it serves | install the vectors built for this application and this model; for `segment_id`, do not download it again, which is refused the same way: keep the set, or install the release into a new, empty `vectors_dir` and open that | `open_semantic_artifact`, installing |
/// | `ArtifactNotPublished` | self-consistent, but the release's manifest is not the one published for it | download the official release again | installing |
/// | `InsufficientDiskSpace` | installing or compacting vectors needs more free space than the device has | free space, and try again | installing, compacting |
/// | `ModelMissing` | there is no model file at `model_path` | download the model | `open_semantic_artifact`, `semantic_index_books` |
/// | `TokenizerMissing` | an ONNX graph without its `tokenizer.json` beside it | install the model's whole package | `open_semantic_artifact`, `semantic_index_books` |
/// | `ModelInvalid` | the file at `model_path` is not a usable model (a truncated download, a placeholder), or its backend could not load it; or `model_path` names no ONNX graph, such as a GGUF, which no build serves since GGUF support was removed (`field` is `model_path` then) | download the model again; for a path that names no ONNX graph, install the ONNX model and point `model_path` at its graph | `open_semantic_artifact`, `semantic_index_books` |
/// | `ModelIdentityMismatch` | the model identity in hand (`model_identity_json`, or the configuration) does not describe the model at `model_path`; `field` names what differs | ship the identity file published with this model, or the model it describes | `open_semantic_artifact`, `semantic_index_books` |
/// | `OnnxRuntimeMissing` | an ONNX model, and no ONNX Runtime library where one is looked for: an `onnx_runtime_path` that names no file; or, with none passed, `OTZARIA_ONNX_RUNTIME` unset or naming no file and none beside the graph | install ONNX Runtime where `onnx_runtime_path` names, or beside the model when none is passed | `open_semantic_artifact`, `semantic_index_books` |
/// | `OnnxRuntimeUnusable` | there is a runtime library, and it cannot be used: not loadable, not ONNX Runtime, older than 1.17, refused earlier in this process, or a different one already loaded | replace it with a supported ONNX Runtime; for the last two, restart the process | `open_semantic_artifact`, `semantic_index_books` |
/// | `BackendNotInBuild` | this build has no embedding backend for the model: an ONNX graph on Android or iOS, or in a build without the ONNX backend | use a build that has the ONNX backend; no file fixes it | `open_semantic_artifact`, `semantic_index_books` |
/// | `SessionConflict` | another semantic session is open, or this one with different inputs | `disable_semantic` first, if replacing it is intended | `configure_semantic`, `open_semantic_artifact` |
/// | `ReadOnlySession` | a call that builds vectors, on an opened artifact, which is read-only | nothing: the device does not build the library's vectors | `semantic_index_books`, `semantic_index_diff`, `remove_semantic_books`, `reset_semantic_index` |
/// | `ReindexRequired` | a session built on this device holds vectors built under another configuration | `reset_semantic_index`, and index again (development) | `semantic_index_books` |
/// | `QueryFailed` | the semantic half of one search failed, and its lexical results were served; the sidecar reports why as text only, so this is not split further | show the results; [`SearchEngine::semantic_status`] says whether the session still serves | search fallback |
/// | `Cancelled` | the search was abandoned through its [`SemanticCancellationToken`]: not a failure, and it was not answered with lexical results instead | nothing: drop it, since the query that cancelled it is the one that matters; a cancelled install, compaction, verification or count left the set as it was | `search_semantic`, and the calls that install, compact, verify or count a vector set |
/// | `InvalidInput` | an input the call cannot take: an empty `model_quantization` or `onnx_runtime_path`, a `model_identity_json` that is not an identity, a pooling or text recipe no backend serves, a token cap out of range, a ranking option or a compaction threshold out of its range; `field` names it when it is known | fix the call: a programming error, not a state of the device | `configure_semantic`, `open_semantic_artifact`, `search_semantic`, `compact_semantic_vectors` |
/// | `Internal` | anything else: an I/O error, a fault inside the engine or the lexical index, a failure the sidecar reports only as text | report it, with the message | any call; status, for a session built on this device |
/// | `VectorsBusy` | another install or compaction of the same vector set is running, in this process or another (`field` is `vectors_dir`); nothing was read or changed, and an open session keeps serving. Verifying: an install replaced bytes the check had read while it read them, and nothing was condemned | try again once it has finished; never `disable_semantic` for it | installing, compacting, verifying |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SemanticErrorKind {
    /// No semantic session is open.
    NotConfigured,
    /// This library was built without semantic support.
    FeatureNotInBuild,
    /// No vector set at the vectors directory.
    ArtifactMissing,
    /// The vectors are damaged.
    ArtifactCorrupt,
    /// Sound vectors, built for another line recipe, model or store format.
    ArtifactIncompatible,
    /// The release is not the one whose digest was published.
    ArtifactNotPublished,
    /// Not enough free space to install or compact the vectors.
    InsufficientDiskSpace,
    /// No model file at the model path.
    ModelMissing,
    /// An ONNX graph without its `tokenizer.json`.
    TokenizerMissing,
    /// The model file is not a usable model, could not be loaded, or is no ONNX graph.
    ModelInvalid,
    /// The model identity in hand does not describe the model file.
    ModelIdentityMismatch,
    /// An ONNX model, and no ONNX Runtime library where one is looked for.
    OnnxRuntimeMissing,
    /// The ONNX Runtime library found cannot be used.
    OnnxRuntimeUnusable,
    /// No embedding backend for the model in this build: no ONNX backend here.
    BackendNotInBuild,
    /// Another semantic session is open, or this one with different inputs.
    SessionConflict,
    /// A call that builds vectors, on a read-only artifact.
    ReadOnlySession,
    /// A session built on this device holds vectors from another configuration.
    ReindexRequired,
    /// The semantic half of one search failed; lexical results were served.
    QueryFailed,
    /// The search was cancelled through its token before it finished.
    Cancelled,
    /// An input the call cannot take.
    InvalidInput,
    /// Anything else; the message says what.
    Internal,
    /// Another install or compaction of the same vector set is running; nothing was read or
    /// changed, and an open session keeps serving. Try again once it has finished.
    VectorsBusy,
}

/// A semantic call that failed. Dart receives it as a thrown `SemanticError`, an
/// `FrbException`, where these calls used to throw `AnyhowException`: flutter_rust_bridge
/// throws the error type of a `Result` as an exception class of its own.
///
/// `message` is the text the call has always produced, with every path, value and field that
/// disagreed, and is for a developer to read. `kind` is what an application branches on —
/// [`SemanticErrorKind`] says what each means and what to do — and `field` names the one
/// field the failure is about, when there is one:
///
/// | kind | `field` |
/// | --- | --- |
/// | `ArtifactIncompatible` | the first field that disagreed, by its path in the set's identity: `text.line_text_version`, `model.family_id`, `model.chunking_identity`, `store.store_format_version`, `store.vector_precision`, or `metadata_version`; for a delta that does not follow the set, `delta.identity`, `delta.codec_params` or `delta.from_library_version`; `segment_id`, installing a version the set has installed that was published again with other bytes |
/// | `ArtifactCorrupt` | the identity field left unfilled, when that is the damage; `manifest_json`, for a release manifest that does not read |
/// | `ModelInvalid` | `model_path`, when the path names no ONNX graph, such as a GGUF |
/// | `ModelIdentityMismatch` | the key of the model identity that the model contradicts: `query_packages`, `tokenizer_checksum`, `embedding_dim` or `pooling` |
/// | `VectorsBusy` | `vectors_dir`: the vector set another install or compaction is running on |
/// | `InvalidInput` | the input at fault, when it is known: `model_quantization`, `max_tokens`, `model_identity_json`, `pooling`, `embedding_text_version`, `normalization_version`, `vectors_dir`, `segment_path`, `onnx_runtime_path`, `scan_threads`; for a ranking, the option as [`SemanticRankingOptions`] names it, `alpha_by_query_type.short` or `rrf_k` say; for a compaction, `policy.` and the [`SemanticCompactionPolicy`] option |
///
/// It is `None` for every other kind, and wherever the failure does not say.
#[frb(dart_code = r#"
  @override
  String toString() =>
      'SemanticError(${kind.name}${field == null ? '' : ', $field'}): $message';
"#)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticError {
    /// What went wrong, as the value to branch on.
    pub kind: SemanticErrorKind,
    /// The detailed description, as the call produced it before it had a kind.
    pub message: String,
    /// The field the failure is about, when it is about one.
    pub field: Option<String>,
}

impl SemanticError {
    pub(crate) fn new(kind: SemanticErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            field: None,
        }
    }

    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    pub(crate) fn with_field(self, field: impl Into<String>) -> Self {
        Self {
            field: Some(field.into()),
            ..self
        }
    }

    /// What a search whose token was cancelled throws, wherever the cancel was noticed: by
    /// this crate's own look at the token or by the sidecar's.
    pub(crate) fn cancelled() -> Self {
        Self::new(
            SemanticErrorKind::Cancelled,
            "the search was cancelled before it finished; nothing failed",
        )
    }
}

/// The message alone, as these errors read before they had a kind: Rust callers that format
/// one see the same text Dart's `message` holds.
impl std::fmt::Display for SemanticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SemanticError {}

/// What a semantic call's `?` meets when the lexical index fails under it: the one source of
/// `anyhow` errors on that path. A failure that has a kind of its own is converted where it
/// arises, never through this, so what arrives here is `Internal`. The whole context chain
/// is kept, on one line.
impl From<anyhow::Error> for SemanticError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(SemanticErrorKind::Internal, format!("{error:#}"))
    }
}

/// What the semantic session can do right now, the value a status view switches on.
/// [`SemanticStatus::available`] says only whether semantic search is served; this says
/// why it is not, and so what to offer.
///
/// Only `NotInBuild`, `NotConfigured`, `Ready` and `Stale` occur on the application's path.
/// An artifact either opens and serves, or is refused by
/// [`SearchEngine::open_semantic_artifact`] with a [`SemanticError`] and leaves no session
/// behind; the other states belong to a session whose vectors are built on this device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SemanticState {
    /// Semantic support is not compiled into this library: an explicit state rather than a
    /// silent fallback. `error_kind` is `FeatureNotInBuild`.
    NotInBuild,
    /// No session is open: none was opened, or `disable_semantic` closed it. `error_kind` is
    /// `NotConfigured`.
    NotConfigured,
    /// A session is open and serves semantic and hybrid search.
    Ready,
    /// A session built on this device is open and has nothing to serve yet: its model is not
    /// loaded or it holds no vectors. Indexing is what loads the model.
    Empty,
    /// A session built on this device holds vectors built under another configuration, and
    /// `needs_full_reindex` says which: reset it and index again.
    NeedsReindex,
    /// A session built on this device is open and cannot serve, for the reason in
    /// `last_error`.
    Failed,
}

/// A serializable, feature-independent projection of sidecar status. It is
/// intentionally available without the `semantic` Cargo feature so Dart can
/// render an explicit `NotInBuild` state rather than silently falling back.
pub struct SemanticStatus {
    /// What the session can do, and the field to switch on.
    pub state: SemanticState,
    pub enabled: bool,
    pub available: bool,
    pub model_loaded: bool,
    pub indexed_book_count: u32,
    pub vector_count: u32,
    pub model_id: String,
    pub embedding_dim: u32,
    pub embedding_backend: Option<String>,
    pub vector_backend: String,
    pub vectors_persisted: bool,
    pub needs_full_reindex: Option<String>,
    pub last_error: Option<String>,
    /// The kind of `last_error`, and `Some` exactly when it is. A session built on this
    /// device reports its own failures as text only, so they are `Internal` here; the call
    /// that failed, usually `semantic_index_books`, threw the precise kind.
    pub error_kind: Option<SemanticErrorKind>,
    /// The library version an opened vector set stands at; `None` for any other session.
    pub vectors_library_version: Option<u32>,
    /// The segments of an opened vector set; 0 for any other session.
    pub vector_segments: u32,
    /// Whether an opened vector set would be compacted now.
    pub needs_compaction: bool,
}

pub struct SemanticBookLineInput {
    pub line_id: u64,
    pub section_id: u64,
    pub text: String,
    pub line_hash: u64,
    pub reference: String,
    pub segment: u64,
}

pub struct SemanticBookInput {
    pub source_book_key: String,
    pub title: String,
    /// The lexical engine's book content hash, or a caller-provided canonical
    /// source fingerprint for PDFs. Zero means it cannot be verified.
    pub content_fingerprint: u64,
    pub is_pdf: bool,
    pub topics: String,
    pub extra_facets: Vec<String>,
    pub lines: Vec<SemanticBookLineInput>,
}

pub struct SemanticIndexingSummary {
    pub enabled: bool,
    pub books_indexed: u32,
    pub books_skipped: u32,
    pub books_empty: u32,
    pub chunks_written: u32,
}

pub struct SemanticIndexDiff {
    pub enabled: bool,
    pub new_books: Vec<String>,
    pub changed_books: Vec<String>,
    pub unverifiable_books: Vec<String>,
    pub removed_books: Vec<String>,
    pub model_mismatch: bool,
    pub chunking_mismatch: bool,
    pub normalization_mismatch: bool,
}

pub struct SemanticResetResult {
    pub enabled: bool,
    pub vectors_removed: u32,
}

pub struct SemanticRemoveResult {
    pub enabled: bool,
    pub vectors_removed: u32,
}

pub struct SemanticSearchResult {
    pub title: String,
    pub reference: String,
    /// The display string, in the same format every other search API in this
    /// engine returns: HTML-escaped and painted with `HighlightConfig`'s
    /// prefix/postfix where the lexical query matched. It is never a raw
    /// unbounded line, on either the sidecar or the fallback path — the app's
    /// snippet parser can treat both alike. `max_chars` bounds how much of the
    /// line is shown; markup and escaping are added on top of that.
    ///
    /// The sidecar path paints against the mark-free stored `text` field,
    /// because that is the copy the sidecar indexes and hydration reads. A
    /// vocalized query therefore selects documents with marks but still shows
    /// (and highlights) the mark-free line; the lexical fallback shows the
    /// vocalized copy. Use [`SearchEngine::get_document_by_id`] when the full,
    /// unabridged line is needed.
    pub snippet_html: String,
    /// Whether `snippet_html` carries highlight markup. False for a purely
    /// semantic hit, whose line matched no query term — the UI can then avoid
    /// promising the user a lexical match that is not there.
    pub is_highlighted: bool,
    pub id: u64,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
    pub merged_count: u32,
    pub merged: Vec<MergedSibling>,
    pub lexical_score: Option<f32>,
    pub semantic_score: Option<f32>,
    pub fused_score: f32,
    pub source: SemanticResultSource,
    /// False after successful Tantivy hydration of a semantic-only item.
    pub needs_hydration: bool,
    /// As [`SearchResult::text_status`]: anything but `Ok` leaves `snippet_html`
    /// unpainted (`Stale`) or empty (`Unavailable`).
    pub text_status: TextStatus,
    /// The window of a long line the semantic hit matched (0: the line's first); past 0,
    /// `snippet_html` shows that window. 255 is no window: a short line found through the
    /// neighbour whose vector it shares, shown whole. Pass it on as
    /// [`SemanticHighlightTarget::window`].
    pub matched_window: u8,
}

/// Result envelope for semantic/hybrid searches. `lexical_total_count` is the
/// truthful corpus-wide Tantivy count; `total_count` is the sidecar candidate
/// fusion count and must not be presented as a corpus-wide semantic total.
// `non_opaque` for the reason given on `SemanticQueryTypeAlphas`.
#[frb(non_opaque)]
pub struct SemanticSearchResponse {
    pub results: Vec<SemanticSearchResult>,
    pub total_count: u32,
    pub lexical_total_count: u32,
    pub group_count: Option<u32>,
    /// Whether `total_count`/`group_count` are exact. Sidecar-backed searches
    /// report candidate-window counts, not a corpus-wide semantic total, and
    /// may also exclude stale records during Tantivy hydration.
    /// `lexical_total_count` uses `truncated` as its accuracy signal instead.
    pub counts_are_exact: bool,
    pub requested_mode: SemanticRetrievalMode,
    pub executed_mode: SemanticExecutedMode,
    pub semantic_available: bool,
    pub fallback_reason: Option<String>,
    /// Why the semantic path did not serve this search, when it was asked to and did not:
    /// `NotConfigured`, `FeatureNotInBuild` or `QueryFailed`. `None` when
    /// it served the search, and when it was not asked (`LexicalOnly`, or a quoted phrase
    /// the sidecar answers lexically). `fallback_reason` can still carry a note then, about
    /// stale records dropped or the candidate window capped, which has no kind:
    /// `candidate_window_truncated` is the cap's typed flag.
    pub fallback_kind: Option<SemanticErrorKind>,
    pub latency_ms: u64,
    /// The sidecar input window hit its hard memory-safety ceiling. This is
    /// separate from `truncated`, which belongs to lexical term expansion.
    pub candidate_window_truncated: bool,
    pub truncated: bool,
    /// Whether a page after this one has a result: what paging asks, which the counts above
    /// cannot tell, since they describe a candidate window rather than the results to come.
    #[frb(default = false)]
    pub has_more: bool,
    /// The requested continuation no longer had a kept session. These results are the
    /// first page of a fresh search: replace the displayed list instead of appending.
    #[frb(default = false)]
    pub session_restarted: bool,
}

/// A semantic hit whose passage [`SearchEngine::semantic_passage_highlights`] should mark: the
/// line, by its book and its id, as a [`SemanticSearchResult`] names it.
#[frb(non_opaque)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticHighlightTarget {
    pub file_path: String,
    pub id: u64,
    /// [`SemanticSearchResult::matched_window`]: the window of the line that is marked.
    #[frb(default = 0)]
    pub window: u8,
}

/// A target's line centred on its clause nearest the query, escaped, the clause in `<mark>`; empty
/// when nothing is marked. Separate from the result, whose own snippet stays as it was.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticPassageHighlight {
    pub file_path: String,
    pub id: u64,
    pub snippet_html: String,
    pub is_highlighted: bool,
    /// The cosine of the marked clause and the query, from the one model that embedded both.
    pub span_score: Option<f32>,
}

/// What a passage highlight call computes of its query, once, for the first line that needs it.
#[cfg(feature = "semantic-integration")]
#[derive(Default)]
struct PassageQuery {
    vector: Option<Vec<f32>>,
    words: Option<HashSet<String>>,
    /// The query's own tokens, without the dictionary's forms.
    tokens: Option<HashSet<String>>,
}

/// How the application abandons a [`SearchEngine::search_semantic`] that nobody is waiting
/// for any more: one token per search, created where the search starts and cancelled when a
/// newer query supersedes it.
///
/// The application searches as the user types, so every query but the last is obsolete
/// before it finishes, and a semantic query embeds the text and then scans every stored
/// vector, which over the library is on the order of a second; left to run, the abandoned
/// queries queue up in front of the one that matters. A search whose token is cancelled
/// stops at its next look at it and throws a [`SemanticError`] of kind `Cancelled`, and the
/// search looks often: before its lexical phase, throughout the sidecar's semantic half,
/// and before it hydrates and paints the page. The one stretch a cancel cannot cut short is
/// the embedding of the query, a single inference. A cancelled search is not a failure: it
/// is not answered with lexical results instead, since nobody is waiting for those either,
/// and one the sidecar stops leaves nothing in its caches.
///
/// Opaque, and borrowed by the search rather than moved into it, so Dart keeps the object,
/// and its [`Self::cancel`] reaches the search while it runs. Both take the token by shared
/// reference, so neither waits for the other: `cancel` returns at once on the isolate that
/// started the search. A token cannot be reset; a new search takes a new one. A search that
/// passed its last look before the cancel returns its results all the same, so the
/// application still tells which query a page answers.
#[frb(opaque)]
pub struct SemanticCancellationToken {
    flag: SearchCancellation,
}

impl SemanticCancellationToken {
    /// A token that is not cancelled.
    #[frb(sync)]
    pub fn new() -> Self {
        Self {
            flag: SearchCancellation::default(),
        }
    }

    /// Ask the search holding this token to stop. Returns at once, whether the search has
    /// not started, is running or has finished: it notices at its next look, not here.
    /// Cancelling again changes nothing.
    #[frb(sync)]
    pub fn cancel(&self) {
        self.flag.cancel();
    }

    /// Whether [`Self::cancel`] has been called.
    #[frb(sync, getter)]
    pub fn is_cancelled(&self) -> bool {
        self.flag.is_cancelled()
    }
}

/// For Rust callers. Kept out of the bindings, where flutter_rust_bridge would export it as a
/// second, asynchronous constructor beside [`SemanticCancellationToken::new`].
impl Default for SemanticCancellationToken {
    #[frb(ignore)]
    fn default() -> Self {
        Self::new()
    }
}

/// How [`SemanticRankingOptions::fusion_strategy`] combines the two sides' scores.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticFusionStrategy {
    /// By weight: `alpha` of the normalized BM25 score and `1 - alpha` of the semantic one,
    /// with the agreement bonus and the other bonuses on top.
    Weighted,
    /// Reciprocal rank fusion, `1 / (rrf_k + rank)` from each side. It needs no calibration of
    /// either side's scores; the semantic threshold only decides which semantic candidates
    /// take part, and no bonus or penalty applies.
    Rrf,
    /// By weight, with BM25 min-max normalized when its scores run high.
    Adaptive,
}

/// The lexical weight `alpha` for each kind of query the sidecar tells apart, by the words
/// in it; `1 - alpha` goes to the semantic side. Each a number from 0 to 1. The defaults are
/// the weights the ranking has always used, and are unmeasured (see
/// [`SemanticRankingOptions`]).
// `non_opaque` binds it as every struct of plain values is bound already. It is there
// because an `#[frb]` on the struct is what takes the `#[frb(default)]`s off its fields,
// where rustc would refuse them, and leaves them to the Dart constructor.
#[frb(non_opaque)]
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticQueryTypeAlphas {
    /// A query with a quoted phrase: a verbatim lookup, where the lexical engine is
    /// authoritative. At 1 a hybrid search does not ask the semantic path at all.
    #[frb(default = 1.0)]
    pub quoted_phrase: f64,
    /// One or two words, one of them with a digit: a reference, such as a page or a verse.
    #[frb(default = 0.85)]
    pub exact_reference: f64,
    /// One or two words without a digit.
    #[frb(default = 0.7)]
    pub short: f64,
    /// Three or four words.
    #[frb(default = 0.5)]
    pub mixed: f64,
    /// Five words or more: a question or a description more than a lookup.
    #[frb(default = 0.3)]
    pub conceptual: f64,
    /// No words at all. Nothing can be embedded, so a hybrid search serves its lexical
    /// results whatever this is; at 1 the semantic path is not even tried.
    #[frb(default = 0.5)]
    pub unknown: f64,
}

/// Every parameter a hybrid search ranks by, passed with one [`SearchEngine::search_semantic`]
/// in place of the ranking the engine uses when it is passed none. What lets the application
/// calibrate and tune the ranking without a release of the engine.
///
/// **The defaults are unmeasured placeholders.** They are the sidecar's `Balanced` preset but
/// for the fusion, which is RRF, the semantic threshold, and the foundational books' preference,
/// and none has been checked against what a reader of this library finds relevant: each was
/// reasoned from a scale (BM25's
/// typical range, a cosine of about 0.1 meaning unrelated) or carried over from the
/// literature, as RRF's `k` of 60 is. Calibrating them needs a labelled relevance set, Hebrew
/// queries of every type each with the lines judged relevant to it; a metric over the page a
/// user sees, nDCG@10 or recall at the page size; and runs over that set that vary one family
/// of parameters at a time, the fusion strategy and RRF's `k` first, since RRF needs no score
/// calibration, then alpha per query type, BM25's `k`, the semantic threshold and the bonuses.
///
/// The Dart constructor's defaults are these values, so a caller names only the options it
/// changes; [`Self::defaults`] reads them from the engine. Passed as they are, they rank
/// exactly as passing no options does.
///
/// | option | default | allowed |
/// | --- | --- | --- |
/// | `fusion_strategy` | `Rrf` | |
/// | `rrf_k` | 60 | at least 1, when `fusion_strategy` is `Rrf`; read by nothing else |
/// | `alpha_override` | none | 0 to 1 |
/// | `alpha_by_query_type` | 1, 0.85, 0.7, 0.5, 0.3, 0.5 | each 0 to 1 |
/// | `bm25_saturation_k` | 10 | above 0 |
/// | `semantic_threshold` | 0.55 | 0 to 1 |
/// | `agreement_bonus` | 0.1 | 0 to 1 |
/// | `phrase_match_bonus`, `rare_term_bonus`, `section_coverage_bonus` | 0 | 0 to 1 |
/// | `duplicate_penalty` | 0 | 0 to 1 |
/// | `metadata_ranking_enabled` | false | |
/// | `candidate_window_multiplier` | 2 | 1 to 10 |
/// | `foundational_bonus` | 0.002 | 0 to 1 |
/// | `foundational_candidate_share` | 0.5 | 0 to 1 |
/// | `window_penalty` | 0.04 | 0 to 0.2 |
/// | `alias_penalty` | 0.04 | 0 to 0.2 |
///
/// A value outside its range, or one that is not a number, is refused before the search runs,
/// with a [`SemanticError`] of kind `InvalidInput` whose `field` names the option
/// (`alpha_by_query_type.short`, `rrf_k`), rather than clamped into a value nobody chose: a
/// calibration run is exactly where a substituted value would go unnoticed. The rules are the
/// sidecar's own (`RankingProfile::validate`), applied to each value at the 32-bit precision the
/// ranking computes in. A build without semantic support has no ranking to apply, and ignores
/// the options as it ignores every semantic input.
// `non_opaque` for the reason given on `SemanticQueryTypeAlphas`.
#[frb(non_opaque)]
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticRankingOptions {
    /// How the two sides' scores are combined.
    #[frb(default = "SemanticFusionStrategy.rrf")]
    pub fusion_strategy: SemanticFusionStrategy,
    /// RRF's `k`, for [`SemanticFusionStrategy::Rrf`]: the larger it is, the less the first
    /// ranks of either side count over the ones after them.
    #[frb(default = 60)]
    pub rrf_k: u32,
    /// One alpha for every query, in place of [`Self::alpha_by_query_type`].
    pub alpha_override: Option<f64>,
    /// The lexical weight for each kind of query, when [`Self::alpha_override`] is `None`.
    #[frb(default = "const SemanticQueryTypeAlphas()")]
    pub alpha_by_query_type: SemanticQueryTypeAlphas,
    /// `k` in BM25's normalization `score / (k + score)`: where the curve bends, so that a
    /// score well above it is hardly told apart from the scores above it.
    #[frb(default = 10.0)]
    pub bm25_saturation_k: f64,
    /// Below this normalized similarity a semantic candidate contributes nothing. A cosine is
    /// mapped to `(cosine + 1) / 2`, so an unrelated line, cosine 0, is 0.5.
    #[frb(default = 0.55)]
    pub semantic_threshold: f64,
    /// Added to a line both sides found, in a hybrid search fused by weight.
    #[frb(default = 0.1)]
    pub agreement_bonus: f64,
    /// Scaled by the share of the query's quoted phrases a line contains, and added.
    #[frb(default = 0.0)]
    pub phrase_match_bonus: f64,
    /// Scaled by the share of the query's rare words a line contains, and added.
    #[frb(default = 0.0)]
    pub rare_term_bonus: f64,
    /// Added to a line whose section holds another result.
    #[frb(default = 0.0)]
    pub section_coverage_bonus: f64,
    /// Taken from every line after the first with the same text.
    #[frb(default = 0.0)]
    pub duplicate_penalty: f64,
    /// Whether a semantic candidate gains a signal from its metadata, when fused by weight:
    /// a primary source, by its book, and an era or a category the search's facets match.
    #[frb(default = false)]
    pub metadata_ranking_enabled: bool,
    /// How many semantic candidates are fetched for each place in the candidate window.
    #[frb(default = 2.0)]
    pub candidate_window_multiplier: f64,
    /// Added once to the fused score of a line of a foundational book (under `/base`), in
    /// every strategy and mode. Under RRF, 0.002 is about an eighth of a first rank's score.
    #[frb(default = 0.002)]
    pub foundational_bonus: f64,
    /// A second semantic query, restricted to the foundational books, fetches this share of
    /// the main query's candidates; 0 runs none. Skipped when the facets already name `/base`.
    #[frb(default = 0.5)]
    pub foundational_candidate_share: f64,
    /// Taken off the cosine of a vector of a long line's window past its first, before the
    /// scan keeps its best: such a window matches a part of a line rather than the line.
    #[frb(default = 0.04)]
    pub window_penalty: f64,
    /// Taken off the cosine of a short line found through the neighbour whose vector it
    /// shares (a format-3 alias), in place of the window penalty.
    #[frb(default = 0.04)]
    pub alias_penalty: f64,
}

impl SemanticRankingOptions {
    /// The ranking a search runs on when it is passed none, read from the engine: the defaults
    /// of the Dart constructor, and unmeasured.
    #[frb(sync)]
    pub fn defaults() -> Self {
        Self {
            fusion_strategy: SemanticFusionStrategy::Rrf,
            rrf_k: 60,
            alpha_override: None,
            alpha_by_query_type: SemanticQueryTypeAlphas {
                quoted_phrase: 1.0,
                exact_reference: 0.85,
                short: 0.7,
                mixed: 0.5,
                conceptual: 0.3,
                unknown: 0.5,
            },
            bm25_saturation_k: 10.0,
            semantic_threshold: 0.55,
            agreement_bonus: 0.1,
            phrase_match_bonus: 0.0,
            rare_term_bonus: 0.0,
            section_coverage_bonus: 0.0,
            duplicate_penalty: 0.0,
            metadata_ranking_enabled: false,
            candidate_window_multiplier: 2.0,
            foundational_bonus: 0.002,
            foundational_candidate_share: 0.5,
            window_penalty: 0.04,
            alias_penalty: 0.04,
        }
    }
}

/// Every input that decides which vectors a sidecar session holds: the whole
/// of [`SemanticConfigInput`]. Kept beside the open engine so
/// [`SearchEngine::configure_semantic`] can tell a harmless repeat call from a
/// real change of model, recipe or library root — see that method for why the
/// difference matters.
///
/// All of it, not only the model and the root: a caller that switches pooling or
/// text recipe under an unchanged model file is asking for different vectors
/// just the same, and a key that ignored those fields would answer that request
/// with a no-op and keep serving the old ones.
///
/// The runtime path decides no vector and is in the key all the same. The
/// process keeps the first ONNX Runtime it loads, so a repeat call that names
/// another one is not a repeat: answered with a no-op, it would leave the caller
/// believing the runtime it named is the one embedding its queries.
#[cfg(feature = "semantic-integration")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct SemanticConfigKey {
    root_dir: PathBuf,
    model_path: PathBuf,
    model_id: String,
    embedding_dim: u32,
    pooling: String,
    max_tokens: u32,
    model_quantization: String,
    embedding_text_version: u32,
    onnx_runtime: Option<PathBuf>,
}

#[cfg(feature = "semantic-integration")]
impl SemanticConfigKey {
    fn from_input(config: &SemanticConfigInput) -> Self {
        Self {
            root_dir: PathBuf::from(&config.root_dir),
            model_path: PathBuf::from(&config.model_path),
            model_id: config.model_id.clone(),
            embedding_dim: config.embedding_dim,
            pooling: config.pooling.clone(),
            max_tokens: config.max_tokens,
            model_quantization: config.model_quantization.clone(),
            embedding_text_version: config.embedding_text_version,
            onnx_runtime: config.onnx_runtime_path.as_ref().map(PathBuf::from),
        }
    }

    /// Names the fields that differ, so a refused reconfiguration says which
    /// input changed instead of only that something did.
    fn changed_fields(&self, other: &Self) -> String {
        let mut changed = Vec::new();
        if self.root_dir != other.root_dir {
            changed.push("root_dir");
        }
        if self.model_path != other.model_path {
            changed.push("model_path");
        }
        if self.model_id != other.model_id {
            changed.push("model_id");
        }
        if self.embedding_dim != other.embedding_dim {
            changed.push("embedding_dim");
        }
        if self.pooling != other.pooling {
            changed.push("pooling");
        }
        if self.max_tokens != other.max_tokens {
            changed.push("max_tokens");
        }
        if self.model_quantization != other.model_quantization {
            changed.push("model_quantization");
        }
        if self.embedding_text_version != other.embedding_text_version {
            changed.push("embedding_text_version");
        }
        if self.onnx_runtime != other.onnx_runtime {
            changed.push("onnx_runtime_path");
        }
        changed.join(", ")
    }
}

/// Everything that decides which artifact session [`SearchEngine::open_semantic_artifact`]
/// opens, so a repeat call can be told from a different one: the whole of
/// [`SemanticArtifactInput`], the runtime path included for the reason
/// [`SemanticConfigKey`] gives. The model identity is held parsed, so two spellings of the
/// same JSON are one key.
#[cfg(feature = "semantic-integration")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct SemanticArtifactKey {
    vectors_dir: PathBuf,
    model_path: PathBuf,
    model: ModelIdentity,
    onnx_runtime: Option<PathBuf>,
    scan_threads: Option<u32>,
}

#[cfg(feature = "semantic-integration")]
impl SemanticArtifactKey {
    /// Names the inputs that differ, by their names in [`SemanticArtifactInput`], as
    /// [`SemanticConfigKey::changed_fields`] does for a development session.
    fn changed_fields(&self, other: &Self) -> String {
        [
            ("vectors_dir", self.vectors_dir != other.vectors_dir),
            ("model_path", self.model_path != other.model_path),
            ("model_identity_json", self.model != other.model),
            ("onnx_runtime_path", self.onnx_runtime != other.onnx_runtime),
            ("scan_threads", self.scan_threads != other.scan_threads),
        ]
        .into_iter()
        .filter_map(|(field, differs)| differs.then_some(field))
        .collect::<Vec<_>>()
        .join(", ")
    }
}

/// Refuses an empty `onnx_runtime_path` before anything is opened.
///
/// The sidecar refuses an empty path too, but only once the model loads (for a development
/// session, at the first indexing), and as a runtime it could not find. An empty path is not
/// a missing file but a mistake in the call: `None` is how to pass none.
#[cfg(feature = "semantic-integration")]
fn check_onnx_runtime_path(path: Option<&str>) -> Result<(), SemanticError> {
    if path == Some("") {
        return Err(SemanticError::new(
            SemanticErrorKind::InvalidInput,
            "onnx_runtime_path is empty, so it names no file; pass the path of the ONNX \
             Runtime library the application ships, or None to look for it through \
             OTZARIA_ONNX_RUNTIME and then beside the model",
        )
        .with_field("onnx_runtime_path"));
    }
    Ok(())
}

/// A sidecar failure on the vector set at `vectors_dir`, read as `message`; a cancel reads
/// as `doing` cancelled, which is not a failure and left the set as it was.
#[cfg(feature = "semantic-integration")]
fn vector_set_error(
    error: &SemanticSearchError,
    vectors_dir: &Path,
    message: String,
    doing: &str,
) -> SemanticError {
    match semantic_errors::sidecar_error(error, SidecarCall::VectorSet { vectors_dir }, message) {
        SemanticError {
            kind: SemanticErrorKind::Cancelled,
            ..
        } => SemanticError::new(
            SemanticErrorKind::Cancelled,
            format!("{doing} was cancelled before it finished; the set is as it was"),
        ),
        error => error,
    }
}

/// Whether two paths name one directory, as written or as resolved.
#[cfg(feature = "semantic-integration")]
fn same_directory(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (fs::canonicalize(a), fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// `policy` as the sidecar takes it, refused as input when a threshold is out of range.
#[cfg(feature = "semantic-integration")]
fn compaction_policy(
    policy: SemanticCompactionPolicy,
) -> Result<otzaria_semantic_search::semantic::segment_set::CompactionPolicy, SemanticError> {
    let ratio = |name: &str, value: f64| {
        if value.is_finite() && value >= 0.0 {
            Ok(value)
        } else {
            Err(SemanticError::new(
                SemanticErrorKind::InvalidInput,
                format!("{name} is {value}, and a ratio is a finite number from 0"),
            )
            .with_field(format!("policy.{name}")))
        }
    };
    let min_free_space_factor = policy.min_free_space_factor;
    if !(min_free_space_factor.is_finite() && min_free_space_factor >= 1.0) {
        return Err(SemanticError::new(
            SemanticErrorKind::InvalidInput,
            format!(
                "min_free_space_factor is {min_free_space_factor}, and a compaction needs at \
                 least its output's size free: a finite number from 1"
            ),
        )
        .with_field("policy.min_free_space_factor"));
    }
    Ok(
        otzaria_semantic_search::semantic::segment_set::CompactionPolicy {
            max_delta_ratio: ratio("max_delta_ratio", policy.max_delta_ratio)?,
            max_segments: policy.max_segments,
            max_dead_ratio: ratio("max_dead_ratio", policy.max_dead_ratio)?,
            min_free_space_factor,
            refresh_hints: policy.refresh_hints,
            force: policy.force,
        },
    )
}

/// What [`otzaria_semantic_search::semantic::segment_set::info`] read, as the API says it.
#[cfg(feature = "semantic-integration")]
fn vectors_info(
    info: Option<otzaria_semantic_search::semantic::segment_set::SetInfo>,
) -> SemanticVectorsInfo {
    let Some(info) = info else {
        return SemanticVectorsInfo {
            present: false,
            identity_digest: String::new(),
            model_family_id: String::new(),
            library_version: 0,
            library_release_tag: String::new(),
            generation: 0,
            segments: Vec::new(),
            slots_live: 0,
            slots_dead: 0,
            bytes_on_disk: 0,
            needs_compaction: false,
            recovered_from_previous: false,
        };
    };
    SemanticVectorsInfo {
        present: true,
        identity_digest: info.identity_digest,
        model_family_id: info.identity.model.family_id,
        library_version: info.library_version,
        library_release_tag: info.library_release_tag,
        generation: info.generation,
        segments: info
            .segments
            .into_iter()
            .map(|segment| SemanticSegmentInfo {
                id: segment.id,
                kind: package_kind(segment.kind),
                from_library_version: segment.from_library_version,
                to_library_version: segment.to_library_version,
                slots: segment.slots,
                slots_dead: segment.slots_dead,
                foreign_unresolved: segment.foreign_unresolved,
                size: segment.size,
            })
            .collect(),
        slots_live: info.slots_live,
        slots_dead: info.slots_dead,
        bytes_on_disk: info.bytes_on_disk,
        needs_compaction: info.needs_compaction,
        recovered_from_previous: info.recovered_from_previous,
    }
}

/// `model_identity_json` as the model family it describes, or a refusal of it as input.
#[cfg(feature = "semantic-integration")]
fn parse_model_identity(json: &str) -> Result<ModelIdentity, SemanticError> {
    serde_json::from_str(json).map_err(|err| {
        SemanticError::new(
            SemanticErrorKind::InvalidInput,
            format!(
                "model_identity_json is not a model identity: {err}. It is the JSON the \
                 vectors were built with, such as the sidecar's \
                 config/models/meivin-round2-onnx/model.json"
            ),
        )
        .with_field("model_identity_json")
    })
}

/// Refuses vectors whose chunking this build cannot resolve. Registered Pardes
/// keys are recomputed from live text; Meivin may use the persisted key column.
#[cfg(feature = "semantic-integration")]
fn ensure_keyed_chunking(
    model: &ModelIdentity,
    refused: &dyn Fn(&dyn std::fmt::Display) -> String,
) -> Result<(), SemanticError> {
    if semantic_keys::keyed_chunking(model.chunking_identity).is_some() {
        return Ok(());
    }
    let keyed_under: Vec<String> = semantic_keys::keyed_chunkings()
        .iter()
        .map(|chunking| chunking.identity().to_string())
        .collect();
    Err(SemanticError::new(
        SemanticErrorKind::ArtifactIncompatible,
        refused(&format!(
            "model.chunking_identity is {}, and this build resolves lines under chunking {}, \
             so no line could be resolved",
            model.chunking_identity,
            keyed_under.join(" or ")
        )),
    )
    .with_field("model.chunking_identity"))
}

/// What this installation is, as a release that declares the store `declared` must agree
/// with it: the line recipe this build indexes with, the model family it queries with, and
/// that store when this build reads it — the anchor family's and the windows family's —
/// or else the default one, so a refusal names what differs.
#[cfg(feature = "semantic-integration")]
fn installation_identity(
    model: ModelIdentity,
    declared: &otzaria_semantic_search::semantic::versioning::StoreIdentity,
) -> otzaria_semantic_search::semantic::versioning::IndexVersion {
    otzaria_semantic_search::semantic::versioning::IndexVersion {
        text: TextIdentity::with_line_text_version(LINE_TEXT_VERSION),
        model,
        store: otzaria_semantic_search::semantic::official_index::readable_store_for(declared),
    }
}

/// The sidecar's segment kinds, as the API names them.
#[cfg(feature = "semantic-integration")]
fn package_kind(
    kind: otzaria_semantic_search::distribution::package::PackageKind,
) -> SemanticVectorsPackageKind {
    use otzaria_semantic_search::distribution::package::PackageKind;
    match kind {
        PackageKind::Base => SemanticVectorsPackageKind::Base,
        PackageKind::Delta => SemanticVectorsPackageKind::Delta,
        PackageKind::Compacted => SemanticVectorsPackageKind::Compacted,
    }
}

/// The vector sets an install or a compaction in this process is working on, by their
/// directories as resolved: see [`VectorSetWork`].
#[cfg(feature = "semantic-integration")]
static VECTOR_SET_WORK: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// An install's or a compaction's hold on one vector set in this process, taken before
/// either reads anything and let go when dropped: one of them at a time per set, and a
/// second is refused at once.
///
/// The sidecar locks a set itself, but an install takes that lock only once it has its
/// segment in hand, and a compressed release is expanded before that. Two installs of one
/// set would otherwise both expand, and the second would meet the first at the sidecar's
/// lock only after spending the time and the space. Another process's install still meets
/// this one at the sidecar's lock, which is refused the same way.
#[cfg(feature = "semantic-integration")]
struct VectorSetWork {
    dir: PathBuf,
}

#[cfg(feature = "semantic-integration")]
impl VectorSetWork {
    /// Hold the set at `vectors_dir` for `doing`, or refuse while something else does.
    fn take(vectors_dir: &Path, doing: &str) -> Result<Self, SemanticError> {
        let dir = resolved_dir(vectors_dir);
        let mut work = VECTOR_SET_WORK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if work.contains(&dir) {
            return Err(vector_set_busy(vectors_dir, doing));
        }
        work.push(dir.clone());
        Ok(Self { dir })
    }
}

#[cfg(feature = "semantic-integration")]
impl Drop for VectorSetWork {
    fn drop(&mut self) {
        VECTOR_SET_WORK
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|dir| *dir != self.dir);
    }
}

/// `dir` as one path names it whatever way it is written: resolved when it exists, and
/// otherwise, as before a first install creates it, its parent resolved and its name.
#[cfg(feature = "semantic-integration")]
fn resolved_dir(dir: &Path) -> PathBuf {
    fs::canonicalize(dir).unwrap_or_else(|_| {
        match (
            dir.parent()
                .and_then(|parent| fs::canonicalize(parent).ok()),
            dir.file_name(),
        ) {
            (Some(parent), Some(name)) => parent.join(name),
            _ => std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf()),
        }
    })
}

/// What a scrub of the set at `vectors_dir` reports, as `verify_semantic_vectors` answers it:
/// the verification, or `VectorsBusy` when an install replaced bytes the scrub had read while
/// it read them — nothing was condemned, and what the set holds now was not checked.
#[cfg(feature = "semantic-integration")]
fn verification_of(
    report: &otzaria_semantic_search::semantic::segment_set::ScrubReport,
    vectors_dir: &Path,
) -> Result<SemanticVectorsVerification, SemanticError> {
    if report.superseded {
        return Err(SemanticError::new(
            SemanticErrorKind::VectorsBusy,
            format!(
                "the semantic vectors at {} changed while they were verified: an install \
                 replaced bytes the check had read. Nothing was condemned; verify them again \
                 once the install has finished",
                vectors_dir.display()
            ),
        )
        .with_field("vectors_dir"));
    }
    Ok(SemanticVectorsVerification {
        generation: report.generation,
        segments: report.segments,
        bytes_checked: report.bytes_checked,
        elapsed_ms: report.elapsed_ms,
    })
}

/// What an install or a compaction of the set at `vectors_dir` is refused with while
/// another of either runs: in this process ([`VectorSetWork`]), or in another, which the
/// sidecar's lock on the set says. `VectorsBusy` about `vectors_dir`.
#[cfg(feature = "semantic-integration")]
fn vector_set_busy(vectors_dir: &Path, doing: &str) -> SemanticError {
    SemanticError::new(
        SemanticErrorKind::VectorsBusy,
        format!(
            "{doing} was refused: another install or compaction of the vector set at {} is \
             running, and one finishes before the next starts. Nothing was read or changed; \
             try again once it has finished",
            vectors_dir.display()
        ),
    )
    .with_field("vectors_dir")
}

/// How long an install or a compaction waits for the sidecar's lock on its set.
#[cfg(feature = "semantic-integration")]
const SET_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// `attempt` again while the set's lock is held, for up to `wait`: something other than an
/// install, such as an open's cleanup, can hold it a moment. A cancel ends the wait.
#[cfg(feature = "semantic-integration")]
fn waiting_for_set_lock<T>(
    cancel: &SearchCancellation,
    wait: std::time::Duration,
    mut attempt: impl FnMut() -> Result<T, SemanticSearchError>,
) -> Result<T, SemanticSearchError> {
    use otzaria_semantic_search::errors::ArtifactError;
    let deadline = Instant::now() + wait;
    loop {
        let result = attempt();
        let held = matches!(
            &result,
            Err(SemanticSearchError::Artifact(ArtifactError::Io { source, .. }))
                if source.kind() == std::io::ErrorKind::WouldBlock
        );
        if !held {
            return result;
        }
        if cancel.is_cancelled() {
            return Err(SemanticSearchError::Cancelled);
        }
        if Instant::now() >= deadline {
            return result;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// What an expansion is named in the set's `incoming/` folder: this prefix and a name of its
/// own, with `.oxv` for the segment and [`EXPANSION_LOCK`] for its lock file.
#[cfg(feature = "semantic-integration")]
const EXPANDING_PREFIX: &str = ".expanding-";

/// The suffix of an expansion's lock file, which its install holds locked from before the
/// segment is written until the install returns, whatever the sidecar did with the segment.
#[cfg(feature = "semantic-integration")]
const EXPANSION_LOCK: &str = ".lock";

/// How many lock files an expansion creates, each taken by another process's cleanup in the
/// instant before it was locked, before the install is refused as busy.
#[cfg(feature = "semantic-integration")]
const EXPANSION_LOCK_ATTEMPTS: u32 = 3;

/// How old an expansion's segment with no lock file beside it must be before an install
/// removes it. A segment always has one while its install runs, so one without is debris;
/// the age spares a segment whose lock file another process removed in the instant between
/// its creation and its locking.
#[cfg(feature = "semantic-integration")]
const STRAY_EXPANSION_AGE: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// Remove what expansions that never finished left in `incoming/`. An expansion's lock
/// file that nothing holds locked is a process gone: its segment and the lock file go. One
/// that is locked is an install still running — writing, or waiting for the sidecar to take
/// what it wrote — and its files stay. A segment with no lock file goes once it is old. Best
/// effort.
#[cfg(feature = "semantic-integration")]
fn remove_abandoned_expansions(incoming: &Path) {
    let Ok(entries) = fs::read_dir(incoming) else {
        return;
    };
    let names: Vec<String> = entries
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(EXPANDING_PREFIX))
        .collect();
    let remove = |path: &Path| {
        if let Err(err) = fs::remove_file(path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                debug!(
                    "an abandoned expansion stays for now: {}: {err}",
                    path.display()
                );
            }
        }
    };
    for name in &names {
        let path = incoming.join(name);
        if let Some(stem) = name.strip_suffix(EXPANSION_LOCK) {
            let Ok(lock) = fs::OpenOptions::new().write(true).open(&path) else {
                continue;
            };
            if lock.try_lock().is_ok() {
                remove(&incoming.join(format!("{stem}.oxv")));
                remove(&path);
            }
            continue;
        }
        let stem = name.strip_suffix(".oxv").unwrap_or(name);
        if names.contains(&format!("{stem}{EXPANSION_LOCK}")) {
            continue;
        }
        let old = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| {
                modified
                    .elapsed()
                    .is_ok_and(|age| age > STRAY_EXPANSION_AGE)
            });
        if old {
            remove(&path);
        }
    }
}

/// What a test runs with each expansion's lock file: see [`AFTER_EXPANSION_LOCK_FILE`].
#[cfg(all(test, feature = "semantic-integration"))]
type ExpansionLockHook = Box<dyn FnMut(&Path)>;

#[cfg(all(test, feature = "semantic-integration"))]
thread_local! {
    /// Called with each expansion's lock file as soon as it is created, before it is locked:
    /// what a test does there is what another process's cleanup could do in that instant.
    static AFTER_EXPANSION_LOCK_FILE: std::cell::RefCell<Option<ExpansionLockHook>> =
        const { std::cell::RefCell::new(None) };
}

/// A segment expanded for an install, and the lock that keeps it the install's: dropping
/// this removes the segment and then the lock file, after the install has returned,
/// whatever the sidecar did with the segment.
#[cfg(feature = "semantic-integration")]
struct Expansion {
    segment: tempfile::TempPath,
    /// Locked from before the segment was created, so an install in another process never
    /// takes the segment for abandoned: not while it is written, and not after it is
    /// closed, while the sidecar verifies it and moves it into the set.
    _lock: tempfile::NamedTempFile,
}

/// A zstd-compressed segment expanded into a file of its own in the set's `incoming/`
/// folder, from which an install moves it into the set.
///
/// The name is the expansion's own, never the download's, so two expansions into one set
/// cannot write one file. Its lock file is created and locked first and held until the
/// [`Expansion`] is dropped. Looks at `cancel` between blocks of 1 MiB; `refused` words a
/// failure.
#[cfg(feature = "semantic-integration")]
fn expand_segment(
    compressed: &Path,
    vectors_dir: &Path,
    cancel: &SearchCancellation,
    refused: &dyn Fn(&dyn std::fmt::Display) -> String,
) -> Result<Expansion, SemanticError> {
    use std::io::{Read, Write};
    let incoming = otzaria_semantic_search::semantic::segment_set::incoming_dir(vectors_dir);
    // Writing it is the one step here that a full disk stops.
    let writing = |err: std::io::Error| {
        let kind = match err.kind() {
            std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
                SemanticErrorKind::InsufficientDiskSpace
            }
            _ => SemanticErrorKind::Internal,
        };
        SemanticError::new(
            kind,
            refused(&format!("expanding into {}: {err}", incoming.display())),
        )
    };
    fs::create_dir_all(&incoming).map_err(writing)?;
    remove_abandoned_expansions(&incoming);
    let source = fs::File::open(compressed).map_err(|err| {
        SemanticError::new(
            SemanticErrorKind::Internal,
            refused(&format!("reading {}: {err}", compressed.display())),
        )
    })?;
    let damaged = |err: std::io::Error| {
        SemanticError::new(
            SemanticErrorKind::ArtifactCorrupt,
            refused(&format!(
                "{} does not expand as zstd: {err}",
                compressed.display()
            )),
        )
    };
    let mut decoder = zstd::stream::read::Decoder::new(source).map_err(damaged)?;
    // Another process's cleanup can take a lock file in the instant between its creation and
    // its locking — find it unlocked, take it for a process gone, and remove it. A lock file
    // that cannot be locked, or is gone once it is, is given up for a new one.
    let mut attempts = 0;
    let lock = loop {
        attempts += 1;
        let lock = tempfile::Builder::new()
            .prefix(EXPANDING_PREFIX)
            .suffix(EXPANSION_LOCK)
            .tempfile_in(&incoming)
            .map_err(writing)?;
        #[cfg(test)]
        AFTER_EXPANSION_LOCK_FILE.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook(lock.path());
            }
        });
        let locked = match lock.as_file().try_lock() {
            Ok(()) => lock.path().exists(),
            Err(std::fs::TryLockError::WouldBlock) => false,
            Err(std::fs::TryLockError::Error(err)) => return Err(writing(err)),
        };
        if locked {
            break lock;
        }
        if attempts == EXPANSION_LOCK_ATTEMPTS {
            return Err(SemanticError::new(
                SemanticErrorKind::VectorsBusy,
                refused(&format!(
                    "another install's cleanup of {} took this one's expansion lock {attempts} \
                     times; try again once it has finished",
                    incoming.display()
                )),
            )
            .with_field("vectors_dir"));
        }
    };
    let stem = lock
        .path()
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_suffix(EXPANSION_LOCK))
        .expect("the lock file is named by its builder")
        .to_owned();
    let path = incoming.join(format!("{stem}.oxv"));
    let file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(writing)?;
    let segment = match tempfile::TempPath::try_from_path(&path) {
        Ok(segment) => segment,
        Err(err) => {
            let _ = fs::remove_file(&path);
            return Err(writing(err));
        }
    };
    let expansion = Expansion {
        segment,
        _lock: lock,
    };
    let mut sink = std::io::BufWriter::new(file);
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        if cancel.is_cancelled() {
            return Err(SemanticError::new(
                SemanticErrorKind::Cancelled,
                "installing the vectors was cancelled before it finished; the set is as it was",
            ));
        }
        let read = decoder.read(&mut buffer).map_err(damaged)?;
        if read == 0 {
            break;
        }
        sink.write_all(&buffer[..read]).map_err(writing)?;
    }
    sink.into_inner()
        .map_err(|err| writing(err.into_error()))?
        .sync_all()
        .map_err(writing)?;
    Ok(expansion)
}

/// The package of the family `model_identity_json` declares that the graph at `model_path` is,
/// found by its checksum: the package this installation embeds queries with, and so the one
/// it names to the artifact.
///
/// A graph that is no package of the family is refused before anything loads: the identity
/// file describes other weights. A file that is no model at all is refused as the sidecar
/// would refuse it when it loads, by kind.
#[cfg(feature = "semantic-integration")]
fn family_package(
    key: &SemanticArtifactKey,
    call: SidecarCall<'_>,
    refused: &dyn Fn(&dyn std::fmt::Display) -> String,
) -> Result<ModelPackage, SemanticError> {
    use otzaria_semantic_search::errors::SemanticSearchError;
    use otzaria_semantic_search::semantic::model_package::validate_model;

    let package = validate_model(&key.model_path).map_err(|err| {
        let err = SemanticSearchError::EmbeddingRuntime(err);
        semantic_errors::sidecar_error(&err, call, refused(&err))
    })?;
    key.model
        .query_packages
        .iter()
        .find(|declared| declared.checksum == package.checksum())
        .cloned()
        .ok_or_else(|| {
            let declared = key
                .model
                .query_packages
                .iter()
                .map(|declared| format!("{} {}", declared.quantization, declared.checksum))
                .collect::<Vec<_>>()
                .join(", ");
            SemanticError::new(
                SemanticErrorKind::ModelIdentityMismatch,
                format!(
                    "model_identity_json declares query_packages '{declared}', but the model at \
                     {} is the package '{}', which is none of them: the identity file describes \
                     other weights",
                    key.model_path.display(),
                    package.checksum()
                ),
            )
            .with_field("query_packages")
        })
}

/// `options` as the sidecar's `RankingProfile`, refused by the sidecar's own
/// `RankingProfile::validate` when a value is outside its range.
///
/// Every field is named, with no `..` from a preset, so that a repin that gives the profile
/// another parameter fails to compile here until someone decides what the application passes
/// for it. What is not a ranking parameter is the `Balanced` preset's: the label, which only
/// telemetry reads, and the caches and telemetry, which change no ranking. The values are
/// narrowed to the 32 bits the ranking computes in.
#[cfg(feature = "semantic-integration")]
fn ranking_profile(options: &SemanticRankingOptions) -> Result<RankingProfile, SemanticError> {
    let preset = RankingProfile::from_profile(SearchProfile::Balanced);
    let alphas = &options.alpha_by_query_type;
    let profile = RankingProfile {
        profile: preset.profile,
        fusion_strategy: match options.fusion_strategy {
            SemanticFusionStrategy::Weighted => FusionStrategy::Weighted,
            SemanticFusionStrategy::Rrf => FusionStrategy::RRF { k: options.rrf_k },
            SemanticFusionStrategy::Adaptive => FusionStrategy::Adaptive,
        },
        alpha_override: options.alpha_override.map(|alpha| alpha as f32),
        alpha_by_query_type: QueryTypeAlphas {
            quoted_phrase: alphas.quoted_phrase as f32,
            exact_reference: alphas.exact_reference as f32,
            short: alphas.short as f32,
            mixed: alphas.mixed as f32,
            conceptual: alphas.conceptual as f32,
            unknown: alphas.unknown as f32,
        },
        bm25_saturation_k: options.bm25_saturation_k as f32,
        semantic_threshold: options.semantic_threshold as f32,
        agreement_bonus: options.agreement_bonus as f32,
        phrase_match_bonus: options.phrase_match_bonus as f32,
        rare_term_bonus: options.rare_term_bonus as f32,
        section_coverage_bonus: options.section_coverage_bonus as f32,
        duplicate_penalty: options.duplicate_penalty as f32,
        metadata_ranking_enabled: options.metadata_ranking_enabled,
        candidate_window_multiplier: options.candidate_window_multiplier as f32,
        foundational_bonus: options.foundational_bonus as f32,
        foundational_candidate_share: options.foundational_candidate_share as f32,
        window_penalty: options.window_penalty as f32,
        alias_penalty: options.alias_penalty as f32,
        query_cache_enabled: preset.query_cache_enabled,
        embedding_cache_enabled: preset.embedding_cache_enabled,
        telemetry_enabled: preset.telemetry_enabled,
    };
    profile
        .validate()
        .map_err(|err| semantic_errors::ranking_error(&err))?;
    Ok(profile)
}

/// An installed vector set as it was opened: the inputs it was opened with.
#[cfg(feature = "semantic-integration")]
struct OpenedArtifact {
    key: SemanticArtifactKey,
}

/// Where a session's vectors come from.
#[cfg(feature = "semantic-integration")]
enum SemanticSource {
    /// [`SearchEngine::configure_semantic`]: vectors embedded on this device, which the
    /// application never does for the library — development and testing.
    SelfBuilt(SemanticConfigKey),
    /// [`SearchEngine::open_semantic_artifact`]: vectors built on the build machine, read
    /// only. The application's path.
    Artifact(OpenedArtifact),
}

/// An open sidecar session: the coordinator, and what it was opened from.
#[cfg(feature = "semantic-integration")]
struct ConfiguredSemantic {
    /// The sidecar's `HybridCoordinator` itself, not the `OtzariaHybridEngine` wrapper the
    /// sidecar offers as a bridge: that wrapper turns every error into its message, and one
    /// failure could then be told from another only by reading it. The coordinator returns
    /// the typed [`SemanticSearchError`](otzaria_semantic_search::errors::SemanticSearchError),
    /// and the wrapper adds nothing else — the sidecar leaves the surface the application
    /// sees to this layer. Behind an `Arc` for the reason the wrapper held one: a call
    /// clones the handle and holds no lock while it runs.
    coordinator: Arc<HybridCoordinator>,
    source: SemanticSource,
}

#[cfg(feature = "semantic-integration")]
impl ConfiguredSemantic {
    /// The model this session embeds with, which deciding a failure's kind can need: whether
    /// there is an ONNX Runtime beside it.
    fn model_path(&self) -> &Path {
        match &self.source {
            SemanticSource::SelfBuilt(key) => &key.model_path,
            SemanticSource::Artifact(opened) => &opened.key.model_path,
        }
    }

    /// The ONNX Runtime the session was opened with, the first place the sidecar looks.
    fn onnx_runtime(&self) -> Option<&Path> {
        match &self.source {
            SemanticSource::SelfBuilt(key) => key.onnx_runtime.as_deref(),
            SemanticSource::Artifact(opened) => opened.key.onnx_runtime.as_deref(),
        }
    }
}

/// What a semantic call works with once it has let go of the session lock: a clone of the
/// coordinator handle, and where the session's model and runtime are.
#[cfg(feature = "semantic-integration")]
struct OpenSession {
    coordinator: Arc<HybridCoordinator>,
    model_path: PathBuf,
    onnx_runtime: Option<PathBuf>,
    /// The vector set an opened artifact serves; `None` for a session built on this device.
    vectors_dir: Option<PathBuf>,
    /// The exact recipe whose keys the installed set records.
    chunking: otzaria_semantic_search::semantic::chunker::ChunkerConfig,
}

#[cfg(feature = "semantic-integration")]
impl OpenSession {
    /// The call that a failure of this session's sidecar came from.
    fn call(&self) -> SidecarCall<'_> {
        SidecarCall::Session {
            model_path: &self.model_path,
            onnx_runtime: self.onnx_runtime.as_deref(),
        }
    }
}

/// Behind a lock, not a plain field, so that [`SearchEngine::open_semantic_artifact`] can
/// take `&self`: opening loads the model and the artifact's vectors, and holding the whole
/// engine's write lock for that long would stall every lexical search in the meantime.
#[cfg(feature = "semantic-integration")]
type SemanticRuntime = RwLock<Option<ConfiguredSemantic>>;
#[cfg(not(feature = "semantic-integration"))]
type SemanticRuntime = ();

/// What the lexical half of a sidecar search produces: the scored candidates
/// fusion consumes, the corpus-wide count for the response envelope, and the
/// inputs needed to paint the *final page* the way the lexical API paints its
/// own results.
#[cfg(feature = "semantic-integration")]
struct SemanticLexicalPhase {
    candidates: Vec<SidecarLexicalCandidate>,
    total_count: u32,
    truncated: bool,
    /// `None` in `SemanticOnly`, where no lexical query is executed and every
    /// result therefore gets an unhighlighted (but still bounded) snippet.
    highlight: Option<SemanticHighlight>,
    /// Candidates whose text is not the indexed line, by id (absent = `Ok`).
    text_status: HashMap<u64, TextStatus>,
    /// Display provenance stays beside the sidecar input, keyed by book and line.
    notes: HashMap<LineKey, NoteSpans>,
}

/// Whether the foundational books' query is planned like the search's own: once per index
/// generation and vector set. Off, it misses texts that moved into those books.
#[cfg(feature = "semantic-integration")]
const PLAN_FOUNDATIONAL_QUERY: bool = true;

/// One [`SearchEngine::search_semantic`] call's inputs, after the defaults are applied.
#[cfg(feature = "semantic-integration")]
struct SemanticRequest<'a> {
    query: &'a str,
    facets: &'a [String],
    lexical_mode: SemanticLexicalMode,
    fuzzy_max_distance: u8,
    retrieval_mode: SemanticRetrievalMode,
    grouping: Option<SemanticGroupingMode>,
    match_nikud: bool,
    match_taamim: bool,
    ranking: &'a RankingProfile,
}

#[cfg(feature = "semantic-integration")]
impl SemanticRequest<'_> {
    /// The key of the session this request continues, over `searcher`.
    fn session_key(
        &self,
        searcher: &Searcher,
        vectors_generation: Option<u64>,
        epoch: u64,
        page_size: u32,
    ) -> SessionKey {
        let mut facets = self.facets.to_vec();
        facets.sort();
        facets.dedup();
        // Opened before it is sampled, or the first search's read opening it would change the
        // sample and start a second session.
        // The user-books part of the key does not change when that store opens.
        let _ = crate::line_source::with_store(crate::line_source::Source::Library, |_| Ok(()));
        SessionKey {
            query: self.query.to_string(),
            facets,
            options: format!(
                "{:?} {} {:?} {:?} {} {} {:?} page_size={}",
                self.lexical_mode,
                self.fuzzy_max_distance,
                self.retrieval_mode,
                self.grouping,
                self.match_nikud,
                self.match_taamim,
                self.ranking,
                page_size
            ),
            index_content: crate::semantic_resolver::index_content(searcher),
            library_generation: crate::line_source::query_cache_generation(),
            vectors_generation,
            epoch,
        }
    }
}

/// One result a session holds, in the session's searcher.
#[cfg(feature = "semantic-integration")]
struct SessionEntry {
    item: HybridResultItem,
    /// Where an item the sidecar asked to hydrate is, and its stored fields.
    hydrated: Option<(DocAddress, SearchResult)>,
    /// Where each of `item.merged` was resolved, when it was.
    siblings: Vec<Option<DocAddress>>,
    /// The text status of a lexical candidate's line.
    text_status: TextStatus,
    notes: NoteSpans,
    /// See [`SemanticSearchResult::matched_window`].
    window: u8,
}

/// What a session keeps besides its results: the last expansion's envelope, and how to
/// paint its pages.
#[cfg(feature = "semantic-integration")]
struct SessionState {
    highlight: Option<SemanticHighlight>,
    painter: Option<SemanticSnippetPainter>,
    lexical_total_count: u32,
    truncated: bool,
    total_count: u32,
    group_count: Option<u32>,
    executed_mode: SemanticExecutedMode,
    semantic_available: bool,
    fallback_reason: Option<String>,
    stale_primaries_dropped: u32,
    unverified: u32,
    /// The window a page asked for beyond [`MAX_SEMANTIC_CANDIDATE_WINDOW`], once one did.
    capped_request: Option<u64>,
    /// The library database's state the session is keyed by.
    library_generation: crate::semantic_resolver::LibraryGeneration,
}

#[cfg(feature = "semantic-integration")]
impl Default for SessionState {
    fn default() -> Self {
        Self {
            highlight: None,
            painter: None,
            lexical_total_count: 0,
            truncated: false,
            total_count: 0,
            group_count: None,
            executed_mode: SemanticExecutedMode::Disabled,
            semantic_available: false,
            fallback_reason: None,
            stale_primaries_dropped: 0,
            unverified: 0,
            capped_request: None,
            library_generation: Default::default(),
        }
    }
}

#[cfg(feature = "semantic-integration")]
impl SessionState {
    fn absorb(&mut self, expanded: Expanded) {
        if let Some((count, truncated)) = expanded.lexical {
            self.lexical_total_count = count;
            self.truncated = truncated;
        }
        if expanded.highlight.is_some() {
            self.highlight = expanded.highlight;
        }
        self.total_count = expanded.total_count;
        self.group_count = expanded.group_count;
        self.executed_mode = expanded.executed_mode;
        self.semantic_available = expanded.semantic_available;
        self.fallback_reason = expanded.fallback_reason;
        self.stale_primaries_dropped = self
            .stale_primaries_dropped
            .saturating_add(expanded.stale_primaries_dropped);
        self.unverified = expanded.unverified;
        self.capped_request = expanded.capped_request.or(self.capped_request);
    }
}

/// What one or more expansions of a session found, applied to it once the search is served.
#[cfg(feature = "semantic-integration")]
struct Expanded {
    /// The corpus-wide lexical count and whether it is a floor; `None` when not counted.
    lexical: Option<(u32, bool)>,
    highlight: Option<SemanticHighlight>,
    total_count: u32,
    group_count: Option<u32>,
    executed_mode: SemanticExecutedMode,
    semantic_available: bool,
    fallback_reason: Option<String>,
    stale_primaries_dropped: u32,
    unverified: u32,
    exhausted: bool,
    capped_request: Option<u64>,
    timings: SemanticTimings,
}

/// Where a semantic search's time went, in milliseconds: logged at debug level, and read by
/// the benchmark.
#[cfg(feature = "semantic-integration")]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct SemanticTimings {
    pub expansions: u32,
    /// The lexical phase, on its own thread beside planning and the semantic half.
    pub lexical_ms: f64,
    /// Planning, embedding, scanning and resolving.
    pub semantic_ms: f64,
    /// Of `semantic_ms`, as the sidecar measured them.
    pub embed_ms: f64,
    pub scan_ms: f64,
    pub resolve_ms: f64,
    pub fuse_ms: f64,
    pub hydrate_ms: f64,
    /// Cutting the page, reading its texts and painting it.
    pub page_ms: f64,
    pub total_ms: f64,
}

#[cfg(feature = "semantic-integration")]
impl SemanticTimings {
    fn plus(self, later: SemanticTimings) -> SemanticTimings {
        SemanticTimings {
            expansions: self.expansions + later.expansions,
            lexical_ms: self.lexical_ms + later.lexical_ms,
            semantic_ms: self.semantic_ms + later.semantic_ms,
            embed_ms: self.embed_ms + later.embed_ms,
            scan_ms: self.scan_ms + later.scan_ms,
            resolve_ms: self.resolve_ms + later.resolve_ms,
            fuse_ms: self.fuse_ms + later.fuse_ms,
            hydrate_ms: self.hydrate_ms + later.hydrate_ms,
            page_ms: self.page_ms + later.page_ms,
            total_ms: self.total_ms + later.total_ms,
        }
    }
}

#[cfg(all(test, feature = "semantic-integration"))]
thread_local! {
    /// The timings of the last semantic search this thread ran.
    pub(crate) static LAST_SEMANTIC_TIMINGS: std::cell::Cell<SemanticTimings> =
        std::cell::Cell::new(SemanticTimings::default());
}

#[cfg(feature = "semantic-integration")]
fn elapsed_ms(started: Instant) -> f64 {
    started.elapsed().as_secs_f64() * 1000.0
}

#[cfg(feature = "semantic-integration")]
impl Expanded {
    fn followed_by(self, later: Expanded) -> Expanded {
        Expanded {
            timings: self.timings.plus(later.timings),
            lexical: later.lexical.or(self.lexical),
            highlight: self.highlight.or(later.highlight),
            stale_primaries_dropped: self
                .stale_primaries_dropped
                .saturating_add(later.stale_primaries_dropped),
            capped_request: later.capped_request.or(self.capped_request),
            ..later
        }
    }
}

#[cfg(feature = "semantic-integration")]
type SemanticSearchSession = crate::semantic_session::SemanticSession<SessionEntry, SessionState>;

#[cfg(feature = "semantic-integration")]
type SemanticSessionCache = crate::semantic_session::SemanticSessions<SemanticSearchSession>;

/// What a benchmark reads from one lexical phase.
#[cfg(all(test, feature = "semantic-integration"))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct BenchLexicalPhase {
    pub candidates: usize,
    pub total_count: u32,
    pub truncated: bool,
    pub highlighted: bool,
}

/// A lexical query kept alive past its own execution so snippets can be built
/// after fusion and pagination rather than for the whole candidate window.
#[cfg(feature = "semantic-integration")]
struct SemanticHighlight {
    /// Drives both fragment selection and term painting.
    query: Box<dyn Query>,
    phrase: Option<PhraseHighlight>,
}

/// Paints one page of sidecar results. Built once per page — creating the
/// generator resolves the doc-frequency of every highlight term — and only when
/// there is a page to paint.
#[cfg(feature = "semantic-integration")]
struct SemanticSnippetPainter {
    searcher: Searcher,
    generator: SnippetGenerator,
    phrase: Option<PhraseHighlight>,
    hl: HighlightConfig,
}

#[cfg(feature = "semantic-integration")]
impl SemanticSnippetPainter {
    /// Plain-text test adapter for the embedded `window` a result shows in place of its `line`; the
    /// whole line instead when a lexical match marks nothing in the window.
    #[cfg(test)]
    fn paint_shown(
        &self,
        line: &str,
        window: Option<&str>,
        lexically_confirmed: bool,
    ) -> (String, bool) {
        let line = DisplayText::plain(line.to_string());
        let window = window.map(|text| DisplayText::plain(text.to_string()));
        self.paint_display_shown(&line, window.as_ref(), lexically_confirmed)
    }

    fn paint_display_shown(
        &self,
        line: &DisplayText,
        window: Option<&DisplayText>,
        lexically_confirmed: bool,
    ) -> (String, bool) {
        if let Some(window) = window {
            let painted = self.paint_display(window, lexically_confirmed);
            if painted.1 || !lexically_confirmed {
                return painted;
            }
        }
        self.paint_display(line, lexically_confirmed)
    }

    #[cfg(test)]
    fn paint(&self, text: &str, lexically_confirmed: bool) -> (String, bool) {
        self.paint_display(&DisplayText::plain(text.to_string()), lexically_confirmed)
    }

    /// Returns the display markup for `text` and whether it carries highlights.
    ///
    /// Mirrors [`SearchEngine::build_results_with_generator`]: tantivy's
    /// term-based highlighter picks the fragment, and a phrase plan re-derives
    /// the painting so only complete in-order occurrences stay painted. A
    /// fragment only exists when at least one query term matched, so an empty
    /// result means this line is a purely semantic hit — it then falls back to a
    /// bounded escaped snippet instead of an unbounded raw line.
    ///
    /// `lexically_confirmed` is what makes the phrase fallback honest. When the
    /// filter finds no complete in-order occurrence, the lexical API paints the
    /// individual terms instead — sound there, because Tantivy already proved the
    /// document satisfies the phrase query, so the fragment merely failed to
    /// contain a whole occurrence. A purely semantic candidate never passed that
    /// query: painting its scattered words would assert a phrase match that does
    /// not exist. Such a result is left unpainted instead.
    fn paint_display(&self, text: &DisplayText, lexically_confirmed: bool) -> (String, bool) {
        let mut snippet = self.generator.snippet(text);
        snippet.set_snippet_prefix_postfix(&self.hl.highlight_prefix, &self.hl.highlight_postfix);
        let (lead, tail) = glued_punctuation(text, snippet.fragment_range());
        let fragment = [lead, snippet.fragment(), tail].concat();
        let fragment_start = snippet.fragment_range().start - lead.len();
        let term_html = || {
            (
                fragment_start,
                htmlescape::encode_minimal(lead)
                    + &snippet.to_html()
                    + &htmlescape::encode_minimal(tail),
            )
        };
        let html = match self.phrase.as_ref() {
            Some(phrase) => SearchEngine::phrase_filtered_snippet(
                &self.searcher,
                &fragment,
                phrase,
                &self.hl,
                None,
            )
            .map(|(offset, html)| (fragment_start + offset, html))
            .or_else(|| {
                SearchEngine::phrase_filtered_snippet(
                    &self.searcher,
                    text,
                    phrase,
                    &self.hl,
                    Some(self.hl.max_chars as usize),
                )
            })
            .or_else(|| (lexically_confirmed && phrase.allow_term_fallback).then(term_html)),
            // No phrase constraint: every occurrence of every query word is a
            // real match of that word, whichever retrieval path found the line,
            // so term painting states nothing untrue.
            None => Some(term_html()),
        };
        match html {
            Some((offset, html)) if !html.is_empty() => (
                text.annotate(
                    html,
                    offset,
                    &self.hl.highlight_prefix,
                    &self.hl.highlight_postfix,
                ),
                true,
            ),
            _ => (bounded_display_snippet(text, self.hl.max_chars), false),
        }
    }
}

/// A shown line for `embedded_window_texts`: address, book, id, stored text, matched window.
#[cfg(feature = "semantic-integration")]
type EmbeddedLine<'a> = (DocAddress, &'a str, u64, &'a DisplayText, u8);

/// Window `window` of a long line, spaces collapsed, with `…` where the line goes on before
/// and after it; `None` for the first window and for one the line does not embed.
#[cfg(feature = "semantic-integration")]
fn long_line_window_text(
    text: &DisplayText,
    window: u8,
    chunker: &otzaria_semantic_search::semantic::chunker::Chunker,
) -> Option<DisplayText> {
    let (span, length) = semantic_keys::window_span_with(text, window, chunker)?;
    let trimmed = text.trimmed();
    let start = trimmed
        .char_indices()
        .nth(span.start)
        .map_or(trimmed.len(), |(at, _)| at);
    let end = trimmed
        .char_indices()
        .nth(span.end)
        .map_or(trimmed.len(), |(at, _)| at);
    let piece = trimmed.slice(start..end).collapse_whitespace();
    let ellipsis = DisplayText::plain("…".to_string());
    Some(if span.end < length {
        DisplayText::join(&[&ellipsis, &piece, &ellipsis], "")
    } else {
        DisplayText::join(&[&ellipsis, &piece], "")
    })
}

/// A display-safe stand-in for a snippet: a bounded, HTML-escaped prefix, so a
/// line that cannot be painted still crosses FFI as markup instead of a raw full
/// line.
///
/// `max_chars` bounds the *source* line, in bytes, matching tantivy's own
/// `max_num_chars`; the cut lands on a UTF-8 char boundary so multi-byte Hebrew
/// is never split. Escaping happens after the cut, so the returned string can be
/// longer than the budget when the line is dense in `&` or `<` — the bound is on
/// how much text is shown, not on the byte length of its encoding.
fn bounded_plain_snippet(text: &str, max_chars: u32) -> String {
    let source = bounded_snippet_source(text, max_chars);
    let mut html = htmlescape::encode_minimal(source);
    if source.len() < text.len() {
        html.push('…');
    }
    html
}

fn bounded_snippet_source(text: &str, max_chars: u32) -> &str {
    let budget = max_chars as usize;
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn bounded_display_snippet(text: &DisplayText, max_chars: u32) -> String {
    let source = bounded_snippet_source(text, max_chars);
    let mut html = text.annotate(htmlescape::encode_minimal(source), 0, "", "");
    if source.len() < text.len() {
        html.push('…');
    }
    html
}

/// The punctuation glued before and after the fragment at `range` in `text`, which
/// tantivy drops by cutting at the edge tokens' offsets (`{פ}` → `פ`).
fn glued_punctuation<'a>(text: &'a str, range: std::ops::Range<usize>) -> (&'a str, &'a str) {
    if range.is_empty() {
        return ("", "");
    }
    let (start, end) = (range.start, range.end);
    let before = text[..start]
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("");
    let after = text[end..].split(char::is_whitespace).next().unwrap_or("");
    // A run that reaches another word (`כי־גר`) is a real cut, not glued punctuation.
    let glued = |run: &'a str| {
        if run.contains(char::is_alphanumeric) {
            ""
        } else {
            run
        }
    };
    (glued(before), glued(after))
}

/// Cuts `text` to at most `budget` bytes around its first phrase occurrence —
/// `ranges[..word_count]`, since each occurrence contributes one range per
/// word, in order — keeping whole context words and complete occurrences.
/// Returns `None` if the first occurrence itself cannot fit the budget.
pub(crate) fn crop_around_first_occurrence<'a>(
    text: &'a str,
    ranges: &[(usize, usize)],
    word_count: usize,
    budget: usize,
) -> Option<(&'a str, Vec<(usize, usize)>)> {
    let occ_start = ranges.first()?.0;
    let occ_end = ranges.get(word_count.checked_sub(1)?)?.1;
    let extra = budget.checked_sub(occ_end - occ_start)?;
    let mut start = occ_start.saturating_sub(extra / 2);
    let mut end = (occ_end + extra - (occ_start - start)).min(text.len());
    start = start.saturating_sub(budget.saturating_sub(end - start));
    while start > 0 && !text.is_char_boundary(start) {
        start += 1;
    }
    while end < text.len() && !text.is_char_boundary(end) {
        end -= 1;
    }
    // A UTF-8 boundary can still be inside a context word. If no whitespace
    // separates the cut from the occurrence (e.g. punctuation-adjacent text),
    // discard that side's context instead of returning a word fragment.
    if start > 0 && !text[..start].ends_with(char::is_whitespace) {
        start = text[start..occ_start]
            .char_indices()
            .find(|&(_, c)| c.is_whitespace())
            .map_or(occ_start, |(offset, c)| start + offset + c.len_utf8());
    }
    if end < text.len() && !text[end..].starts_with(char::is_whitespace) {
        end = text[occ_end..end]
            .rfind(char::is_whitespace)
            .map_or(occ_end, |offset| occ_end + offset);
    }
    let cropped = ranges
        .chunks_exact(word_count)
        .filter(|occurrence| occurrence.iter().all(|&(s, e)| s >= start && e <= end))
        .flatten()
        .map(|&(s, e)| (s - start, e - start))
        .collect();
    Some((&text[start..end], cropped))
}

/// טווח הקרבה הנדרש בין מילות שאילתה מרובת-מילים במסלול המתקדם.
pub enum SearchScope {
    /// ההתנהגות הקיימת: המילים מופיעות לפי סדר השאילתה, עם מגבלת
    /// מילים-ביניים לכל זוג סמוך (`distance` / `custom_spacing`).
    WordDistance,
    /// כל המילים באותה פסקה (מסמך אינדקס אחד = שורת ספר), בכל סדר ובכל
    /// מרחק. `distance`/`custom_spacing` אינם רלוונטיים במצב זה.
    SameParagraph,
    /// כל המילים תחת אותה כותרת (אותו בלוק `reference` — סעיף/פרק), גם
    /// כשהן פזורות על פני שורות שונות. התוצאות הן השורות שבתוך סעיף
    /// חותך שמכילות מילה מהשאילתה.
    SameSection,
}

/// כמה ממילות שאילתה מרובת-מילים חייבות להופיע בתוצאה (המסלול המתקדם).
///
/// בכל מצב שאינו [`WordMatchMode::All`] דרישת הסדר והמרחק חסרת משמעות —
/// מילים עשויות להיות חסרות — ולכן ההתאמה נעשית ברזולוציית ה-scope בלבד:
/// `WordDistance` מתנהג כ-`SameParagraph`, ו-`SameSection` דורש שהסעיף
/// יכיל לפחות את מספר המילים הנדרש.
pub enum WordMatchMode {
    /// ההתנהגות הקיימת: כל המילים חובה.
    All,
    /// די במילה אחת ממילות השאילתה.
    AnyWord,
    /// רוב המילים: יותר ממחצית (`n/2 + 1`).
    MostWords,
    /// לפחות `word_match_count` מילים (הפרמטר הנלווה); נחתך ל-`[1, n]`.
    AtLeast,
}

/// הצורה הפנימית של צמד הפרמטרים `word_match_mode`/`word_match_count`.
/// (מופרד מ-[`WordMatchMode`] כי enum נושא-נתונים בגשר היה גורר תלות freezed.)
enum WordMatch {
    All,
    AtLeast(u32),
    Most,
}

impl WordMatch {
    fn from_api(mode: Option<WordMatchMode>, count: Option<u32>) -> Self {
        match mode.unwrap_or(WordMatchMode::All) {
            WordMatchMode::All => WordMatch::All,
            WordMatchMode::AnyWord => WordMatch::AtLeast(1),
            WordMatchMode::MostWords => WordMatch::Most,
            WordMatchMode::AtLeast => WordMatch::AtLeast(count.unwrap_or(1).max(1)),
        }
    }

    /// מספר המילים המינימלי הנדרש עבור שאילתה בת `word_count` מילים.
    fn min_words(&self, word_count: usize) -> usize {
        let min = match self {
            WordMatch::All => word_count,
            WordMatch::AtLeast(count) => *count as usize,
            WordMatch::Most => word_count / 2 + 1,
        };
        min.min(word_count)
    }
}

pub enum ResultsOrder {
    Catalogue,
    Relevance,
    Generation,
}

/// Per-word index-term sets plus their [`Proximity`], used by the snippet
/// renderer to keep only highlights that form a *phrase* occurrence.
///
/// tantivy's `SnippetGenerator` highlights term-by-term: it paints every
/// occurrence of every query term with no positional constraint. For the
/// multi-word phrase paths (exact `PhraseQuery`, advanced/lexical-fuzzy
/// `RegexPhraseQuery`) that over-paints — a lone "משה" lights up even when the
/// search matched only the adjacent phrase "משה ואהרן". This filter drops such
/// stray highlights so the snippet paints exactly what the search matched.
///
/// The proximity is the one `display_highlight` and the engine's phrase
/// verification use (per-pair gaps, or one any-order window), so the results
/// snippet, the search, and the book agree.
struct PhraseHighlight {
    /// One index-term set per query word, in query order.
    per_word_terms: Vec<HashSet<String>>,
    /// Ordered gaps have length `per_word_terms.len() - 1`.
    proximity: Proximity,
    /// The analyzer that tokenizes snippet fragments for re-highlighting —
    /// must match the field the terms came from ([`DISPLAY_ANALYZER`] for `text`,
    /// [`VOCALIZED_DISPLAY_ANALYZER`] for `textVocalized`).
    analyzer: &'static str,
    /// The search also matches the phrase across a line break (see [`cross_line`]): a
    /// hit without an in-line occurrence is shown joined with its next line.
    cross_line: bool,
    /// Complete alternate phrases (e.g. an acronym's canonical expansion).
    /// Each carries its own word count and gaps; no cross-alternative mixing.
    alternatives: Vec<PhraseHighlight>,
    /// Legacy phrase paths may show individual terms when a complete occurrence
    /// cannot fit the snippet. Smart-search quotations never permit that fallback.
    allow_term_fallback: bool,
}

impl PhraseHighlight {
    fn variants(&self) -> impl Iterator<Item = &Self> {
        std::iter::once(self).chain(self.alternatives.iter())
    }
}

/// What a highlight-query builder resolves to: the flat term query that drives
/// tantivy's fragment selection (and, absent a phrase filter, its
/// highlighting), plus the optional phrase constraint above.
type AcronymAlternative = (Box<dyn Query>, bool, Vec<String>);

struct HighlightPlan {
    /// `None` falls back to the main search query, which already exposes its
    /// terms to `SnippetGenerator` when it is a Term/Phrase/TermSet query.
    query: Option<Box<dyn Query>>,
    phrase: Option<PhraseHighlight>,
}

impl HighlightPlan {
    /// No highlight query and no phrase filter — the snippet generator falls
    /// back to the main query's own terms (single-word/plain paths).
    fn none() -> Self {
        HighlightPlan {
            query: None,
            phrase: None,
        }
    }
}

/// What [`SearchEngine::build_advanced_query`] returns: the executable query,
/// one joined regex pattern per word (for highlight-term materialization), the
/// resolved proximity (folds `custom_spacing`/`distance`/any-order in — the
/// phrase highlight filter's allowances), and whether single-word
/// collection truncated. The fifth element carries the acronym-expansion
/// alternatives ("ראשי תיבות") as per-word literal-pattern lists — the
/// highlight builders materialize their terms so a document matched through
/// an expansion (רמב"ם → "רבי משה בן מיימון") still gets its snippet painted.
type AdvancedQueryBuild = (
    Box<dyn Query>,
    Vec<String>,
    Proximity,
    bool,
    Vec<Vec<String>>,
);

/// Prepared highlighting for displayed book text, including HTML and nikud.
/// Use `matcher.find_matches` for phrases or `matcher.find_word_matches` for
/// independent words. The string fields are legacy compatibility metadata.
pub struct HighlightPattern {
    /// Deprecated for phrases: a single-word regex or the safe never-match
    /// sentinel `(?!)` for multiple words. Use `matcher` for real matching.
    pub combined_pattern: String,
    /// Per-word regex, used to locate each word inside a combined match.
    pub word_patterns: Vec<String>,
    /// Per-word: `true` when the word has no morphological expansion option,
    /// so the UI may require token boundaries around its match.
    pub word_boundary_eligible: Vec<bool>,
    /// Prepared bounded matcher. Always populated by native generators;
    /// optional only for compatibility with manually constructed Dart values.
    pub matcher: Option<HighlightMatcher>,
}

/// A range measured in Dart UTF-16 code units.
pub struct HighlightRange {
    pub start: u32,
    pub end: u32,
}

pub struct HighlightMatch {
    pub start: u32,
    pub end: u32,
    /// Word ranges relative to `start`.
    pub word_ranges: Vec<HighlightRange>,
}

/// Compiles each query word once. Matching is linear in paragraph token count
/// times query word count, including near misses and arbitrary phrase gaps.
#[frb(opaque)]
pub struct HighlightMatcher {
    plan: Arc<display_highlight_matcher::PreparedHighlightMatcher>,
}

impl HighlightMatcher {
    /// Whether a display caller may join adjacent lines to complete this pattern.
    #[frb(sync)]
    pub fn allows_cross_line(&self) -> bool {
        self.plan.allows_cross_line()
    }

    #[frb(sync)]
    pub fn find_matches(
        &self,
        data: String,
        require_token_boundaries: Vec<bool>,
    ) -> Vec<HighlightMatch> {
        convert_highlight_matches(self.plan.find_matches(&data, &require_token_boundaries))
    }

    #[frb(sync)]
    pub fn find_word_matches(
        &self,
        data: String,
        require_token_boundaries: Vec<bool>,
    ) -> Vec<HighlightMatch> {
        convert_highlight_matches(
            self.plan
                .find_word_matches(&data, &require_token_boundaries),
        )
    }
}

impl From<display_highlight::DisplayHighlight> for DisplayPattern {
    fn from(hl: display_highlight::DisplayHighlight) -> Self {
        Self {
            combined_pattern: hl.combined_pattern,
            word_patterns: hl.word_patterns,
            word_boundary_eligible: hl.word_boundary_eligible,
            matcher: Arc::new(hl.matcher),
        }
    }
}

impl DisplayPattern {
    fn to_api(&self) -> HighlightPattern {
        HighlightPattern {
            combined_pattern: self.combined_pattern.clone(),
            word_patterns: self.word_patterns.clone(),
            word_boundary_eligible: self.word_boundary_eligible.clone(),
            matcher: Some(HighlightMatcher {
                plan: Arc::clone(&self.matcher),
            }),
        }
    }
}

fn convert_highlight_matches(
    matches: Vec<display_highlight_matcher::Match>,
) -> Vec<HighlightMatch> {
    matches
        .into_iter()
        .filter_map(|m| {
            Some(HighlightMatch {
                start: m.start.try_into().ok()?,
                end: m.end.try_into().ok()?,
                word_ranges: m
                    .word_ranges
                    .into_iter()
                    .map(|r| {
                        Some(HighlightRange {
                            start: r.start.try_into().ok()?,
                            end: r.end.try_into().ok()?,
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            })
        })
        .collect()
}

// ── SearchEngine ───────────────────────────────────────────────────────────────

// `Index::writer(budget)` splits the budget across indexing threads and
// requires ≥15MB per thread, so the budget effectively picks the thread
// count: 50MB caps tantivy at 3 threads (≈16MB arenas — frequent flushes,
// many small segments), while 300MB lets it use all 8 (37.5MB arenas). The
// budget is only consumed while indexing is active; mobile keeps the small
// footprint.
#[cfg(any(target_os = "android", target_os = "ios"))]
const DEFAULT_WRITER_HEAP_SIZE: usize = 50_000_000;
#[cfg(not(any(target_os = "android", target_os = "ios")))]
const DEFAULT_WRITER_HEAP_SIZE: usize = 300_000_000;

/// Threads that collect one search's segments in parallel: half the cores, to
/// leave room for indexing and the UI; phones keep searching on one thread.
fn search_executor_threads() -> usize {
    if cfg!(any(target_os = "android", target_os = "ios")) {
        return 1;
    }
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
    (cores / 2).clamp(1, 8)
}

/// Gives `index` the search thread pool of [`search_executor_threads`]. Must run
/// before a reader is built: a searcher keeps the executor of its index.
fn set_search_executor(index: &mut Index) {
    let threads = search_executor_threads();
    if threads > 1 {
        if let Err(err) = index.set_multithread_executor(threads) {
            warn!("search stays single-threaded: {err}");
        }
    }
}

// Economy indexing uses one writer thread and the mobile heap budget.
const ECONOMY_WRITER_HEAP_SIZE: usize = 50_000_000;
/// Target number of searchable segments after `optimize`. This is deliberately
/// soft: a tiny new segment must not force a rewrite of an unrelated large one.
const MAX_SEGMENTS_AFTER_OPTIMIZE: usize = 8;
/// Do not merge a healthy segment with one less than a quarter of its size.
/// This bounds the write amplification of incremental indexing.
const MAX_COMPACT_SIZE_RATIO: u64 = 4;
/// A segment whose deleted-doc share exceeds this is compacted regardless of size.
const OPTIMIZE_COMPACT_DELETE_RATIO: f64 = 0.3;
const INDEX_METADATA_FILE_NAME: &str = "otzaria_index_meta.json";
const INDEX_FORMAT: &str = "otzaria-search-index";
// גרסה 3 (טרם פורסמה): המעבר ל-HebrewTokenizer (גרשיים/גרש נשמרים
// בטוקנים) משנה את מילון הטרמים, הוסר ה-fast field מ-`text` (עותק columnar
// של כל הקורפוס שאיש לא קרא), ונוסף השדה `textVocalized` (אינדקס + אחסון
// של שורות מנוקדות, טוקנייזר ששומר ניקוד/טעמים) עבור חיפוש מנוקד —
// הסכימה בדיסק שונה מגרסה 2, אינדקסים ישנים חייבים בנייה מחדש.
//
// שים לב: נוסף השדה `sectionId` (FAST) עבור חיפוש "תחת אותה כותרת" —
// שינוי סכימה שמחייב העלאת גרסה לפני פרסום (בדיקת התאימות משווה גם את
// סכימת ה-tantivy בפועל, כך שאינדקס ישן ידווח rebuild_required גם בלעדיה).
//
// שים לב: הטמעת הטוקן-התאום נטול-הגרשיים (emit_quote_free בטוקנייזרים)
// משנה את *תוכן* מילון הטרמים בלי לשנות את סכימת ה-tantivy — בדיקת
// ההתאימות לא תתפוס זאת מעצמה, ולכן חובה להעלות את הגרסה לפני פרסום
// כדי שאינדקסים ישנים ייבנו מחדש (בלעדיהם חיפוש `רמבם` לא ימצא `רמב"ם`
// ו"התעלם מגרשיים" לא יעבוד על ספרים שאונדקסו קודם).
//
// שים לב: נוספו השדה `lineHash` (FAST, חתימת דה-דופ לשורה) ו-facets
// ממדיים (author/era/base על שדה topics) — שינויי סכימה/תוכן שמחייבים
// העלאת גרסה לפני פרסום (את lineHash בדיקת ההתאימות תתפוס גם לבדה;
// את ה-facets הממדיים — לא, כמו הטוקן נטול-הגרשיים לעיל).
//
// v4: נוסף השדה `textHash` (FAST) — חתימת טקסט-בלבד לצד החתימה הקנונית,
// כדי שאימות דריפט תוכן לא ייפסל משינויי metadata (סדר קטלוגי וכו').
//
// v5: נוסף השדה `chunkKey` (FAST) — מפתח הטקסט שהשורה מוטמעת בו (ראו
// semantic_keys). `text`/`textVocalized` are no longer stored: the display copies are
// `textStored`/`textVocalizedStored`, written only for `TextStorage::InIndex` documents,
// and official books read their text from the library database, verified against the
// new `lineCheck` (FAST). An index of version 4, or of a version 5 build without these
// fields, stores its text where this build does not read it: the compatibility check
// compares the whole Tantivy schema, so it is `rebuild_required`.
//
// A change to `normalize_text_for_indexing` or to the row preparation
// (`line_source::prepare_row`) needs a bump: that is what keeps `TextStatus::Ok` true,
// the promise that the displayed text is exactly what was indexed.
//
// v6: disk savings. `id` and `filePath` carry no fieldnorms; `id` is not indexed; the
// doc store keeps `reference`, `id` and the stored texts (see `doc_store`); the book
// fingerprints sit on one document of a book; `sectionId`/`generationSort` are derived
// (see `derived_columns`); and the fields no longer index a quote-free twin beside a
// quoted word (see `quote_expansion`). A version 5 index is rebuilt.
//
// v10 keeps v7's fields, with canonical marks, no CGJ and contextual sup/sub boundaries.
// One Hebrew hanging letter remains inside its word across transparent formatting
// and zero-width controls, without treating a stray mark as a preceding letter.
// v11 adds only stored note provenance. Version 10's normalization and chunk-key
// recipe are unchanged, and its indexes stay readable and writable in place.
pub(crate) const INDEX_SCHEMA_VERSION: u32 = 11;
/// The oldest schema version this engine opens, searches and writes to as it was built.
/// Every version from it to [`INDEX_SCHEMA_VERSION`] is one this engine reads, each with
/// the one schema [`schema_of_version`] gives it; an older one is `rebuild_required`.
const MIN_READABLE_SCHEMA_VERSION: u32 = 10;
/// The column that holds each line's chunk key: see [`crate::semantic_keys`].
const CHUNK_KEY_FIELD: &str = "chunkKey";
const TANTIVY_INDEX_VERSION: &str = "0.26.2";

/// Version of the line recipe: how [`SearchEngine::add_text_book`] turns a book's text into
/// the documents the index stores, which is the text every semantic chunk key is computed
/// from. Version 1: the text is split on `\n`; every line becomes one document, a blank one
/// included, whose `text` is [`normalize_text_for_indexing`] of it; and a line that starts
/// with `<h` opens a new section and belongs to it. Version 2: sup/sub tags separate words
/// ([`crate::hebrew_query::tag_breaks_word`]).
///
/// Declared to the semantic sidecar as `text.line_text_version`, the half of a vector set's
/// identity that only this crate can know. A change to any of the four — another split, a
/// normalization that keeps or drops something else, another rule for sections — changes
/// the text a line's key is computed from, so it is a new version, even where the schema
/// does not change.
/// Version 3: transparent formatting around or inside a hanging letter is ignored.
/// Version 4: that context also ignores bidi/zero-width controls, CGJ and varika;
/// attached marks never stand in for a missing preceding base letter.
pub(crate) const LINE_TEXT_VERSION: u32 = 4;

/// תקרת אורך טוקן (בבייטים של UTF-8) לכל האנליזטורים — אינדוקס ושאילתה
/// כאחד. 128 בייט ≈ 64 אותיות עבריות: פי כמה מכל מילה לגיטימית (כולל
/// גרשיים וניקוד), ומתחת לכל ריצת-זבל אמיתית (base64 שזלג לקורפוס).
pub(crate) const MAX_TOKEN_BYTES: usize = 128;

/// Upper bound on distinct dictionary terms collected for highlighting an
/// advanced (regex) query. Bounds work when a pattern (e.g. partial match)
/// expands very widely; far more matches than a snippet could ever show.
/// Scaled ×4 with the search-side expansion ceilings (parity: a document
/// found via a wide expansion should still highlight its variant); the
/// display char budget bounds the final pattern size regardless.
const MAX_HIGHLIGHT_TERMS: usize = 2_048;
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
/// Relevance tiers of an advanced single-word search ranked by relevance:
/// the typed form, then affixed forms, then spelling variants, then the rest
/// (see `hebrew_query::match_tier`). Only the order matters; a document's
/// clauses sum, so each tier outweighs every lower one combined.
const ADVANCED_TIER_BOOSTS: [Score; 4] = [1000.0, 100.0, 10.0, 1.0];
const FUZZY_BOOST_EXACT_REL: Score = 1.0;
const FUZZY_BOOST_LEXICAL: Score = 30.0;
const FUZZY_BOOST_FUZZY: Score = 1.0;
// Multi-word fuzzy requires every word anywhere; adjacency only ranks. Above
// two exact-word floors so an exact phrase outranks the same words scattered.
const FUZZY_BOOST_PHRASE: Score = 2000.0;
// Adjacency of expanded forms (`RegexPhraseQuery`) costs 0.5-0.9 s on the real
// library, so it stays off; only the exact-token phrase boosts.
const LEXICAL_FUZZY_EXPANDED_PHRASE_BOOST: bool = false;

/// The schema fields resolved together by [`SearchEngine::all_fields`]:
/// `(title, reference, text, id, segment, isPdf, filePath, topics,
/// textVocalized, sectionOffset, lineHash)`. The stamps are [`StampFields`].
type SchemaFields = (
    Field,
    Field,
    Field,
    Field,
    Field,
    Field,
    Field,
    Field,
    Field,
    Field,
    Field,
);

/// The stamp columns of a book: the values it was written with, on one of its documents.
#[derive(Clone, Copy)]
struct StampFields {
    content: Field,
    text: Field,
    metadata: Field,
    source: Field,
}

impl StampFields {
    fn of(schema: &Schema) -> Result<Self> {
        Ok(Self {
            content: schema.get_field("contentHash")?,
            text: schema.get_field("textHash")?,
            metadata: schema.get_field(METADATA_HASH_FIELD)?,
            source: schema.get_field(SOURCE_HASH_FIELD)?,
        })
    }
}

/// A book's stamps as written: 0 is "none".
#[frb(ignore)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Stamps {
    content: u64,
    text: u64,
    metadata: u64,
    source: u64,
}

/// Records a book's stamps on one of its documents, the columns' only value for the book: a
/// value repeated on every line cost 64 bits a line once blocks of the column straddle two
/// books. 0, "none", is not written.
fn stamp_book(document: &mut TantivyDocument, fields: StampFields, stamps: Stamps) {
    for (field, value) in [
        (fields.content, stamps.content),
        (fields.text, stamps.text),
        (fields.metadata, stamps.metadata),
        (fields.source, stamps.source),
    ] {
        if value != 0 {
            document.add_u64(field, value);
        }
    }
}

/// Records `key` in the `chunkKey` column, when the index has one this build writes: `0`
/// for a line with no vector, a line's [`ChunkKey::column_value`] for one with a vector.
///
/// [`ChunkKey::column_value`]: otzaria_semantic_search::semantic::chunk_key::ChunkKey::column_value
fn set_chunk_key(document: &mut TantivyDocument, field: Option<Field>, key: u64) {
    if let Some(field) = field {
        document.add_u64(field, key);
    }
}

#[derive(Serialize, Deserialize)]
struct IndexMetadata {
    format: String,
    schema_version: u32,
    engine_version: String,
    tantivy_version: String,
    created_at_unix_seconds: u64,
    /// The recipe the index's `chunkKey` column was written under, as [`ChunkKeyRecipe`]'s
    /// three fields: recorded when an index with the column is created, and absent from an
    /// index without one, or one whose recipe is not known.
    ///
    /// Read leniently: a value that does not parse is a recipe not known, which makes the
    /// column count as absent, and never makes the metadata unreadable. A column can always
    /// be recomputed from the text, so nothing in it is a reason to rebuild an index.
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    line_text_version: Option<u32>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    chunk_key_version: Option<u32>,
    #[serde(
        default,
        deserialize_with = "lenient",
        skip_serializing_if = "Option::is_none"
    )]
    chunk_key_chunking_identity: Option<u64>,
}

impl IndexMetadata {
    /// The recipe the `chunkKey` column was written under, when all of it is recorded.
    fn chunk_key_recipe(&self) -> Option<ChunkKeyRecipe> {
        Some(ChunkKeyRecipe {
            line_text_version: self.line_text_version?,
            key_version: self.chunk_key_version?,
            chunking_identity: self.chunk_key_chunking_identity?,
        })
    }
}

/// A value, or `None` when it is not one: never an error, so one field cannot make the
/// rest of a document unreadable.
fn lenient<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = JsonValue::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

/// Deliberately **not** `#[frb(sync)]` — this reads the index metadata file, and a
/// synchronous binding blocks the calling Dart isolate on disk I/O.
pub fn check_index_compatibility(path: String) -> IndexCompatibility {
    check_index_compatibility_path(Path::new(&path))
}

/// Points the line source at the library database (`seforim.db`) that
/// [`TextStorage::LibraryDb`] documents read their text from. Nothing is opened
/// here — the first result window (or `LibraryDb` indexing) opens it, read-only,
/// by its plain path (UNC paths work). A different path discards the open
/// connection and every cache.
pub fn configure_line_source(db_path: String) -> Result<()> {
    crate::line_source::configure(&db_path)
}

/// Closes the library connection and invalidates row caches and ongoing scans even
/// when the path is unchanged. After the active windows/scans finish, the next read
/// opens the current file. Hosts call this after replacing a database at the same
/// path; cache generations never identify text from the replaced connection.
///
/// As with `configure_line_source`, this configuration is process-wide: a host must
/// serialize switching to an unrelated library with any old snapshot's requests.
#[frb(ignore)]
pub fn refresh_line_source(db_path: String) -> Result<()> {
    crate::line_source::refresh(&db_path)
}

/// Whether results of a search on the mark-free text show the line with its nikud and
/// cantillation (highlights unchanged), for lines that have them. Off by default; applies
/// to every engine of the process. Vocalized searches always show them.
#[frb(sync)]
pub fn set_vocalized_snippets(enabled: bool) {
    crate::vocalized_snippet::set_enabled(enabled);
}

/// Closes the library database so it can be renamed or replaced, waiting for a
/// window that is reading it (milliseconds). Returns once the file handle is
/// released. Calls nest; while suspended, `LibraryDb` results come back
/// [`TextStatus::Unavailable`].
pub fn suspend_line_source() -> Result<()> {
    crate::line_source::suspend();
    Ok(())
}

/// Undoes one [`suspend_line_source`]. The last one discards every cache, since
/// the file may have been replaced; the next window reopens it.
pub fn resume_line_source() -> Result<()> {
    crate::line_source::resume()
}

/// Suspends for a Dart owner port. Duplicate holds are ignored; a closed port is
/// released automatically before the next source access, including isolate exit.
pub fn suspend_line_source_owned(owner_port: i64) -> Result<()> {
    crate::line_source::suspend_owned(owner_port)
}

/// Releases only this owner's hold. Safe to repeat or call after its port closed.
pub fn resume_line_source_owned(owner_port: i64) -> Result<()> {
    crate::line_source::resume_owned(owner_port);
    Ok(())
}

/// Points the second line source at the personal-books database (`user_books.db`)
/// that `uid:<bookId>` [`TextStorage::LibraryDb`] documents read their text from;
/// `None` closes it (waiting for a window that is reading it) and unconfigures it.
/// Opened lazily and read-only like the library database. The app may keep writing
/// it (WAL): a commit is seen by the next window, and a row that no longer matches
/// its line comes back [`TextStatus::Stale`]. [`suspend_line_source`] does not
/// apply to it.
pub fn configure_user_books_line_source(db_path: Option<String>) -> Result<()> {
    crate::line_source::configure_user_books(db_path.as_deref())
}

/// Points the line source of the attached database `slug` at its file, which
/// `db:<slug>:<bookId>` [`TextStorage::LibraryDb`] documents read their text from; `None`
/// closes it (waiting for a window or scan that is reading it) and unconfigures it, after
/// which its documents come back [`TextStatus::Unavailable`]. Opened lazily, read-only and
/// hardened like the app's untrusted connections; `immutable` opens it with
/// `immutable=1` (the app's `AttachedLibrary.immutable`), and SQLite then never notices a
/// change to the file. The open file cannot be replaced or deleted on Windows: configure
/// `None` before the app updates, replaces or removes the file, and the path again after.
/// Any number of slugs may be configured at once. [`suspend_line_source`] does not apply.
pub fn configure_attached_line_source(
    slug: String,
    db_path: Option<String>,
    immutable: bool,
) -> Result<()> {
    crate::line_source::configure_attached(&slug, db_path.as_deref(), immutable)
}

/// State of the attached database `slug`'s line source.
pub struct AttachedLineSourceStatus {
    /// A database path is configured ([`configure_attached_line_source`]).
    pub configured: bool,
    /// A connection is currently open.
    pub open: bool,
    /// Bumped whenever cached knowledge of the database is discarded.
    pub generation: u64,
    /// Books of this database asked for as [`TextStorage::LibraryDb`] that were stored
    /// [`TextStorage::InIndex`] instead. Only grows within a process.
    pub library_fallbacks: u64,
}

pub fn attached_line_source_status(slug: String) -> AttachedLineSourceStatus {
    match crate::line_source::attached_status(&slug) {
        Some(status) => AttachedLineSourceStatus {
            configured: status.configured,
            open: status.open,
            generation: status.generation,
            library_fallbacks: status.library_fallbacks,
        },
        None => AttachedLineSourceStatus {
            configured: false,
            open: false,
            generation: 0,
            library_fallbacks: 0,
        },
    }
}

pub fn line_source_status() -> LineSourceStatus {
    use crate::line_source::Source;
    let status = crate::line_source::status(Source::Library);
    let user_books = crate::line_source::status(Source::UserBooks);
    LineSourceStatus {
        configured: status.configured,
        open: status.open,
        suspend_depth: status.suspend_depth,
        host_api_ready: crate::sqlite_host::is_ready(),
        generation: status.generation,
        library_fallbacks: status.library_fallbacks,
        user_books_configured: user_books.configured,
        user_books_open: user_books.open,
        user_books_generation: user_books.generation,
        user_books_fallbacks: user_books.library_fallbacks,
    }
}

/// The entry point to hand to `sqlite3_auto_extension` (as a function pointer)
/// before the engine uses SQLite, followed by opening any connection; 0 when
/// this build bundles its own SQLite and needs nothing from the host. It stays
/// registered and costs nothing after the first open; the app may
/// `sqlite3_cancel_auto_extension` it once that open has returned.
#[frb(sync)]
pub fn sqlite_host_entry_address() -> usize {
    crate::sqlite_host::entry_address()
}

/// Builds display-highlight regex patterns for a search query, so the app can
/// mark matches inside an opened book exactly the way the engine matched them.
///
/// Pure string computation (no index access) — safe to call synchronously.
/// Parameters mirror [`SearchEngine::search_advanced`]: `distance` is the
/// default intermediate-word allowance, `custom_spacing` is keyed
/// `"i-(i+1)"`, `alternative_words` is keyed by word position, and
/// `search_options` is keyed `"{word}_{index}"` using the same tokenization
/// as engine queries. `any_order` (default false) matches the words in any
/// order within one window, like the advanced search's flag.
///
/// Returns `None` when the query contains no highlightable words.
#[frb(sync)]
pub fn generate_highlight_pattern(
    query: String,
    distance: u32,
    custom_spacing: HashMap<String, String>,
    alternative_words: HashMap<u32, Vec<String>>,
    search_options: HashMap<String, HashMap<String, bool>>,
    any_order: Option<bool>,
) -> Option<HighlightPattern> {
    display_highlight::build_display_highlight(
        &query,
        distance,
        &custom_spacing,
        &alternative_words,
        &search_options,
        any_order.unwrap_or(false),
    )
    .map(|hl| HighlightPattern {
        combined_pattern: hl.combined_pattern,
        word_patterns: hl.word_patterns,
        word_boundary_eligible: hl.word_boundary_eligible,
        matcher: Some(HighlightMatcher {
            plan: Arc::new(hl.matcher),
        }),
    })
}

/// Builds the regex for highlighting *literal* in-book search matches (the
/// simple/exact mode that scans an open book locally): the phrase as typed,
/// whitespace-joined, nikud-tolerant, geresh/gershayim matching both ASCII and
/// Hebrew forms, with word-boundary lookarounds. The Dart side compiles the
/// returned string with `RegExp(pattern, caseSensitive: false, unicode: true)`
/// and performs no pattern construction of its own.
///
/// Pure string computation — safe to call synchronously. Returns `None` for a
/// whitespace-only query.
#[frb(sync)]
pub fn generate_literal_highlight_pattern(query: String) -> Option<String> {
    display_highlight::build_literal_pattern(&query)
}

/// A smart display fallback with literal word alternatives and quotation groups.
/// Pure string preparation: no index, dictionary, model or I/O is accessed.
#[frb(sync)]
pub fn generate_smart_highlight_pattern(
    query: String,
    lexical_mode: SemanticLexicalMode,
) -> Option<HighlightPattern> {
    let exact = matches!(lexical_mode, SemanticLexicalMode::Exact)
        || smart_query::whole_query_quoted(&query);
    smart_query::display_pattern(&query, exact, |words| Ok(words.to_vec()))
        .expect("literal smart display preparation has no fallible term lookup")
        .map(|pattern| pattern.to_api())
}

/// Normalises a search query exactly like the engine does internally, so the
/// app can build option keys / UI state from the same tokens the engine sees.
///
/// `״→"`, `׳→'`, `־`/`-`→space; strips `,;!?:*()[]{}^$|\+.~\``; collapses
/// whitespace; trims. Pure string computation — safe to call synchronously.
/// This is the single source of truth; the Dart `SearchQueryBuilder.sanitizeQuery`
/// delegates here so index-time and query-time normalisation cannot drift apart.
#[frb(sync)]
pub fn sanitize_query(query: String) -> String {
    hebrew_query::sanitize_query(&query)
}

/// Splits a query into word tokens the same way the engine tokenizes the
/// indexed `text` field (see [`generate_highlight_pattern`] for the key format
/// that consumes these). A `"` or `'` *between* word characters stays inside
/// the token (`רמב"ם`, `ד'אש`), and a trailing `'` is absorbed (`תוס'`); a
/// quote at a word edge separates. See [`hebrew_query::split_query_words`] for
/// the exact rules.
///
/// Pure string computation — safe to call synchronously. Single source of
/// truth for `SearchQueryBuilder.splitQueryWords`.
#[frb(sync)]
pub fn split_query_words(query: String) -> Vec<String> {
    hebrew_query::split_query_words(&query)
}

/// One engine query word with its original range in the user's query. Offsets
/// are UTF-16 code units, matching Flutter's `TextSelection` offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryWordSpan {
    pub word: String,
    pub start: u32,
    pub end: u32,
}

/// Maps the complete raw query to engine words and exact source selections.
/// A paired `(X) [Y]` contributes only Y, even across whitespace; the returned
/// word order is identical to `split_query_words` and keys per-word options.
/// Pure string computation — safe to call synchronously.
#[frb(sync)]
pub fn query_word_spans(query: String) -> Vec<QueryWordSpan> {
    hebrew_query::query_word_spans(&query)
        .into_iter()
        .map(|(word, start, end)| QueryWordSpan { word, start, end })
        .collect()
}

/// Normalises a text-book line for indexing exactly the way the engine expects
/// stored text to look: strip HTML, decompose presentation forms and strip
/// nikud/cantillation — keeping punctuation, which search results display.
/// Single source of truth for the Dart
/// `IndexingDocumentBuilder.normalizeTextForIndexing`.
///
/// Pure string computation — safe to call synchronously (including from the
/// indexing isolate).
#[frb(sync)]
pub fn normalize_text_for_indexing(input: String) -> String {
    hebrew_query::normalize_text_for_indexing(&input)
}

/// Like [`normalize_text_for_indexing`] but for PDF page text: also drops bidi
/// and zero-width invisibles and collapses whitespace first. Single source of
/// truth for the Dart `IndexingDocumentBuilder.normalizePdfTextForIndexing`.
#[frb(sync)]
pub fn normalize_pdf_text_for_indexing(input: String) -> String {
    hebrew_query::normalize_pdf_text_for_indexing(&input)
}

/// Batch form of [`normalize_text_for_indexing`]: one FFI round-trip per line
/// *batch* instead of one per line. The per-call bridge overhead (string
/// encode/decode + call dispatch) dominates the normalisation itself for
/// short lines, and a full library is millions of lines — the indexing
/// isolate should always prefer this over the single-line form.
#[frb(sync)]
pub fn normalize_texts_for_indexing(inputs: Vec<String>) -> Vec<String> {
    inputs
        .iter()
        .map(|s| hebrew_query::normalize_text_for_indexing(s))
        .collect()
}

/// A PDF line prepared for indexing: the normalised text together with its
/// garbage verdict, so the batch API answers both questions the indexing
/// isolate asks per line in one round-trip.
pub struct PdfIndexLine {
    pub text: String,
    pub is_garbage: bool,
}

/// Batch form of [`normalize_pdf_text_for_indexing`] +
/// [`is_probably_garbage_pdf_text`]: normalises each line and evaluates the
/// garbage heuristic on the result — replacing the two-FFI-calls-per-line
/// pattern with one call per batch.
#[frb(sync)]
pub fn normalize_pdf_texts_for_indexing(inputs: Vec<String>) -> Vec<PdfIndexLine> {
    inputs
        .iter()
        .map(|s| {
            let text = hebrew_query::normalize_pdf_text_for_indexing(s);
            let is_garbage = hebrew_query::is_probably_garbage_pdf_text(&text);
            PdfIndexLine { text, is_garbage }
        })
        .collect()
}

/// Whether a normalised PDF page looks like garbage (OCR noise) and should be
/// skipped. Single source of truth for the Dart
/// `IndexingDocumentBuilder.isProbablyGarbagePdfText`.
#[frb(sync)]
pub fn is_probably_garbage_pdf_text(normalized_text: String) -> bool {
    hebrew_query::is_probably_garbage_pdf_text(&normalized_text)
}

/// 64-bit content fingerprint (FNV-1a over UTF-8 bytes) of a book's raw
/// source text. The whole-book indexing paths ([`SearchEngine::add_text_book`])
/// stamp it on the `textHash` column; recompute it from the current library
/// source and compare against [`SearchEngine::get_book_text_fingerprints`]
/// to detect books whose *content* changed without reindexing everything.
///
/// שימו לב: [`SearchEngine::get_book_fingerprints`] מחזיר את החתימה
/// **הקנונית** [`compute_book_fingerprint`], הכוללת גם metadata (כותרת
/// וכו') — השוואה שלה מול הפונקציה הזו (טקסט בלבד) תזהה כל ספר כ"השתנה".
///
/// Never returns 0 — that value is reserved for "no fingerprint recorded".
/// Deliberately hashes the *raw* text (before normalization/tokenization) so
/// the fingerprint does not shift when text-processing internals change.
///
/// Pure string computation — safe to call synchronously (including from the
/// indexing isolate).
#[frb(sync)]
pub fn compute_content_fingerprint(text: String) -> u64 {
    content_fingerprint(&text)
}

/// [`compute_content_fingerprint`] על bytes גולמיים של UTF-8, אסינכרוני —
/// גיבוב ספר שלם אסור שירוץ על ה-UI isolate של הקורא. UTF-8 לא-תקין
/// מוחלף (lossy), בדיוק כמו במסלולי האינדוקס של הספר השלם, כך שהתוצאה
/// שווה לגיבוב הטקסט המפוענח שנחתם בעמודת `textHash`.
pub fn compute_content_fingerprint_bytes(text: Vec<u8>) -> u64 {
    let text = match String::from_utf8_lossy(&text) {
        std::borrow::Cow::Borrowed(_) => {
            // UTF-8 תקין — נטילת בעלות בלי העתקה נוספת.
            unsafe { String::from_utf8_unchecked(text) }
        }
        std::borrow::Cow::Owned(fixed) => fixed,
    };
    content_fingerprint(&text)
}

fn content_fingerprint(text: &str) -> u64 {
    let mut fnv = Fnv::new();
    fnv.feed(text.as_bytes());
    fnv.finish()
}

/// חתימת האינדוקס הקנונית לספר טקסט: הטקסט הגולמי + כל ה-metadata שמוטבע
/// במסמכי הספר — כותרת, נתיב קטגוריה וממדי הסינון. שינוי בכל אחד מהם, גם ללא
/// שינוי טקסט, משנה את החתימה. מקום הספר בקטלוג ובדורות אינו בה: הוא מיפוי
/// ([`SearchEngine::set_book_order`]) ששינויו אינו מצריך אינדוקס מחדש.
///
/// ה-extra_facets ממוינים ומנוקי-כפילויות — סדרם אינו משפיע על האינדקס.
/// Never returns 0. Pure string computation — safe to call synchronously.
#[frb(sync)]
pub fn compute_book_fingerprint(
    text: String,
    title: String,
    topics: String,
    extra_facets: Option<Vec<String>>,
) -> u64 {
    book_fingerprint(&text, &title, &topics, extra_facets.unwrap_or_default())
}

/// Borrowing form of [`compute_book_fingerprint`] so the indexing path
/// never clones a whole book to hash it.
fn book_fingerprint(text: &str, title: &str, topics: &str, extra_facets: Vec<String>) -> u64 {
    let mut fnv = Fnv::new();
    fnv.feed_field(text.as_bytes());
    feed_metadata(&mut fnv, title, topics, extra_facets);
    fnv.finish()
}

/// The `metadataHash` of a book: everything its documents carry besides its text — title,
/// category path and the extra facets, sorted and deduplicated as the index keeps them.
/// [`SearchEngine::get_book_stamps`] returns the value each book was written with, so a
/// book whose metadata changed is found without reading its text. Never returns 0.
#[frb(sync)]
pub fn compute_book_metadata_fingerprint(
    title: String,
    topics: String,
    extra_facets: Option<Vec<String>>,
) -> u64 {
    metadata_fingerprint(&title, &topics, extra_facets.unwrap_or_default())
}

fn metadata_fingerprint(title: &str, topics: &str, extra_facets: Vec<String>) -> u64 {
    let mut fnv = Fnv::new();
    feed_metadata(&mut fnv, title, topics, extra_facets);
    fnv.finish()
}

fn feed_metadata(fnv: &mut Fnv, title: &str, topics: &str, mut extra_facets: Vec<String>) {
    extra_facets.sort_unstable();
    extra_facets.dedup();
    fnv.feed_field(title.as_bytes());
    fnv.feed_field(topics.as_bytes());
    for facet in &extra_facets {
        fnv.feed_field(facet.as_bytes());
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a מצטבר — הבסיס לכל חתימות התוכן. `feed_field` מקדים קידומת-אורך
/// כדי ששרשורי שדות שונים לא יתלכדו ("אב"+"ג" מול "א"+"בג").
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(FNV_OFFSET)
    }

    fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }

    fn feed_field(&mut self, bytes: &[u8]) {
        self.feed(&(bytes.len() as u64).to_le_bytes());
        self.feed(bytes);
    }

    /// 0 שמור ל"אין חתימה" — לעולם אינו מוחזר כחתימה אמיתית.
    fn finish(self) -> u64 {
        if self.0 == 0 {
            1
        } else {
            self.0
        }
    }
}

/// שורש-ממד שמור בנתיבי facet: נתיב שהמקטע הראשון שלו הוא אחד מאלה שייך
/// לממד סינון (מחבר/תקופה/ספרי-יסוד) ולא לעץ הקטגוריות. הסמנטיקה בסינון
/// (ראו [`SearchEngine::facet_filter_query`]): נתיבים מאותו ממד הם OR
/// ביניהם, וממדים שונים (וכן קבוצת הקטגוריות) הם AND — "ראשונים AND
/// מסכת ברכות" עובד, בעוד ריבוי מחברים נשאר "אחד מהם".
///
/// השורשים באנגלית בכוונה: שמות קטגוריות בספרייה הם עבריים, כך שאין
/// התנגשות עם עץ הקטגוריות הקיים.
pub const FACET_DIMENSION_ROOTS: [&str; 3] = ["author", "era", "base"];

/// תקרת חברות-הקבוצה המוחזרות לכל תוצאה מאוחדת ([`SearchResult::merged`]).
/// `merged_count` מדווח את הגודל האמיתי גם כשהרשימה נחתכת — למעט תחת
/// `truncated`, שם הוא תחתית (ראו [`SearchResult::merged_count`]).
const MERGED_SIBLINGS_CAP: usize = 10;

/// תקרת הקבוצות שהצבירה **הגלובלית** של [`GroupCollector`] מחזיקה (מפה
/// משותפת אחת לכל החיפוש, לא פר-סגמנט). הזיכרון של הקיבוץ פרופורציונלי
/// למספר הקבוצות הייחודיות — בלי תקרה, חיפוש exact של מילה נפוצה
/// (TermQuery, שאינו עובר דרך תקציב ה-postings של מילה-בודדת) צובר
/// GroupAcc לכל שורה ייחודית: ~100 בייט לקבוצה, מאות MB בשאילתות של
/// מיליוני שורות. בהגעה לתקרה — degrade, לא שגיאה: הקבוצה *הגרועה* לפי
/// סדר המיון מפנה את מקומה לקבוצה טובה ממנה (ראו [`BoundedGroups`]) — כך
/// שכל עמוד בטווח התקרה נשאר מדויק — ו-`truncated` מדווח למעלה (אותו דגל
/// "תוצאות חלקיות" שה-UI כבר מציג). `raw_total` נשאר מדויק. הזיכרון:
/// מפה גלובלית חסומת-50k (מפתח + GroupAcc עם עד [`MERGED_SIBLINGS_CAP`]
/// siblings + אינדקס BTreeSet) בתוספת buffers חסומים פר-סגמנט
/// ([`GROUP_FLUSH_BUFFER`]) — חסום, ללא תלות במספר הסגמנטים.
const GROUP_COLLECTOR_MAX_GROUPS: usize = 50_000;

/// תקציב הסעיפים הנספרים במסלול ההתאמה החלקית של טווח "תחת אותה כותרת" —
/// מילה נפוצה חותכת מאות אלפי סעיפים והמפה המשותפת גדלה כאיחוד שלהן.
/// מעבר לתקציב סעיפים *חדשים* נשמטים (degrade מדווח ב-`truncated`, כמו
/// [`GROUP_COLLECTOR_MAX_GROUPS`]) וסעיפים שכבר נספרים ממשיכים להצטבר —
/// המפה חסומה ל-500k רשומות `(u64, usize)`.
const SECTION_COUNT_BUDGET: usize = 500_000;

/// צובר את `sections` לתוך מפת הספירה תחת התקציב; מחזיר האם נחתך.
fn accumulate_section_counts(
    counts: &mut HashMap<u64, usize>,
    sections: HashSet<u64>,
    budget: usize,
) -> bool {
    let mut truncated = false;
    for section in sections {
        if let Some(count) = counts.get_mut(&section) {
            *count += 1;
        } else if counts.len() < budget {
            counts.insert(section, 1);
        } else {
            truncated = true;
        }
    }
    truncated
}

/// מינימום אותיות עבריות לחתימת דה-דופליקציה: שורה קצרה מזה מקבלת 0
/// ("אין חתימה") ולעולם לא תתאחד עם שורות אחרות במצב `IdenticalText` —
/// כותרות ושורות בנות מילה-שתיים זהות בכל מקום ואיחודן היה מטעה.
const LINE_DEDUP_MIN_LETTERS: usize = 12;

/// חתימת דה-דופליקציה לשורת אינדקס: FNV-1a על האותיות והספרות (כל
/// יוניקוד, מקופלות-רישיות) של הטקסט המנורמל — רווחים, פיסוק, גרשיים
/// וסימנים אחרים אינם משתתפים, כך ששתי מהדורות שנבדלות רק
/// בפיסוק/רווחים מקבלות אותה חתימה. ספרות ואותיות זרות *כן* משתתפות:
/// "לשלם 100 שקלים" ו"לשלם 200 שקלים" הן שורות שונות, לא כפילות (המחיר:
/// מהדורה שמוסיפה מספרי-פסוק בספרות לא תתאחד עם מהדורה נקייה — עדיף
/// פיצול-יתר על איחוד שמעלים תוכן). שינויי כתיב (מלא/חסר) גם הם משנים
/// את החתימה — זהו דה-דופ שמרני של טקסט זהה, לא זיהוי מקבילות מקורב.
/// מוטבעת בשדה `lineHash` (FAST) בזמן אינדוקס ומשמשת את מצב האיחוד
/// [`ResultGrouping::IdenticalText`].
///
/// 0 שמור ל"אין חתימה" — פחות מ-[`LINE_DEDUP_MIN_LETTERS`] אותיות
/// *עבריות* (אלפאנומרי משתתף בחתימה אך אינו נספר לסף).
pub(crate) fn line_dedup_hash(normalized_text: &str) -> u64 {
    let mut hasher = LineDedupHasher::new();
    hasher.feed(normalized_text);
    hasher.finish()
}

/// [`line_dedup_hash`] of `parts` joined with spaces, a part more each time: spaces take no
/// part in it, so each part is fed once.
#[cfg(feature = "semantic-integration")]
pub(crate) fn line_dedup_hashes<'a>(parts: &'a [&str]) -> impl Iterator<Item = u64> + 'a {
    let mut hasher = LineDedupHasher::new();
    parts.iter().map(move |part| {
        hasher.feed(part);
        hasher.finish()
    })
}

/// [`line_dedup_hash`], fed in parts.
struct LineDedupHasher {
    fnv: Fnv,
    letters: usize,
}

impl LineDedupHasher {
    fn new() -> Self {
        Self {
            fnv: Fnv::new(),
            letters: 0,
        }
    }

    fn feed(&mut self, text: &str) {
        let mut buf = [0u8; 4];
        for c in text.chars() {
            // Hebrew letters and ASCII skip the Unicode tables (same result).
            if ('א'..='ת').contains(&c) {
                self.letters += 1;
                self.fnv.feed(c.encode_utf8(&mut buf).as_bytes());
                continue;
            }
            if c.is_ascii() {
                if c.is_ascii_alphanumeric() {
                    self.fnv.feed(&[c.to_ascii_lowercase() as u8]);
                }
                continue;
            }
            if !c.is_alphanumeric() {
                continue;
            }
            for lower in c.to_lowercase() {
                self.fnv.feed(lower.encode_utf8(&mut buf).as_bytes());
            }
        }
    }

    fn finish(&self) -> u64 {
        if self.letters < LINE_DEDUP_MIN_LETTERS {
            return 0;
        }
        self.fnv.finish()
    }
}

/// What a text book's documents carry besides its lines.
pub(crate) struct TextBookMeta {
    pub title: String,
    pub topics: String,
    pub file_path: String,
    pub extra_facets: Vec<String>,
    /// The caller's `sourceHash`, 0 for none.
    pub source_hash: u64,
}

/// A text book's documents, built by [`SearchEngine::prepare_text_book`].
#[frb(ignore)]
pub(crate) struct PreparedTextBook {
    pub documents: Vec<TantivyDocument>,
    pub(crate) spooled_documents: Option<crate::indexing_buffers::DocumentSpool>,
    pub(crate) buffered_documents: crate::indexing_diagnostics::BufferedBytes,
    pub(crate) source_generation: Option<(crate::line_source::Source, u64)>,
    /// The book's number: the high half of its ids.
    pub book_number: u32,
    /// The book's `contentHash` ([`book_fingerprint`]).
    pub content_hash: u64,
    /// The book's `textHash` ([`content_fingerprint`]).
    pub text_hash: u64,
    /// The book's `metadataHash` ([`metadata_fingerprint`]).
    pub metadata_hash: u64,
    pub source_hash: u64,
    pub text_storage: TextStorage,
    pub prepare_time: std::time::Duration,
    pub keys_time: std::time::Duration,
}

impl PreparedTextBook {
    #[cfg(test)]
    pub(crate) fn for_each_document(
        &mut self,
        mut f: impl FnMut(TantivyDocument) -> Result<()>,
    ) -> Result<()> {
        if let Some(spool) = &mut self.spooled_documents {
            spool.for_each(f)?;
        } else {
            for document in &self.documents {
                f(document.clone())?;
            }
        }
        Ok(())
    }

    /// Moves the documents into `writer`, in line order; returns how many.
    pub(crate) fn enqueue(&mut self, writer: &IndexWriter) -> Result<u32> {
        if let Some(mut spool) = self.spooled_documents.take() {
            let enqueued = spool.for_each(|document| {
                writer.add_document(document)?;
                Ok(())
            });
            self.buffered_documents.set(0);
            return enqueued;
        }
        let count = self.documents.len() as u32;
        for document in self.documents.drain(..) {
            writer.add_document(document)?;
        }
        self.buffered_documents.set(0);
        Ok(count)
    }
}

/// How many lines `text.split('\n')` yields.
fn line_count(text: &str) -> usize {
    text.bytes().filter(|&b| b == b'\n').count() + 1
}

/// The display copies of a document whose text is kept in the index. `textStored` is
/// written even when empty: its absence is what marks a library-database document.
fn add_stored_text(
    document: &mut TantivyDocument,
    (text_stored_f, vocalized_stored_f): (Field, Field),
    plain: &str,
    vocalized: Option<&str>,
) {
    document.add_text(text_stored_f, plain);
    if let Some(vocalized) = vocalized {
        document.add_text(vocalized_stored_f, vocalized);
    }
}

/// The library book id of an official book's `file_path` (`id:<bookId>`).
#[cfg(feature = "semantic-integration")]
pub(crate) fn library_book_id(file_path: &str) -> Option<i64> {
    match line_source_book(file_path)? {
        (crate::line_source::Source::Library, id) => Some(id),
        _ => None,
    }
}

/// The line source and book id of a `file_path` whose rows a database holds:
/// `id:<bookId>` (library), `uid:<bookId>` (user books) or `db:<slug>:<bookId>` (an
/// attached database), in canonical form only.
pub(crate) fn line_source_book(file_path: &str) -> Option<(crate::line_source::Source, i64)> {
    use crate::line_source::Source;
    let (source, digits) = if let Some(digits) = file_path.strip_prefix("id:") {
        (Source::Library, digits)
    } else if let Some(digits) = file_path.strip_prefix("uid:") {
        (Source::UserBooks, digits)
    } else {
        // As the app's `attachedSlugOfKey`: the id follows the last `:`.
        let (slug, digits) = file_path.strip_prefix("db:")?.rsplit_once(':')?;
        (Source::attached(slug)?, digits)
    };
    let id: i64 = digits.parse().ok()?;
    (id >= 0 && id.to_string() == digits).then_some((source, id))
}

/// `DocumentInput` always stores its text: nothing ties a ready-made document's `segment`
/// to a row of the library database, so a `LibraryDb` one is refused.
fn refuse_library_storage(docs: &[DocumentInput]) -> Result<()> {
    if let Some(doc) = docs
        .iter()
        .find(|doc| doc.text_storage == Some(TextStorage::LibraryDb))
    {
        anyhow::bail!(
            "TextStorage::LibraryDb is only accepted by add_text_book/add_text_book_bytes \
             (line {} of {:?})",
            doc.ordinal,
            doc.file_path
        );
    }
    Ok(())
}

fn require_library_book_id(file_path: &str) -> Result<(crate::line_source::Source, i64)> {
    line_source_book(file_path).with_context(|| {
        format!(
            "TextStorage::LibraryDb needs a file_path of the form id:<bookId>, \
             uid:<bookId> or db:<slug>:<bookId>, got {file_path:?}"
        )
    })
}

/// `LibraryDb` when the book's line source holds exactly `lines` rows for the book;
/// otherwise `InIndex`, since a document's `segment` would not be its row's ordinal (a row
/// containing `\n` splits into two lines) or nothing can tell.
fn library_storage_or_fallback(
    (source, book_id): (crate::line_source::Source, i64),
    lines: usize,
    title: &str,
) -> TextStorage {
    let rows =
        crate::line_source::with_store_for_indexing(source, |store| store.book_row_count(book_id));
    match rows {
        Ok(rows) if rows == lines as u64 => TextStorage::LibraryDb,
        rows => {
            crate::line_source::note_library_fallback(source);
            warn!(
                "add_text_book '{title}' ({source:?} {book_id}): {lines} lines against \
                 database rows {rows:?}; storing its text in the index"
            );
            TextStorage::InIndex
        }
    }
}

/// Mirrors the Dart `IndexingDocumentBuilder._updateReferenceTrail`: a new
/// `<h…>` heading line replaces any earlier trail entry that shares its
/// first four characters (same heading level) and everything after it, then
/// appends itself. Char-based like the Dart UTF-16 indexing (identical for
/// BMP text, which Hebrew books are).
fn update_reference_trail<'a>(trail: &mut Vec<&'a str>, line: &'a str) {
    if line.chars().count() >= 4 && !trail.is_empty() {
        let prefix: Vec<char> = line.chars().take(4).collect();
        if let Some(idx) = trail.iter().position(|entry| {
            entry.chars().take(4).eq(prefix.iter().copied()) && entry.chars().count() >= 4
        }) {
            trail.truncate(idx);
        }
    }
    trail.push(line);
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
        return check_sidecar_metadata(index_path, metadata_path);
    }

    check_legacy_tantivy_metadata(index_path, metadata_path)
}

/// `Some(reason)` כשלא ניתן לאשר שהסכימה השמורה ב-meta.json של tantivy זהה
/// לסכימת המנוע הנוכחית — בדיוק ההשוואה ש-`Index::open_or_create` מבצע.
/// בלעדיה, קובץ צדדי שמצהיר על הגרסה הנכונה עובר את בדיקת התאימות בעוד
/// שפתיחת המנוע עדיין נופלת על SchemaError (אינדקס שנבנה בגרסת-ביניים של
/// אותה schema_version) — והאפליקציה נופלת בשקט לאינדקס זמני.
/// גם meta.json חסר/פגום נחשב אי-התאמה: sidecar תקין לא מעיד כלום כשה-metadata
/// של tantivy עצמו לא קריא, ופתיחת האינדקס תיכשל באותה מידה.
fn stored_schema_mismatch(index_path: &Path, version: u32, expected: &Schema) -> Option<String> {
    let raw = match fs::read_to_string(index_path.join("meta.json")) {
        Ok(raw) => raw,
        Err(err) => return Some(format!("tantivy meta.json is missing or unreadable: {err}")),
    };
    let meta: JsonValue = match serde_json::from_str(&raw) {
        Ok(meta) => meta,
        Err(err) => return Some(format!("tantivy meta.json is not valid JSON: {err}")),
    };
    let Some(schema_json) = meta.get("schema").cloned() else {
        return Some("tantivy meta.json has no schema entry".to_string());
    };
    match serde_json::from_value::<Schema>(schema_json) {
        Ok(stored) if stored == *expected => None,
        Ok(_) => Some(format!(
            "tantivy schema on disk differs from the engine schema of version {version}"
        )),
        Err(err) => Some(format!("stored tantivy schema is unreadable: {err}")),
    }
}

fn check_sidecar_metadata(index_path: &Path, metadata_path: PathBuf) -> IndexCompatibility {
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

    if metadata.schema_version < MIN_READABLE_SCHEMA_VERSION {
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

    // The schema the declared version has, and no other: a sidecar can declare a version
    // its index was not built at, as an intermediate build did.
    let expected = schema_of_version(metadata.schema_version)
        .expect("every version from MIN_READABLE_SCHEMA_VERSION to INDEX_SCHEMA_VERSION has one");
    if let Some(reason) = stored_schema_mismatch(index_path, metadata.schema_version, &expected) {
        return compatibility(
            false,
            "rebuild_required",
            Some(metadata.schema_version),
            metadata_path,
            Some(reason),
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

    if let Some(version) = readable_schema_version(&tantivy_metadata) {
        // Versions 7 through 10 have the same fields, but different text recipes.
        if !tantivy_metadata
            .get("segments")
            .and_then(JsonValue::as_array)
            .is_some_and(Vec::is_empty)
        {
            return compatibility(
                false,
                "rebuild_required",
                None,
                metadata_path,
                Some(
                    "otzaria metadata is missing: the text recipe of existing segments is unknown"
                        .to_string(),
                ),
            );
        }
        return compatibility(
            true,
            "legacy_compatible",
            Some(version),
            metadata_path,
            Some(format!(
                "otzaria metadata is missing, but the index has no segments and its Tantivy \
                 schema matches the engine's schema version {version}"
            )),
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

/// The version, of those this engine reads, whose schema the on-disk one is. Compares the
/// full schema — the same equality `Index::open_or_create` enforces — so a legacy index
/// can't pass the check (e.g. on the `id` field alone) and then fail to open.
fn readable_schema_version(metadata: &JsonValue) -> Option<u32> {
    let found: Schema = serde_json::from_value(metadata.get("schema")?.clone()).ok()?;
    (MIN_READABLE_SCHEMA_VERSION..=INDEX_SCHEMA_VERSION)
        .rev()
        .find(|&version| schema_of_version(version).is_some_and(|schema| schema == found))
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

/// Writes the metadata of an index found without any (`legacy_compatible`), a new one
/// included: the version its schema is, and — when that schema has the `chunkKey` column
/// and the index holds no document yet (`empty`) — this build's chunk-key recipe, under
/// which every key it will hold is computed. The recipe of a column that already holds keys
/// cannot be read back from it, so it is left unrecorded, and the column counts as absent.
fn ensure_current_index_metadata(index_path: &Path, empty: bool) -> Result<()> {
    let compatibility = check_index_compatibility_path(index_path);
    if !compatibility.compatible || compatibility.status == "compatible" {
        return Ok(());
    }
    let Some(version) = compatibility.found_schema_version else {
        return Ok(());
    };
    let has_column =
        schema_of_version(version).is_some_and(|schema| schema.get_field(CHUNK_KEY_FIELD).is_ok());
    let recipe = (has_column && empty).then(ChunkKeyRecipe::current);
    write_index_metadata(index_path, &index_metadata(version, recipe))
}

/// What a new index's metadata is, written into a directory a test made by other means.
#[cfg(test)]
fn write_current_index_metadata(index_path: &Path) -> Result<()> {
    write_index_metadata(index_path, &current_index_metadata())
}

fn write_index_metadata(index_path: &Path, metadata: &IndexMetadata) -> Result<()> {
    let metadata_path = index_metadata_path(index_path);
    let serialized = serde_json::to_string_pretty(metadata)?;
    atomic_replace(&metadata_path, format!("{serialized}\n").as_bytes()).with_context(|| {
        format!(
            "failed to write index metadata to {}",
            metadata_path.display()
        )
    })
}

/// The metadata of a new index: this engine's schema version and chunk-key recipe.
#[cfg(test)]
fn current_index_metadata() -> IndexMetadata {
    index_metadata(INDEX_SCHEMA_VERSION, Some(ChunkKeyRecipe::current()))
}

fn index_metadata(schema_version: u32, chunk_keys: Option<ChunkKeyRecipe>) -> IndexMetadata {
    IndexMetadata {
        format: INDEX_FORMAT.to_string(),
        schema_version,
        engine_version: env!("CARGO_PKG_VERSION").to_string(),
        tantivy_version: TANTIVY_INDEX_VERSION.to_string(),
        created_at_unix_seconds: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        line_text_version: chunk_keys.map(|recipe| recipe.line_text_version),
        chunk_key_version: chunk_keys.map(|recipe| recipe.key_version),
        chunk_key_chunking_identity: chunk_keys.map(|recipe| recipe.chunking_identity),
    }
}

/// The metadata an index directory holds, when it holds metadata this engine reads.
fn read_index_metadata(index_path: &Path) -> Option<IndexMetadata> {
    let path = index_metadata_path(index_path);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        Err(err) => {
            warn!("index metadata {} is unreadable: {err}", path.display());
            return None;
        }
    };
    serde_json::from_str(&raw)
        .inspect_err(|err| warn!("index metadata {} is invalid: {err}", path.display()))
        .ok()
}

/// The index's `chunkKey` column, when it has one written under this build's recipe.
///
/// A column whose metadata records another recipe, or none, is one this build does not use: it counts as absent, so nothing reads it and nothing is
/// written to it, and the keys of the index's lines are computed from their text instead.
pub(crate) fn live_chunk_key_field(schema: &Schema, index_path: &Path) -> Option<Field> {
    let field = schema.get_field(CHUNK_KEY_FIELD).ok()?;
    let recorded = read_index_metadata(index_path).and_then(|metadata| metadata.chunk_key_recipe());
    let current = ChunkKeyRecipe::current();
    if recorded == Some(current) {
        return Some(field);
    }
    info!(
        "the index's chunkKey column was written under {recorded:?}, and this build computes \
         {current:?}: the column counts as absent"
    );
    None
}

/// Opens the index in `directory` with the schema it was built with, or creates one with
/// [`current_schema`] when there is none.
///
/// An index of an older version this engine reads is opened as it is: `open_or_create`
/// refuses it under [`current_schema`], and it is opened again under the schema of each
/// older version in turn. A schema none of them is stays refused, as the `SchemaError`
/// `open_or_create` returns.
fn open_or_create_index(directory: IndexDirectory) -> tantivy::Result<Index> {
    let mut opened = Index::builder()
        .schema(current_schema())
        .settings(new_index_settings())
        .open_or_create(directory.clone());
    for version in (MIN_READABLE_SCHEMA_VERSION..INDEX_SCHEMA_VERSION).rev() {
        if !matches!(opened, Err(tantivy::TantivyError::SchemaError(_))) {
            break;
        }
        let schema = schema_of_version(version).expect("every readable version has a schema");
        opened = Index::open_or_create(directory.clone(), schema);
    }
    opened
}

/// Doc-store settings of a new index; an existing index keeps those it was created with.
/// zstd over 16K blocks: a cold random read is ~45µs against ~108µs at 64K, for ~7% more store.
fn new_index_settings() -> tantivy::IndexSettings {
    tantivy::IndexSettings {
        docstore_compression: tantivy::store::Compressor::Zstd(tantivy::store::ZstdCompressor {
            compression_level: Some(DOCSTORE_ZSTD_LEVEL),
        }),
        docstore_blocksize: DOCSTORE_BLOCK_SIZE,
        ..tantivy::IndexSettings::default()
    }
}

const DOCSTORE_ZSTD_LEVEL: i32 = 3;
const DOCSTORE_BLOCK_SIZE: usize = 16_384;

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

/// The schema of the indexes this engine creates, version [`INDEX_SCHEMA_VERSION`]. Kept in
/// one place so `new()` and the compatibility checks can never drift apart.
fn current_schema() -> Schema {
    index_schema()
}

/// The one schema an index of `version` has, for the versions this engine reads.
fn schema_of_version(version: u32) -> Option<Schema> {
    match version {
        10 => Some(index_schema_of_version(false)),
        INDEX_SCHEMA_VERSION => Some(index_schema()),
        _ => None,
    }
}

/// The schema of version [`INDEX_SCHEMA_VERSION`].
fn index_schema() -> Schema {
    index_schema_of_version(true)
}

fn index_schema_of_version(notes: bool) -> Schema {
    let mut schema_builder = Schema::builder();
    // Deliberately NOT fast: a text fast field stores every raw line in a
    // columnar dictionary — a second full copy of the corpus — and nothing
    // reads it (collectors use only the filePath/contentHash/id columns).
    // Not stored either: Tantivy stores per field, so the display copy lives in
    // `textStored`, written only for documents whose text is not read from the
    // library database (see `TextStorage`).
    schema_builder.add_text_field(
        "text",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("hebrew")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    // השדה המנוקד: מאוכלס רק בשורות שנושאות ניקוד/טעמים (שאר השורות פשוט
    // אינן קיימות בו — tantivy מטפל בשדה חסר בחינם). מאונדקס בטוקנייזר
    // ששומר את הסימנים; העותק המוצג בתוצאות חיפוש מנוקד הוא
    // `textVocalizedStored`. חיפוש רגיל אינו נוגע בשדה הזה כלל.
    schema_builder.add_text_field(
        "textVocalized",
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("hebrew_vocalized")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        ),
    );
    schema_builder.add_text_field("reference", STORED);
    // `title`, `isPdf` and `filePath` are read from their columns (see `doc_store`), and
    // `segment` is stored only where `id` does not imply it.
    schema_builder.add_text_field(
        "title",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("raw")
                    .set_fieldnorms(false),
            )
            .set_fast(None),
    );
    // Not indexed: a lookup by id scans the column instead (see `id_lookup`).
    schema_builder.add_u64_field("id", FAST | STORED);
    schema_builder.add_u64_field("segment", STORED);
    schema_builder.add_bool_field("isPdf", FAST);
    schema_builder.add_text_field(
        "filePath",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("raw")
                    .set_fieldnorms(false),
            )
            .set_fast(None),
    );
    // Book-level content fingerprint, stamped identically on all documents of
    // a book. FAST-only: read columnar by get_book_fingerprints, never searched
    // or returned. 0 = no fingerprint recorded.
    schema_builder.add_u64_field("contentHash", FAST);
    // חתימת טקסט-בלבד של הספר (content_fingerprint על הטקסט הגולמי), זהה על
    // כל מסמכי הספר. FAST בלבד — נקראת עמודתית ע"י get_book_text_fingerprints.
    schema_builder.add_u64_field("textHash", FAST);
    // The book's metadata (`compute_book_metadata_fingerprint`) and the caller's opaque
    // source hash, on the same one document as the fingerprints.
    schema_builder.add_u64_field(METADATA_HASH_FIELD, FAST);
    schema_builder.add_u64_field(SOURCE_HASH_FIELD, FAST);
    // מזהה סעיף: כל השורות שתחת אותה כותרת (אותו בלוק reference) נושאות
    // אותו ערך, ייחודי גלובלית — id_base של הספר + אינדקס בלוק הכותרת
    // (ב-PDF: מספר העמוד). FAST בלבד: מסלול "תחת אותה כותרת"
    // (SearchScope::SameSection) קורא אותו עמודתית לחיתוך סעיפים; אינו
    // מאוחסן ואינו מחופש ישירות.
    // `sectionId` is derived from `id` and this (see `derived_columns`): a value relative
    // to the book compresses to a few bits. The generation of a book is not in the index:
    // see `book_order`.
    schema_builder.add_u64_field(SECTION_OFFSET_FIELD, FAST);
    // חתימת דה-דופליקציה של גוף השורה (ראו line_dedup_hash). FAST בלבד:
    // נקראת עמודתית ע"י קיבוץ IdenticalText; אינה מאוחסנת ואינה מחופשת.
    schema_builder.add_u64_field("lineHash", FAST);
    // CRC-32 of the exact line the indexing path normalized (`line_source::line_check`),
    // written only for documents whose text is read from the library database: the row
    // read back must match it to be shown as the indexed line.
    schema_builder.add_u64_field(LINE_CHECK_FIELD, FAST);
    schema_builder.add_facet_field("topics", FacetOptions::default());
    // The key of the text the line is embedded as, `ChunkKey::column_value` (see
    // `semantic_keys`): 0 for a line the recipe does not embed, a PDF's, and any line
    // not added by `add_text_book`. FAST only: read columnar to tie a stored vector to
    // the lines that hold its text; neither searched nor stored.
    schema_builder.add_u64_field(CHUNK_KEY_FIELD, FAST);
    // Display copies of `text`/`textVocalized`, stored only (never indexed): on every
    // `TextStorage::InIndex` document and on the `stored_lines` of a `LibraryDb` book.
    // `textStored` is present on each, even when empty, so its absence marks a document
    // whose text is in the library database.
    schema_builder.add_text_field(TEXT_STORED_FIELD, STORED);
    schema_builder.add_text_field(TEXT_VOCALIZED_STORED_FIELD, STORED);
    // Where a phrase may continue from one line onto the next (see `cross_line`): the
    // positions of the line's first and last content words, and their terms.
    schema_builder.add_u64_field(cross_line::LINE_FIRST_FIELD, FAST);
    schema_builder.add_u64_field(cross_line::LINE_LAST_FIELD, FAST);
    schema_builder.add_text_field(
        cross_line::LINE_EDGE_FIELD,
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("raw")
                .set_fieldnorms(false)
                .set_index_option(IndexRecordOption::Basic),
        ),
    );
    // Appended so every pre-existing field retains its number. No indexing, fast
    // column, fieldnorm or tokenizer: note boundaries affect display only.
    if notes {
        schema_builder.add_text_field(note_spans::FIELD, STORED);
    }
    schema_builder.build()
}

pub(crate) const TEXT_STORED_FIELD: &str = "textStored";
const METADATA_HASH_FIELD: &str = "metadataHash";
const SOURCE_HASH_FIELD: &str = "sourceHash";
pub(crate) const LINE_CHECK_FIELD: &str = "lineCheck";
const TEXT_VOCALIZED_STORED_FIELD: &str = "textVocalizedStored";

/// Cache key for materialized single-word term sets. The searcher
/// `generation_id` changes on every reader reload (i.e. after each commit),
/// so entries from a stale index snapshot can never be served.
#[derive(Clone, Hash, PartialEq, Eq)]
struct TermCacheKey {
    generation: u64,
    /// The dictionary the branches were scanned against (`text` vs
    /// `textVocalized`) — identical branch strings on different fields
    /// materialize different term sets.
    field: Field,
    branches: Vec<String>,
    /// Tokens expanded via Levenshtein-1 automaton scans (single-word typo
    /// path); part of the key because the same branches with different typo
    /// tokens materialize different term sets.
    typo_tokens: Vec<String>,
    max_expansions: u32,
    /// The postings-cost ceiling the terms were collected under (differs
    /// between the single-word path and the phrase-position path, which can
    /// otherwise share every other key component through the string API).
    postings_budget: u64,
}

/// Entries kept in [`SearchEngine::term_cache`]. Each entry holds at most
/// `max_expansions` terms (≤50 000 short Hebrew tokens ≈ ~1MB worst case), so
/// the cache tops out at a few tens of MB when 32 distinct worst-case queries
/// are live — typically far less (the postings budget truncates collection
/// long before the term ceiling on any realistic query).
const TERM_CACHE_ENTRIES: usize = 32;

/// A materialized single-word term set plus whether collection stopped early
/// on a budget overflow. Cached together so every engine call behind one user
/// search (stream + counts + count-by-book + facets + pagination) reports the
/// same truncation state without re-scanning the FST dictionary.
#[derive(Clone)]
struct CachedTermSet {
    terms: Arc<Vec<Term>>,
    truncated: bool,
}

/// Ceiling on the summed `doc_freq` (over all segments) of the terms a
/// single-word term set keeps. This — not the term count — is the true cost
/// guard: executing the resulting `TermSetQuery` unions one postings list per
/// matched term per segment into a BitSet, O(Σ doc_freq). The term-count
/// ceiling (`max_expansions`) remains only as a memory guard on the
/// materialized `Vec<Term>`. On the full library 8M keeps every realistic
/// expansion whole (אמר + affixes: 2.6M, ~60ms to count) and stops only
/// pathological ones (ב with prefix+suffix windows: 64M).
const SINGLE_WORD_POSTINGS_BUDGET: u64 = 8_000_000;

/// Term ceiling for one *word position* of a phrase on the degrade path
/// ([`TermListPhraseQuery`]). Unlike tantivy's `RegexPhraseQuery` ceiling —
/// which is cumulative across every position and *errors* on overflow — this
/// bounds each position independently and overflow degrades: collection
/// truncates from the back of the branch-priority order, exactly like the
/// single-word path. The value bounds the materialized `Vec<Term>` and the
/// per-segment positional postings the verifier opens (one per matched term
/// per position), so it is a memory guard, not a scan-time guard.
const PHRASE_POSITION_MAX_EXPANSIONS: u32 = 8_192;

/// Postings-cost ceiling for one phrase position on the degrade path.
/// Deliberately far looser than [`SINGLE_WORD_POSTINGS_BUDGET`]: the exact
/// `RegexPhraseQuery` path this replaces never bounded Σ doc_freq at all (its
/// bucketed unions contain the post-expansion cost), so a tight budget here
/// would truncate phrases that used to work. This exists only as a backstop
/// against a pathological position (e.g. a 1-char word with prefix+suffix
/// windows) unioning a huge slice of the index.
const PHRASE_POSITION_POSTINGS_BUDGET: u64 = 20_000_000;

// ── Vocalized-path expansion ceilings ──────────────────────────────────────
// Vocalized patterns are always expansions (free-mark runs match every
// vocalization of the word), so even "exact" vocalized search materializes a
// term set. The postings budget above remains the true cost guard; these
// only bound the materialized `Vec<Term>` / phrase-expansion memory.

/// Term ceiling for one exact vocalized word (`TermSetQuery` path).
const VOC_EXACT_SINGLE_MAX_EXPANSIONS: u32 = 4_096;
/// Cumulative expansion ceiling for a vocalized phrase (`RegexPhraseQuery`).
const VOC_PHRASE_MAX_EXPANSIONS: u32 = 8_192;
/// Term ceiling for one fuzzy vocalized word (exact + lexical + edit-distance
/// branches share it; overflow degrades like the advanced single-word path).
const VOC_FUZZY_MAX_EXPANSIONS: u32 = 20_000;
/// Cap on plain-dictionary variants collected per token by the vocalized
/// fuzzy/typo expansion (the Levenshtein scan runs on mark-free bases).
const VOC_VARIANTS_PER_TOKEN: usize = 128;

/// Mirrors the pinned sidecar's semantic candidate ceiling. Applying the same
/// bound before constructing Tantivy's `TopDocs` collector prevents hostile or
/// accidental `limit`/`offset` values from driving an unbounded allocation.
#[cfg(feature = "semantic-integration")]
const MAX_SEMANTIC_CANDIDATE_WINDOW: u32 = 10_000;

pub struct SearchEngine {
    /// The schema of the index this engine opened, which is the one it was built with.
    schema: Schema,
    /// The `chunkKey` column, when the index has one this build uses: written under the
    /// chunk-key recipe this build computes (see [`live_chunk_key_field`]). `None` for a
    /// column written under another recipe, or none recorded, which counts as absent:
    /// nothing writes to it, and nothing reads it.
    chunk_key_field: Option<Field>,
    /// The directory this engine opened. Retained only so a build can ask whether the
    /// index it is about to read is one this version reads — see
    /// [`Self::index_compatibility`].
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    index_path: PathBuf,
    index: Index,
    index_writer: Option<IndexWriter>,
    writer_closed: bool,
    // This handle may read an app-owned index but can never create a writer or
    // mutate index/order/vector files, even if a caller invokes a write API.
    read_only: bool,
    writer_heap_size: usize,
    economy_indexing: bool,
    index_reader: IndexReader,
    /// Optional lexical morphology lexicon for the approximate (`fuzzy`) path.
    /// `None` until [`SearchEngine::set_magic_dictionary_path`] loads a valid
    /// `lexical.db`; while `None`, fuzzy search behaves exactly as before.
    magic_dict: Option<MagicDictionary>,
    /// מילון תרגום ארמי↔עברי לאפשרות "תרגום ארמי" של החיפוש המתקדם.
    /// `None` עד ש-[`SearchEngine::set_translation_dictionary_path`] טוען
    /// קובץ תקין; בהיעדרו האפשרות פשוט לא מרחיבה דבר.
    translation_dict: Option<TranslationLexicon>,
    /// מילון פענוח ראשי-תיבות לאפשרות "ראשי תיבות" של החיפוש המתקדם.
    /// `None` עד ש-[`SearchEngine::set_acronyms_dictionary_path`] טוען קובץ
    /// תקין; בהיעדרו האפשרות פשוט לא מרחיבה דבר.
    acronym_dict: Option<AcronymLexicon>,
    /// Materialized-terms cache for the single-word regex path. One user
    /// search triggers several engine calls with identical parameters
    /// (stream + count + count-by-book + facet counts + pagination), and the
    /// FST dictionary scan behind `single_regex_term_query` is the expensive
    /// part of each — this makes every call after the first near-free.
    term_cache: Mutex<LruCache<TermCacheKey, CachedTermSet>>,
    term_work: crate::highlight_cache::WorkGate<TermCacheKey>,
    /// Bulk-indexing mode (see [`SearchEngine::set_bulk_indexing`]): while
    /// on, the live writer (and any lazily-reopened one) uses `NoMergePolicy`.
    bulk_indexing: bool,
    /// Optional semantic sidecar. It owns vector retrieval, fusion, grouping
    /// and semantic-index lifecycle; Tantivy remains owned by this engine.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    semantic_runtime: SemanticRuntime,
    /// What resolving a vector set's hits against this index remembers from one search to
    /// the next, for one generation of it: see [`crate::semantic_resolver`].
    #[cfg(feature = "semantic-integration")]
    semantic_resolver: Mutex<crate::semantic_resolver::ResolverCache>,
    /// The open vector set's generation as a search plans its scan against it: see
    /// [`crate::semantic_moves`]. Opened on the first planned search of a generation, and
    /// let go when the session moves to another or closes.
    #[cfg(feature = "semantic-integration")]
    semantic_set_view: Mutex<Option<Arc<crate::semantic_moves::SetView>>>,
    /// The results recent semantic searches showed, which their next pages continue.
    #[cfg(feature = "semantic-integration")]
    semantic_sessions: SemanticSessionCache,
    /// Passage highlights already computed: see [`Self::semantic_passage_highlights`].
    #[cfg(feature = "semantic-integration")]
    semantic_highlights: crate::semantic_highlight::HighlightCache,
    /// Ranked hits and counts of recent searches: see [`crate::page_cache`].
    page_cache: PageCache,
    /// Highlight term sets and display patterns: see [`crate::highlight_cache`].
    highlight_cache: HighlightCache,
    grouped_cache: GroupCache,
    dictionary_epoch: u64,
    memory_epoch: std::sync::atomic::AtomicU64,
    /// Request ids of running lexical searches: see [`crate::lexical_cancellation`].
    lexical_cancellations: Arc<LexicalCancellations>,
    /// Which book holds which number (see [`crate::book_order`]): read from the index when
    /// first needed.
    book_numbers: Mutex<Option<BookNumbers>>,
    /// Set whenever this engine (re)takes the writer: another engine may have written and
    /// handed out numbers while it did not hold it.
    numbers_stale: AtomicBool,
    /// The order mapping [`Self::set_book_order`] stores.
    book_order: RwLock<BookOrderState>,
    /// The rank tables of the current mapping and book numbers.
    order_tables: Mutex<Option<CachedOrderTables>>,
}

/// How [`SearchEngine::write_inputs_with`] makes a document's id.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ids {
    /// From the book's number and the line's ordinal.
    Composed,
    /// This id, for the one document written: tests written before book numbers existed,
    /// and tests of ids the engine never composes.
    #[cfg(test)]
    Raw(u64),
}

/// Which documents [`SearchEngine::write_inputs_with`] records a book's stamps on.
#[derive(Clone, Copy)]
enum Stamping {
    /// The first of each book in the batch (again when its stamps differ).
    FirstInBatch,
    /// Line 0 only: one line written at a time would otherwise stamp every line.
    LineZero,
}

/// The order mapping as an engine holds it.
struct BookOrderState {
    revision: u64,
    places: Arc<HashMap<String, BookPlace>>,
    /// Process-unique, so a cache entry ranked under another mapping is never served.
    epoch: u64,
    /// The mapping file as it was read or written, to notice another engine replacing it.
    stamp: Option<(SystemTime, u64)>,
}

impl BookOrderState {
    fn of(mapping: OrderMapping, stamp: Option<(SystemTime, u64)>) -> Self {
        Self {
            revision: mapping.revision,
            places: Arc::new(mapping.places),
            epoch: book_order::next_epoch(),
            stamp,
        }
    }

    fn read(index_dir: &Path) -> Self {
        // Stamped first: a file replaced after the read is read again next time.
        let stamp = order_file_stamp(index_dir);
        Self::of(OrderMapping::load(index_dir), stamp)
    }
}

struct CachedOrderTables {
    epoch: u64,
    numbers_version: u64,
    tables: Arc<OrderTables>,
}

/// Analyzers that keep the quote-free twin of a quoted word (`רמב"ם` → `רמבם` at the
/// same position): what snippets and phrase highlights tokenize text with, so a query
/// reaching `רמב"ם` through `רמבם` paints it, and what `lineEdge` terms are made with.
pub(crate) const DISPLAY_ANALYZER: &str = "hebrew_display";
pub(crate) const VOCALIZED_DISPLAY_ANALYZER: &str = "hebrew_vocalized_display";

/// The analyzers every field and query of the index is tokenized with.
/// `index_twins`: the fields' analyzers emit the quote-free twin, as up to schema 5 —
/// only for tests comparing such an index with one without.
fn register_hebrew_tokenizers_with(index: &Index, index_twins: bool) {
    // The fields index each word once; a query reaches its quoted spellings through
    // `quote_expansion`, and the query analyzers never split a word in two.
    // RemoveLongFilter על *כל* האנליזטורים — כולל צד השאילתה, אחרת ספירת הטוקנים
    // ב-PhraseQuery סוטה מהאינדקס (זבל base64 שזלג לקורפוס לא נכנס למילון).
    for (name, emit_quote_free, keep_marks) in [
        ("hebrew", index_twins, false),
        ("hebrew_vocalized", index_twins, true),
        (DISPLAY_ANALYZER, true, false),
        (VOCALIZED_DISPLAY_ANALYZER, true, true),
        ("hebrew_query", false, false),
        ("hebrew_vocalized_query", false, true),
    ] {
        index.tokenizers().register(
            name,
            TextAnalyzer::builder(HebrewTokenizer {
                emit_quote_free,
                keep_marks,
            })
            .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
            .filter(LowerCaser)
            .build(),
        );
    }
}

/// Installs a stderr logger (once per process) so the engine's `info!`
/// timing logs are visible in the app console without any Dart-side setup.
/// `RUST_LOG` still overrides the default filter; if a logger is already
/// installed (tests, benchmark_cli), `try_init` leaves it in place.
fn init_engine_logger() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or("search_engine=info"),
        )
        .try_init();
    });
}

impl SearchEngine {
    /// Deliberately **not** `#[frb(sync)]` — opening the index mmaps and reads every
    /// segment footer; a synchronous binding blocks the calling Dart isolate throughout.
    pub fn new(path: &str) -> Self {
        Self::open_with(path, false)
    }

    /// An engine whose index holds the quote-free twins schema 5 indexed: for tests that
    /// compare answers with and without them.
    #[cfg(test)]
    pub(crate) fn with_quote_free_twins(path: &str) -> Self {
        Self::open_with(path, true)
    }

    fn open_with(path: &str, index_twins: bool) -> Self {
        init_engine_logger();
        debug!("new path={}", path);
        let directory =
            IndexDirectory::open(Path::new(path)).expect("unable to open mmap directory");
        let mut index = match open_or_create_index(directory) {
            Ok(index) => index,
            Err(tantivy::TantivyError::SchemaError(err)) => panic!(
                "index at {path} was built with an incompatible schema ({err}); \
                 call check_index_compatibility before opening and rebuild the index"
            ),
            Err(err) => panic!("Failed to open index at {path}: {err}"),
        };
        let schema = index.schema();
        register_hebrew_tokenizers_with(&index, index_twins);
        set_search_executor(&mut index);
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .expect("Failed to create index reader");
        // Search-only instances acquire neither the writer lock nor its worker pool.
        let index_writer = None;

        // Nothing committed yet, so a chunk-key recipe recorded now describes every key the
        // index will hold.
        let empty = index
            .searchable_segment_ids()
            .is_ok_and(|segments| segments.is_empty());
        if let Err(err) = ensure_current_index_metadata(Path::new(path), empty) {
            warn!("failed to ensure index metadata: {err:#}");
        }
        let chunk_key_field = live_chunk_key_field(&schema, Path::new(path));

        SearchEngine {
            schema,
            chunk_key_field,
            index_path: PathBuf::from(path),
            index,
            index_writer,
            writer_closed: false,
            read_only: false,
            writer_heap_size: DEFAULT_WRITER_HEAP_SIZE,
            economy_indexing: false,
            index_reader,
            magic_dict: None,
            translation_dict: None,
            acronym_dict: None,
            term_work: crate::highlight_cache::WorkGate::default(),
            term_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(TERM_CACHE_ENTRIES).expect("cache size is non-zero"),
            )),
            bulk_indexing: false,
            #[cfg(feature = "semantic-integration")]
            semantic_runtime: RwLock::new(None),
            #[cfg(not(feature = "semantic-integration"))]
            semantic_runtime: (),
            #[cfg(feature = "semantic-integration")]
            semantic_resolver: Mutex::default(),
            #[cfg(feature = "semantic-integration")]
            semantic_set_view: Mutex::default(),
            #[cfg(feature = "semantic-integration")]
            semantic_sessions: SemanticSessionCache::default(),
            #[cfg(feature = "semantic-integration")]
            semantic_highlights: Default::default(),
            page_cache: PageCache::default(),
            highlight_cache: HighlightCache::default(),
            grouped_cache: GroupCache::default(),
            dictionary_epoch: book_order::next_epoch(),
            memory_epoch: std::sync::atomic::AtomicU64::default(),
            lexical_cancellations: Arc::default(),
            book_numbers: Mutex::new(None),
            numbers_stale: AtomicBool::new(true),
            book_order: RwLock::new(BookOrderState::read(Path::new(path))),
            order_tables: Mutex::new(None),
        }
    }

    /// Merges every committed segment into one, so a test can compare on-disk sizes that
    /// do not depend on how the writer's threads happened to split the documents.
    #[cfg(test)]
    pub(crate) fn merge_into_one_segment(&mut self) {
        let writer = self.writer_mut().unwrap();
        writer.commit().unwrap();
        let segments = self.index.searchable_segment_ids().unwrap();
        let writer = self.writer_mut().unwrap();
        if segments.len() > 1 {
            writer.merge(&segments).wait().unwrap();
        }
        writer.garbage_collect_files().wait().unwrap();
        self.index_reader.reload().unwrap();
    }

    /// Opens an existing compatible index without a writer or index metadata changes.
    /// Missing, legacy and newer schemas are errors; this never creates or upgrades them.
    /// Tantivy still takes its metadata coordination lock (which may create the lock
    /// file), so this is a read-only *index handle*, not support for a filesystem that
    /// forbids lock bookkeeping. Mutating APIs return an error for the handle's lifetime.
    /// The reader is a stable snapshot. Use [`Self::refresh_reader`] for commits in the
    /// same index directory; close/reopen the handle after replacing the directory.
    /// Semantic artifact opening is refused because the sidecar currently performs
    /// on-disk recovery while opening its otherwise read-only session.
    #[frb(ignore)]
    pub fn open_read_only(path: &str) -> Result<Self> {
        let path = Path::new(path);
        let compatibility = check_index_compatibility_path(path);
        anyhow::ensure!(
            compatibility.compatible,
            "cannot open index read-only at {}: {} ({:?})",
            path.display(),
            compatibility.status,
            compatibility.reason,
        );
        let directory = IndexDirectory::open(path)
            .with_context(|| format!("cannot open index directory {}", path.display()))?;
        let index = Index::open(directory)
            .with_context(|| format!("cannot open index at {}", path.display()))?;
        Self::from_read_only_index(index, path)
    }

    /// Publishes committed changes from the same open index. The exclusive receiver
    /// keeps host-level searches from overlapping a refresh; Tantivy keeps any older
    /// searcher alive until its users finish. It never commits or acquires a writer.
    /// A directory replacement requires reopening, because mapped segments and the
    /// directory object still belong to the previous index.
    #[frb(ignore)]
    pub fn refresh_reader(&mut self) -> Result<()> {
        let compatibility = check_index_compatibility_path(&self.index_path);
        anyhow::ensure!(
            compatibility.compatible,
            "cannot refresh index: {} ({:?})",
            compatibility.status,
            compatibility.reason,
        );
        self.index_reader.reload()?;
        self.release_memory();
        self.numbers_stale.store(true, Ordering::Release);
        Ok(())
    }

    /// An engine over an index opened by the caller, with no writer and no metadata written:
    /// read-only hosts and benchmarks on an index another process owns.
    pub(crate) fn from_read_only_index(mut index: Index, path: &Path) -> Result<Self> {
        let schema = index.schema();
        register_hebrew_tokenizers_with(&index, false);
        set_search_executor(&mut index);
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        let chunk_key_field = live_chunk_key_field(&schema, path);
        Ok(SearchEngine {
            schema,
            chunk_key_field,
            index_path: path.to_path_buf(),
            index,
            index_writer: None,
            writer_closed: false,
            read_only: true,
            writer_heap_size: DEFAULT_WRITER_HEAP_SIZE,
            economy_indexing: false,
            index_reader,
            magic_dict: None,
            translation_dict: None,
            acronym_dict: None,
            term_work: crate::highlight_cache::WorkGate::default(),
            term_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(TERM_CACHE_ENTRIES).expect("cache size is non-zero"),
            )),
            bulk_indexing: false,
            #[cfg(feature = "semantic-integration")]
            semantic_runtime: RwLock::new(None),
            #[cfg(not(feature = "semantic-integration"))]
            semantic_runtime: (),
            #[cfg(feature = "semantic-integration")]
            semantic_resolver: Mutex::default(),
            #[cfg(feature = "semantic-integration")]
            semantic_set_view: Mutex::default(),
            #[cfg(feature = "semantic-integration")]
            semantic_sessions: SemanticSessionCache::default(),
            #[cfg(feature = "semantic-integration")]
            semantic_highlights: Default::default(),
            page_cache: PageCache::default(),
            highlight_cache: HighlightCache::default(),
            grouped_cache: GroupCache::default(),
            dictionary_epoch: book_order::next_epoch(),
            memory_epoch: std::sync::atomic::AtomicU64::default(),
            lexical_cancellations: Arc::default(),
            book_numbers: Mutex::new(None),
            numbers_stale: AtomicBool::new(true),
            book_order: RwLock::new(BookOrderState::read(path)),
            order_tables: Mutex::new(None),
        })
    }

    /// Adds `file_path` at `catalogue_order`/`generation_order` to the order mapping, keeping
    /// every other book's place: how tests written against catalogue-composed ids place
    /// their books.
    #[cfg(test)]
    pub(crate) fn place_book_for_tests(
        &self,
        file_path: &str,
        catalogue_order: u32,
        generation_order: u32,
    ) {
        let places = Arc::clone(
            &self
                .book_order
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .places,
        );
        let place = BookPlace {
            catalogue_order,
            generation_order,
        };
        if places.get(file_path) == Some(&place) {
            return;
        }
        let mut entries: Vec<BookOrderEntry> = places
            .iter()
            .filter(|entry| entry.0.as_str() != file_path)
            .map(|(path, place)| BookOrderEntry {
                file_path: path.clone(),
                catalogue_order: place.catalogue_order,
                generation_order: place.generation_order,
            })
            .collect();
        entries.push(BookOrderEntry {
            file_path: file_path.to_owned(),
            catalogue_order,
            generation_order,
        });
        self.set_book_order(entries).unwrap();
    }

    /// [`Self::add_text_book`] for a test written against catalogue-composed ids: the book
    /// gets number `catalogue_order + 1`, and is placed as [`Self::place_book_for_tests`]
    /// does.
    #[cfg(test)]
    pub(crate) fn add_text_book_placed(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        catalogue_order: u32,
        generation_order: u32,
        text: String,
        extra_facets: Option<Vec<String>>,
        text_storage: TextStorage,
        stored_lines: Option<Vec<u32>>,
    ) -> Result<u32> {
        self.force_book_number_for_tests(&file_path, catalogue_order + 1);
        self.place_book_for_tests(&file_path, catalogue_order, generation_order);
        self.add_text_book(
            title,
            topics,
            file_path,
            text,
            extra_facets,
            text_storage,
            stored_lines,
            None,
        )
    }

    /// [`Self::add_text_book_bytes`] with the book placed as [`Self::place_book_for_tests`]
    /// does.
    #[cfg(test)]
    pub(crate) fn add_text_book_bytes_placed(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        catalogue_order: u32,
        generation_order: u32,
        text: Vec<u8>,
        extra_facets: Option<Vec<String>>,
        text_storage: TextStorage,
        stored_lines: Option<Vec<u32>>,
    ) -> Result<u32> {
        self.force_book_number_for_tests(&file_path, catalogue_order + 1);
        self.place_book_for_tests(&file_path, catalogue_order, generation_order);
        self.add_text_book_bytes(
            title,
            topics,
            file_path,
            text,
            extra_facets,
            text_storage,
            stored_lines,
            None,
        )
    }

    /// [`Self::add_pdf_book`] with the book placed as [`Self::place_book_for_tests`] does.
    #[cfg(test)]
    pub(crate) fn add_pdf_book_placed(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        catalogue_order: u32,
        generation_order: u32,
        pages: Vec<PdfPageInput>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<u32> {
        self.force_book_number_for_tests(&file_path, catalogue_order + 1);
        self.place_book_for_tests(&file_path, catalogue_order, generation_order);
        self.add_pdf_book(title, topics, file_path, pages, extra_facets)
    }

    /// [`Self::add_document`] for a test written against a caller-composed `id`, which the
    /// document gets: the id's high half becomes the book's number, and a `generation_order`
    /// places the book (catalogue = that number). An id below `1 << 32` has no book number,
    /// as before numbers existed: it is written as it is, sorting before every book, and
    /// cannot carry a generation.
    #[cfg(test)]
    pub(crate) fn add_document_at(
        &mut self,
        id: u64,
        title: &str,
        reference: &str,
        topics: &str,
        text: &str,
        segment: u64,
        is_pdf: bool,
        file_path: &str,
        section_id: Option<u64>,
        generation_order: Option<u32>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<()> {
        self.write_document_at(
            false,
            id,
            title,
            reference,
            topics,
            text,
            segment,
            is_pdf,
            file_path,
            section_id,
            generation_order,
            extra_facets,
        )
    }

    /// [`Self::upsert_document`] by the id [`Self::add_document_at`] gives.
    #[cfg(test)]
    pub(crate) fn upsert_document_at(
        &mut self,
        id: u64,
        title: &str,
        reference: &str,
        topics: &str,
        text: &str,
        segment: u64,
        is_pdf: bool,
        file_path: &str,
        section_id: Option<u64>,
        generation_order: Option<u32>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<()> {
        self.write_document_at(
            true,
            id,
            title,
            reference,
            topics,
            text,
            segment,
            is_pdf,
            file_path,
            section_id,
            generation_order,
            extra_facets,
        )
    }

    #[cfg(test)]
    fn write_document_at(
        &mut self,
        upsert: bool,
        id: u64,
        title: &str,
        reference: &str,
        topics: &str,
        text: &str,
        segment: u64,
        is_pdf: bool,
        file_path: &str,
        section_id: Option<u64>,
        generation_order: Option<u32>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<()> {
        let number = (id >> 32) as u32;
        if number == 0 {
            assert!(
                generation_order.is_none(),
                "a raw id below 1 << 32 has no book to place in a generation"
            );
            let document = Self::single_input(
                0,
                title,
                reference,
                topics,
                text,
                segment,
                is_pdf,
                file_path,
                section_id.map(|section| section as u32),
                extra_facets,
            );
            return self.write_inputs_with(
                vec![document],
                upsert,
                Ids::Raw(id),
                Stamping::LineZero,
            );
        }
        self.force_book_number_for_tests(file_path, number);
        if let Some(generation_order) = generation_order {
            self.place_book_for_tests(file_path, number, generation_order);
        }
        let line = |id: u64| ((id & 0xFFFF_FFFF) as u32).saturating_sub(1);
        let document = Self::single_input(
            line(id),
            title,
            reference,
            topics,
            text,
            segment,
            is_pdf,
            file_path,
            section_id.map(line),
            extra_facets,
        );
        self.write_inputs_with(vec![document], upsert, Ids::Composed, Stamping::LineZero)
    }

    /// A one-line document of `file_path` with exactly `id`, whatever the book's number: for
    /// tests of indexes whose ids the engine did not compose.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn add_raw_document_for_tests(&mut self, id: u64, file_path: &str, text: &str) {
        let document =
            Self::single_input(0, "t", "r", "/root", text, 0, false, file_path, None, None);
        self.write_inputs_with(vec![document], false, Ids::Raw(id), Stamping::LineZero)
            .unwrap();
    }

    /// Gives `file_path` book number `number`, as a test written against catalogue-composed
    /// ids expects it (`catalogue_order + 1`).
    #[cfg(test)]
    pub(crate) fn force_book_number_for_tests(&self, file_path: &str, number: u32) {
        let mut numbers = self
            .book_numbers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        numbers
            .get_or_insert_with(|| BookNumbers::new(&self.index_path))
            .force_for_tests(file_path, number);
    }

    /// The order searches rank by, for tests that build a line search themselves.
    #[cfg(test)]
    pub(crate) fn order_tables_for_tests(&self) -> (u64, Arc<OrderTables>) {
        self.order_tables().unwrap()
    }

    /// The number the engine gave `file_path`, for tests that compose expected ids.
    #[cfg(test)]
    pub(crate) fn book_number_for_tests(&self, file_path: &str) -> u32 {
        self.book_number(file_path).unwrap()
    }

    /// Forget every semantic session, so the next search computes its first page afresh.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn invalidate_semantic_sessions_for_bench(&self) {
        self.semantic_sessions.invalidate();
    }

    /// Forget the keys earlier searches recomputed, as a session that shares none would.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn forget_semantic_keys_for_bench(&self) {
        self.semantic_resolver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .forget_keys();
    }

    /// A passage highlight target's line text as the highlight reads it; `None` when no line
    /// of the book has that id.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn passage_text_for_bench(
        &self,
        file_path: &str,
        id: u64,
    ) -> Result<Option<(String, TextStatus)>> {
        let searcher = self.index_reader.searcher();
        let Some(address) = self.address_in_book(&searcher, file_path, id)? else {
            return Ok(None);
        };
        Ok(self
            .texts_at(&searcher, &[address])?
            .pop()
            .map(|(text, status)| (text.text, status)))
    }

    /// Plan unfiltered searches and the foundational books' query as the first search would;
    /// how long it took, in ms.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn plan_semantic_for_bench(&self) -> Result<f64> {
        let started = Instant::now();
        let open = self
            .semantic_engine()
            .context("no semantic session is open")?;
        let vectors_dir = open.vectors_dir.clone().context("not a vector set")?;
        let view = open
            .coordinator
            .vector_set_info()
            .and_then(|info| self.semantic_set_view(&vectors_dir, info.generation))
            .context("the vector set has no view")?;
        let mut resolver = crate::semantic_resolver::LiveResolver::for_chunking(
            self.index_reader.searcher(),
            self.chunk_key_field,
            &self.semantic_resolver,
            open.chunking.clone(),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let cancel = SearchCancellation::default();
        let mut filters = SidecarSearchFilters::default();
        resolver
            .plan(&filters, &view, &cancel)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        if PLAN_FOUNDATIONAL_QUERY {
            filters.facets = Some(vec![FOUNDATIONAL_FACET.to_string()]);
            resolver
                .plan(&filters, &view, &cancel)
                .map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        Ok(started.elapsed().as_secs_f64() * 1000.0)
    }

    /// One inference of `text` as a passage, through the session pool a query embeds with, in
    /// milliseconds: what a search's query embedding costs and waits for. `None` with no session.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn bench_embed_once(&self, text: &str) -> Option<f64> {
        let open = self.semantic_engine()?;
        let cancel = SearchCancellation::default();
        let started = Instant::now();
        open.coordinator.embed_passages(&[text], &cancel).ok()?;
        Some(started.elapsed().as_secs_f64() * 1000.0)
    }

    /// The lexical phase of [`Self::search_semantic`] alone, for benchmarks:
    /// `None` is Exact, `Some(d)` is Fuzzy at distance `d`.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn bench_semantic_lexical_phase(
        &self,
        query: &str,
        facets: &[String],
        window: u32,
        fuzzy_distance: Option<u8>,
    ) -> Result<BenchLexicalPhase> {
        let searcher = self.index_reader.searcher();
        let phase = match fuzzy_distance {
            None => self.semantic_exact_lexical_candidates(
                &searcher, query, facets, window, false, false, true,
            )?,
            Some(distance) => self.semantic_fuzzy_lexical_candidates(
                &searcher, query, facets, window, distance, false, false, true,
            )?,
        };
        Ok(BenchLexicalPhase {
            candidates: phase.candidates.len(),
            total_count: phase.total_count,
            truncated: phase.truncated,
            highlighted: phase.highlight.is_some(),
        })
    }

    /// Loads a `lexical.db` morphology lexicon for the approximate (`fuzzy`)
    /// search path. Returns `true` if the file opened and has the expected
    /// schema, `false` if it is missing or unusable — in which case the engine
    /// keeps its existing fuzzy behaviour (no error is surfaced, so the app can
    /// call this unconditionally at startup). Does **not** affect exact or
    /// advanced search.
    /// Dictionary bindings are asynchronous: loading does disk I/O, and even
    /// status queries can wait for an indexing operation to release the engine.
    pub fn set_magic_dictionary_path(&mut self, path: String) -> bool {
        self.dictionary_epoch = book_order::next_epoch();
        self.highlight_cache.clear();
        self.page_cache.clear();
        self.grouped_cache.clear();
        #[cfg(feature = "semantic-integration")]
        let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
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
    pub fn has_magic_dictionary(&self) -> bool {
        self.magic_dict.is_some()
    }

    /// טוען את מילון התרגום הארמי-עברי (ה-`dictionary.json` של האפליקציה)
    /// עבור אפשרות "תרגום ארמי" בחיפוש המתקדם. מחזיר `true` אם הקובץ נטען;
    /// `false` אם חסר/פגום — ואז האפשרות לא מרחיבה דבר (אין שגיאה כלפי
    /// האפליקציה, שיכולה לקרוא לזה ללא תנאי באתחול).
    pub fn set_translation_dictionary_path(&mut self, path: String) -> bool {
        self.dictionary_epoch = book_order::next_epoch();
        self.highlight_cache.clear();
        self.page_cache.clear();
        self.grouped_cache.clear();
        match TranslationLexicon::load(Path::new(&path)) {
            Ok(lexicon) => {
                debug!(
                    "translation dictionary loaded from {path} ({} headwords)",
                    lexicon.len()
                );
                self.translation_dict = Some(lexicon);
                true
            }
            Err(err) => {
                warn!("translation dictionary unavailable at {path}: {err:#}");
                self.translation_dict = None;
                false
            }
        }
    }

    /// האם מילון תרגום ארמי-עברי טעון כרגע.
    pub fn has_translation_dictionary(&self) -> bool {
        self.translation_dict.is_some()
    }

    /// טוען את מילון ראשי-התיבות (ה-`Acronyms.json` של האפליקציה) עבור
    /// אפשרות "ראשי תיבות" בחיפוש המתקדם. מחזיר `true` אם הקובץ נטען;
    /// `false` אם חסר/פגום — ואז האפשרות לא מרחיבה דבר (אין שגיאה כלפי
    /// האפליקציה, שיכולה לקרוא לזה ללא תנאי באתחול).
    pub fn set_acronyms_dictionary_path(&mut self, path: String) -> bool {
        self.dictionary_epoch = book_order::next_epoch();
        self.highlight_cache.clear();
        self.page_cache.clear();
        self.grouped_cache.clear();
        match AcronymLexicon::load(Path::new(&path)) {
            Ok(lexicon) => {
                debug!(
                    "acronyms dictionary loaded from {path} ({} acronyms)",
                    lexicon.len()
                );
                self.acronym_dict = Some(lexicon);
                true
            }
            Err(err) => {
                warn!("acronyms dictionary unavailable at {path}: {err:#}");
                self.acronym_dict = None;
                false
            }
        }
    }

    /// האם מילון ראשי-תיבות טעון כרגע.
    pub fn has_acronyms_dictionary(&self) -> bool {
        self.acronym_dict.is_some()
    }

    // ── Semantic sidecar API ────────────────────────────────────────────────

    /// Open a semantic session whose vectors are built on this device, and wire
    /// it to the already-open Tantivy engine. The sidecar owns semantic fusion;
    /// Tantivy stays owned here. When this crate was built without the optional
    /// semantic feature this is a no-op that returns an explicit `NotInBuild`
    /// status.
    ///
    /// **Development and testing scaffolding.** The application never builds
    /// the library's vectors; it opens the artifact the build machine made with
    /// [`Self::open_semantic_artifact`]. Kept, and not deprecated, because the
    /// test suites and existing application code call it. While an artifact is
    /// open this is refused.
    ///
    /// **The sidecar's vector store is in-memory** (check
    /// [`SemanticStatus::vectors_persisted`]): vectors live only for the
    /// lifetime of this session and must be rebuilt after a restart. Opening an
    /// engine re-reads the on-disk manifest and drops every book record whose
    /// vectors did not survive, so a *re-open* discards the session's semantic
    /// index. This method therefore does not re-open:
    ///
    /// - Called again with the same inputs it is a no-op returning the current
    ///   status, so a caller that configures defensively cannot lose an index.
    /// - Called with different inputs while a session is open it fails and says
    ///   which input changed. Switching model, text recipe or library root is an
    ///   explicit act: call [`Self::disable_semantic`] first and accept the
    ///   rebuild.
    ///
    /// A refusal is a [`SemanticError`]: `SessionConflict` while another session,
    /// or this one with other inputs, is open, and `InvalidInput` for a value the
    /// sidecar cannot serve. The model loads lazily, so a missing or unusable model,
    /// or ONNX Runtime, is not refused here but by the first
    /// [`Self::semantic_index_books`].
    pub fn configure_semantic(
        &mut self,
        config: SemanticConfigInput,
    ) -> Result<SemanticStatus, SemanticError> {
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "index was opened read-only",
            ));
        }
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            let requested = SemanticConfigKey::from_input(&config);
            let active = self
                .semantic_runtime
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner);
            match active.as_ref().map(|active| &active.source) {
                None => {}
                Some(SemanticSource::SelfBuilt(key)) if *key == requested => {
                    return Ok(self.semantic_status());
                }
                Some(SemanticSource::SelfBuilt(key)) => {
                    return Err(SemanticError::new(
                        SemanticErrorKind::SessionConflict,
                        format!(
                            "the semantic sidecar is already configured and {} changed; the \
                             vector store is in-memory, so re-opening would discard the vectors \
                             indexed in this session. Call disable_semantic() first if that is \
                             intended",
                            key.changed_fields(&requested)
                        ),
                    ));
                }
                Some(SemanticSource::Artifact(opened)) => {
                    return Err(SemanticError::new(
                        SemanticErrorKind::SessionConflict,
                        format!(
                            "a prebuilt semantic artifact is open ({}); configure_semantic would \
                             replace it with vectors built on this device. Call \
                             disable_semantic() first if that is intended",
                            opened.key.vectors_dir.display()
                        ),
                    ));
                }
            }

            // The sidecar validates the rest of the configuration when it opens:
            // pooling, text recipe and token cap. A blank quantization it would
            // record as given, leaving an index whose identity names no weights.
            if requested.model_quantization.trim().is_empty() {
                return Err(SemanticError::new(
                    SemanticErrorKind::InvalidInput,
                    "model_quantization is empty; it is part of the semantic index's \
                     identity, so name the precision of the model's weights, e.g. \
                     \"int8\" for the Meivin INT8 graph or \"fp32\" for the full-precision one",
                )
                .with_field("model_quantization"));
            }
            check_onnx_runtime_path(config.onnx_runtime_path.as_deref())?;
            let embedding_max_tokens = usize::try_from(requested.max_tokens).map_err(|_| {
                SemanticError::new(
                    SemanticErrorKind::InvalidInput,
                    format!(
                        "max_tokens {} does not fit this platform's address width",
                        requested.max_tokens
                    ),
                )
                .with_field("max_tokens")
            })?;

            // Each input lands on the one sidecar field that carries it. What stays
            // at `SemanticConfig::default()` is not the caller's to choose:
            // `vector_precision` is the store's format, `embedding_batch_size` is
            // throughput, and the rest of the chunking recipe is the root
            // `chunking.json` that both known models are built with.
            let mut semantic_config = SemanticConfig {
                root_dir: requested.root_dir.clone(),
                model_path: requested.model_path.clone(),
                embedding_model_id: requested.model_id.clone(),
                embedding_dim: requested.embedding_dim,
                pooling: requested.pooling.clone(),
                embedding_max_tokens,
                model_quantization: requested.model_quantization.clone(),
                // Where this machine keeps ONNX Runtime: a deployment fact the sidecar
                // keeps out of the manifest, and the first place it looks. `None` leaves
                // it to `OTZARIA_ONNX_RUNTIME` and then the file beside the graph.
                deployment: EmbeddingDeployment {
                    onnx_runtime: requested.onnx_runtime.clone(),
                    ..Default::default()
                },
                ..SemanticConfig::default()
            };
            // The text recipe is part of the chunking configuration rather than a
            // field of its own, because the chunker is what applies it and
            // `ChunkerConfig::identity` is what records it.
            semantic_config.chunking.embedding_text_version = requested.embedding_text_version;
            // Both dimensions must agree or the sidecar refuses the config, and
            // the store's path is derived from our root rather than the
            // sidecar's own default root.
            semantic_config.store.embedding_dim = requested.embedding_dim;
            semantic_config.store.db_path = requested.root_dir.join("vectors");

            let engine = SemanticEngine::open(semantic_config).map_err(|err| {
                semantic_errors::sidecar_error(
                    &err,
                    SidecarCall::Configure,
                    format!("failed to open semantic sidecar: {err}"),
                )
            })?;
            *self
                .semantic_runtime
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner) = Some(ConfiguredSemantic {
                coordinator: Arc::new(HybridCoordinator::new(Some(engine))),
                source: SemanticSource::SelfBuilt(requested),
            });
            Ok(self.semantic_status())
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = config;
            Ok(Self::semantic_disabled_status())
        }
    }

    /// Open the vector set installed on this device and serve semantic and hybrid
    /// search from it, with every result hydrated from this Tantivy index as
    /// [`Self::search_semantic`] always does. **This is the application's
    /// semantic path**: the library's vectors are built on the build machine,
    /// and the device embeds only the query. When this crate was built without
    /// the optional semantic feature this is a no-op that returns an explicit
    /// `NotInBuild` status.
    ///
    /// Opening recovers what an interrupted install left, opens the generation
    /// `CURRENT` names (or `PREVIOUS`, when it does not open), loads the model,
    /// and verifies the set against this installation, all of it and before a
    /// vector is read:
    ///
    /// | half | expected value, from | fixed by |
    /// | --- | --- | --- |
    /// | text | the line recipe this build indexes with, and the key function it computes keys with | a set built for this application's line recipe |
    /// | model | `model_identity_json`, and the model at `model_path` once loaded | installing the model the set was built with |
    /// | store | what this build can read | a build that reads the set's format |
    ///
    /// The set is not tied to one state of the index. Its vectors are addressed by
    /// the key of the text they were embedded from, and each hit is resolved, on
    /// every search, to the live lines that hold that text: lines added, deleted,
    /// moved or renumbered since the set was built leave every unchanged line's
    /// vector usable, and nothing goes stale.
    ///
    /// A mismatch is an error naming every field that disagreed, and nothing is
    /// left open. On success the session is read-only: `semantic_index_books`,
    /// `remove_semantic_books`, `reset_semantic_index` and `semantic_index_diff`
    /// are refused by name, and so is [`Self::configure_semantic`].
    ///
    /// - Called again with the same inputs it is a no-op returning the status.
    /// - Called while another session is open it fails: call
    ///   [`Self::disable_semantic`] first.
    ///
    /// Every refusal is a [`SemanticError`] whose kind says which of these it was,
    /// so the application can tell the user what to install: the vectors, the
    /// model or ONNX Runtime. The table on [`SemanticErrorKind`] has each kind, and
    /// `field` names the identity field that disagreed when the set was built for
    /// something else.
    ///
    /// `&self`, unlike [`Self::configure_semantic`]: opening loads the model and
    /// maps the set, which takes time, and a `&mut self` binding would hold the
    /// engine's write lock throughout, stalling every lexical search.
    pub fn open_semantic_artifact(
        &self,
        config: SemanticArtifactInput,
    ) -> Result<SemanticStatus, SemanticError> {
        // The sidecar's current open performs recovery and garbage collection.
        // A read-only vector session does not make that initial open write-free.
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "opening semantic artifacts can recover files on a read-only index",
            ));
        }
        self.open_semantic_artifact_impl(config)
    }

    /// Real-data benchmarks keep the lexical directory read-only but explicitly
    /// allow recovery/cleanup in their separate vector set. Keep this capability
    /// out of production builds; every lexical and semantic mutation guard stays
    /// active on the engine, including the public artifact-open guard.
    #[cfg(all(test, feature = "semantic-integration"))]
    pub(crate) fn open_semantic_artifact_for_bench(
        &self,
        config: SemanticArtifactInput,
    ) -> Result<SemanticStatus, SemanticError> {
        self.open_semantic_artifact_impl(config)
    }

    fn open_semantic_artifact_impl(
        &self,
        config: SemanticArtifactInput,
    ) -> Result<SemanticStatus, SemanticError> {
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            let model = parse_model_identity(&config.model_identity_json)?;
            check_onnx_runtime_path(config.onnx_runtime_path.as_deref())?;
            let scan_threads = match config.scan_threads {
                Some(0) => {
                    return Err(SemanticError::new(
                        SemanticErrorKind::InvalidInput,
                        "scan_threads is 0, so no thread would scan; pass None for the \
                         sidecar's default, or a number of threads from 1",
                    )
                    .with_field("scan_threads"))
                }
                threads => threads.and_then(|threads| NonZeroUsize::new(threads as usize)),
            };
            let key = SemanticArtifactKey {
                vectors_dir: PathBuf::from(&config.vectors_dir),
                model_path: PathBuf::from(&config.model_path),
                model,
                onnx_runtime: config.onnx_runtime_path.as_ref().map(PathBuf::from),
                scan_threads: config.scan_threads,
            };
            if let Some(refusal) = self.refuse_second_session(&key) {
                return refusal;
            }

            let refused = |err: &dyn std::fmt::Display| {
                format!(
                    "failed to open the semantic vectors at {}: {err}",
                    key.vectors_dir.display()
                )
            };
            let call = SidecarCall::OpenArtifact {
                vectors_dir: &key.vectors_dir,
                model_path: &key.model_path,
                onnx_runtime: key.onnx_runtime.as_deref(),
            };
            // The installation's own half first, by the checks the sidecar makes of it
            // before it reads the artifact: the sidecar reports a refusal of either side
            // in the same types, and settling this one first is what makes the kind of
            // whatever it refuses afterwards that of the artifact, the model or the
            // runtime. None of them reads the package's precision, which is found next.
            let family = LocalModel::of_family(key.model_path.clone(), &key.model, "");
            semantic_errors::check_local_model(&family, |err| refused(err))?;
            ensure_keyed_chunking(&key.model, &refused)?;
            let package = family_package(&key, call, &refused)?;
            let local = LocalModel {
                model_quantization: package.quantization,
                ..family
            };
            let index = OfficialSemanticIndex::open(OfficialIndexConfig {
                vectors_dir: key.vectors_dir.clone(),
                // The line recipe this build writes and recomputes keys under. A set keyed
                // under another would resolve nothing, so it is refused by that field.
                text: TextIdentity::with_line_text_version(LINE_TEXT_VERSION),
                model: local,
                // As for `configure_semantic`. No identity field reads it, so it makes no
                // set the wrong one.
                deployment: EmbeddingDeployment {
                    onnx_runtime: key.onnx_runtime.clone(),
                    ..Default::default()
                },
                scan_threads,
            })
            .map_err(|err| semantic_errors::sidecar_error(&err, call, refused(&err)))?;

            // The field the sidecar takes from the loaded package rather than from the
            // identity file. The artifact agreed with the package, so an identity file that
            // disagrees with it describes other weights, and is refused rather than quietly
            // outvoted.
            let loaded = &index.identity().model;
            if key.model.tokenizer_checksum != loaded.tokenizer_checksum {
                return Err(SemanticError::new(
                    SemanticErrorKind::ModelIdentityMismatch,
                    format!(
                        "model_identity_json declares tokenizer_checksum '{}', but the model \
                         at {} has '{}', as does the vector set: the identity file describes \
                         other weights",
                        key.model.tokenizer_checksum,
                        key.model_path.display(),
                        loaded.tokenizer_checksum
                    ),
                )
                .with_field("tokenizer_checksum"));
            }

            let mut active = self
                .semantic_runtime
                .write()
                .unwrap_or_else(PoisonError::into_inner);
            // Checked again under the lock: another call may have opened a session while
            // this one was loading.
            if let Some(existing) = active.as_ref() {
                let same = Self::is_artifact_session(existing, &key);
                drop(active);
                return if same {
                    Ok(self.semantic_status())
                } else {
                    Err(SemanticError::new(
                        SemanticErrorKind::SessionConflict,
                        format!(
                            "another semantic session was opened while these vectors were \
                             loading; call disable_semantic() first to open {}",
                            key.vectors_dir.display()
                        ),
                    ))
                };
            }
            *active = Some(ConfiguredSemantic {
                coordinator: Arc::new(HybridCoordinator::with_official_index(index)),
                source: SemanticSource::Artifact(OpenedArtifact { key }),
            });
            drop(active);
            Ok(self.semantic_status())
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = config;
            Ok(Self::semantic_disabled_status())
        }
    }

    /// Whether `active` is the artifact session `key` would open.
    #[cfg(feature = "semantic-integration")]
    fn is_artifact_session(active: &ConfiguredSemantic, key: &SemanticArtifactKey) -> bool {
        matches!(&active.source, SemanticSource::Artifact(opened) if opened.key == *key)
    }

    /// What [`Self::open_semantic_artifact`] answers while a session is already open:
    /// the status for a repeat of the same artifact, a `SessionConflict` for anything
    /// else, and `None` when nothing is open.
    #[cfg(feature = "semantic-integration")]
    fn refuse_second_session(
        &self,
        key: &SemanticArtifactKey,
    ) -> Option<Result<SemanticStatus, SemanticError>> {
        let active = self
            .semantic_runtime
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let refusal = match active.as_ref().map(|active| &active.source)? {
            SemanticSource::Artifact(opened) if opened.key == *key => None,
            SemanticSource::Artifact(opened) => Some(format!(
                "a semantic vector set is already open ({}) and {} changed; call \
                 disable_semantic() first to open {}",
                opened.key.vectors_dir.display(),
                opened.key.changed_fields(key),
                key.vectors_dir.display()
            )),
            SemanticSource::SelfBuilt(_) => Some(format!(
                "a semantic session with vectors built on this device is open \
                 (configure_semantic); call disable_semantic() first to open {}",
                key.vectors_dir.display()
            )),
        };
        // Released before the status is read, which takes the lock again.
        drop(active);
        Some(match refusal {
            None => Ok(self.semantic_status()),
            Some(refusal) => Err(SemanticError::new(
                SemanticErrorKind::SessionConflict,
                refusal,
            )),
        })
    }

    /// Remove the configured sidecar without touching its on-disk files.
    /// This is useful when an app switches library roots or wants lexical-only
    /// operation for the current session, and it is the explicit way to allow a
    /// subsequent [`Self::configure_semantic`] or [`Self::open_semantic_artifact`]
    /// with different inputs.
    ///
    /// For a session from [`Self::configure_semantic`] this drops its vectors,
    /// which are in memory only: configuring it again needs a full re-index. An
    /// opened artifact is only closed; its files are untouched.
    pub fn disable_semantic(&mut self) {
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            *self
                .semantic_runtime
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner) = None;
            self.forget_set_view();
        }
    }

    /// The open sidecar session, or `None` when semantic search has not been
    /// configured in this session. A clone of the handle, which is an `Arc`, so no
    /// lock is held while it is used.
    #[cfg(feature = "semantic-integration")]
    fn semantic_engine(&self) -> Option<OpenSession> {
        self.semantic_runtime
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|active| OpenSession {
                coordinator: Arc::clone(&active.coordinator),
                model_path: active.model_path().to_path_buf(),
                onnx_runtime: active.onnx_runtime().map(Path::to_path_buf),
                vectors_dir: match &active.source {
                    SemanticSource::Artifact(opened) => Some(opened.key.vectors_dir.clone()),
                    SemanticSource::SelfBuilt(_) => None,
                },
                chunking: match &active.source {
                    SemanticSource::Artifact(opened) => {
                        semantic_keys::keyed_chunking(opened.key.model.chunking_identity)
                            .expect("artifact recipe was checked before opening")
                    }
                    SemanticSource::SelfBuilt(_) => semantic_keys::production_chunking(),
                },
            })
    }

    /// Deliberately **not** `#[frb(sync)]`. Reading the status takes the
    /// sidecar's engine lock, which indexing holds for the duration of one
    /// book's embedding run; a synchronous binding would block the calling Dart
    /// isolate for that whole time. Progress polling is the expected caller, so
    /// it must not be able to freeze the UI.
    pub fn semantic_status(&self) -> SemanticStatus {
        #[cfg(feature = "semantic-integration")]
        {
            if let Some(session) = self.semantic_engine() {
                let status = session.coordinator.status();
                let available = status.available;
                // A session that needs a full re-index is not served. `Failed` and `Empty`
                // are a session built on this device's; an opened vector set serves its
                // index whatever is committed to it, since its hits are resolved against
                // the live index on every search.
                let state = if status.needs_full_reindex.is_some() {
                    SemanticState::NeedsReindex
                } else if available {
                    SemanticState::Ready
                } else if status.last_error.is_some() {
                    SemanticState::Failed
                } else {
                    SemanticState::Empty
                };
                // The sidecar keeps a session's failures as text, and a vector set reports
                // none, so this is the text of a session built on this device: a failed
                // model load or vector insert, or the note that an unusable manifest was
                // moved aside. Its typed error went to the call that failed; the text alone
                // is `Internal`.
                let error_kind = status
                    .last_error
                    .as_ref()
                    .map(|_| SemanticErrorKind::Internal);
                let last_error = status.last_error;
                let set = session.coordinator.vector_set_info();
                return SemanticStatus {
                    vectors_library_version: set.as_ref().map(|set| set.library_version),
                    vector_segments: set.as_ref().map_or(0, |set| set.segments.len() as u32),
                    needs_compaction: set.as_ref().is_some_and(|set| set.needs_compaction),
                    state,
                    enabled: true,
                    available,
                    model_loaded: status.model_loaded,
                    indexed_book_count: status.indexed_book_count,
                    vector_count: status.vector_count,
                    model_id: status.model_id,
                    embedding_dim: status.embedding_dim,
                    embedding_backend: status.embedding_backend,
                    vector_backend: status.vector_backend,
                    vectors_persisted: status.vectors_persisted,
                    needs_full_reindex: status.needs_full_reindex,
                    last_error,
                    error_kind,
                };
            }
            Self::semantic_not_configured_status()
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            Self::semantic_disabled_status()
        }
    }

    /// Install a release of the library's vectors into the set at `vectors_dir`, creating
    /// the set when there is none: a base replaces whatever the set holds, and a delta
    /// brings it from the library version it stands at to the next. The release must be
    /// of this installation — the line recipe this build indexes with, the model family
    /// `model_identity_json` describes, chunked as this build keys lines, and the store
    /// this build reads — and, given `published_manifest_sha256`, the one published.
    ///
    /// The set is locked throughout, and the new generation goes live in one flip: a
    /// release that is refused, cancelled through `cancellation`, or cut off by a crash
    /// leaves the set as it was. A segment compressed with zstd (`.zst`) is expanded into
    /// a file of the install's own in the set's `incoming/` folder first, beside a lock file
    /// the install holds until it returns, so it needs its expanded size free besides what
    /// the install needs; both are gone when this returns, installed or not. An open session
    /// on the same set is moved onto the new generation before this returns.
    ///
    /// One install or compaction of a set runs at a time. While another runs in this
    /// process, this one is refused before it reads anything; otherwise it waits up to a
    /// second for the set's lock before it is refused. Either way the refusal is
    /// `VectorsBusy` about `vectors_dir`: nothing was changed, an open session keeps
    /// serving, and the install can be tried again once the other has finished.
    ///
    /// Refusals are [`SemanticError`]s of the kinds in the table on
    /// [`SemanticErrorKind`]: `ArtifactNotPublished` for a manifest that is not the
    /// published one, `ArtifactIncompatible` for a release of another identity or a delta
    /// that does not follow the set, `ArtifactCorrupt` for a segment that is not the one
    /// its manifest describes, `InsufficientDiskSpace`, `VectorsBusy`, and
    /// `Cancelled`.
    ///
    /// `&self`: it touches the vector set only, and a `&mut self` binding would hold the
    /// engine's write lock while it copies a segment of hundreds of megabytes.
    pub fn install_semantic_vectors(
        &self,
        input: SemanticVectorsInstallInput,
        cancellation: &SemanticCancellationToken,
    ) -> Result<SemanticVectorsInstallReport, SemanticError> {
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "index was opened read-only",
            ));
        }
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            use otzaria_semantic_search::semantic::segment_set::{
                self, InstallExpectation, InstallSource,
            };
            let vectors_dir = PathBuf::from(&input.vectors_dir);
            let refused = |err: &dyn std::fmt::Display| {
                format!(
                    "failed to install the semantic vectors into {}: {err}",
                    vectors_dir.display()
                )
            };
            let model = parse_model_identity(&input.model_identity_json)?;
            ensure_keyed_chunking(&model, &refused)?;
            let downloaded = PathBuf::from(&input.segment_path);
            if !downloaded.is_file() {
                return Err(SemanticError::new(
                    SemanticErrorKind::InvalidInput,
                    refused(&format!(
                        "segment_path names no file: {}",
                        downloaded.display()
                    )),
                )
                .with_field("segment_path"));
            }
            // A manifest that does not read is a damaged release, whatever the set holds;
            // the sidecar's refusal of it would read as a set missing or damaged.
            let release = match serde_json::from_str::<
                otzaria_semantic_search::semantic::segment_set::ReleaseManifest,
            >(&input.manifest_json)
            {
                Ok(release) => release,
                Err(err) => {
                    return Err(SemanticError::new(
                        SemanticErrorKind::ArtifactCorrupt,
                        refused(&format!("the release manifest does not read: {err}")),
                    )
                    .with_field("manifest_json"))
                }
            };
            let cancel = &cancellation.flag;
            // Held until the session is on the new generation: nothing else installs into
            // or compacts this set meanwhile, in this process.
            let _work = VectorSetWork::take(&vectors_dir, "installing the vectors")?;
            let compressed = downloaded.extension().is_some_and(|ext| ext == "zst");
            // An expansion, removed when this is dropped — its segment, and then the lock
            // that kept it this install's: an install moves the segment into the set, and a
            // refused one leaves it where it was.
            let expanded = if compressed {
                Some(expand_segment(&downloaded, &vectors_dir, cancel, &refused)?)
            } else {
                None
            };
            let source = InstallSource {
                segment: expanded
                    .as_ref()
                    .map_or(downloaded.as_path(), |expansion| &expansion.segment),
                manifest_json: &input.manifest_json,
            };
            let expectation = InstallExpectation {
                identity: installation_identity(model, &release.identity.store),
                published_manifest_sha256: input.published_manifest_sha256,
            };
            let installed = waiting_for_set_lock(cancel, SET_LOCK_WAIT, || {
                segment_set::install_package(&vectors_dir, &source, &expectation, cancel)
            });
            drop(expanded);
            let report = installed.map_err(|err| {
                vector_set_error(&err, &vectors_dir, refused(&err), "installing the vectors")
            })?;
            self.reload_open_vectors(&vectors_dir)?;
            Ok(SemanticVectorsInstallReport {
                kind: package_kind(report.kind),
                library_version: report.library_version,
                generation: report.generation,
                segments: report.segments,
                slots_added: report.slots_added,
                tombstones_applied: report.tombstones_applied,
                duplicates_removed: report.duplicates_removed,
                foreign_unresolved: report.foreign_unresolved,
                bytes_on_disk: report.bytes_on_disk,
                needs_compaction: report.needs_compaction,
                already_applied: report.already_applied,
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = (input, cancellation);
            Err(Self::semantic_not_in_build())
        }
    }

    /// Merge the set at `vectors_dir` into one segment, when `policy` (or, with `None`, the
    /// sidecar's defaults, [`SemanticCompactionPolicy::defaults`]) asks for it; the report
    /// says whether it did, and why. Deltas pile up as the library is updated, and each one
    /// a search scans costs it time; the set wants compacting when
    /// [`SemanticVectorsInfo::needs_compaction`] says so.
    ///
    /// `live_library_version` is the library version the open index holds, which the
    /// application knows and the index does not. When it is the set's own, and the index
    /// has the `chunkKey` column, every record is moved onto the live line that holds its
    /// text, and records whose book no longer holds it are dropped; otherwise records are
    /// kept as they are, which costs nothing but the space.
    ///
    /// Locked and crash-safe as an install is, and cancellable: a cancelled or failed
    /// compaction leaves the set as it was. It refuses to start without
    /// `min_free_space_factor` times the output's size free, as `InsufficientDiskSpace`,
    /// and while another install or compaction of the set runs, as `VectorsBusy` about
    /// `vectors_dir`, waiting for the set's lock as an install does. An open session on the
    /// same set is moved onto the compacted generation.
    pub fn compact_semantic_vectors(
        &self,
        vectors_dir: String,
        live_library_version: Option<u32>,
        policy: Option<SemanticCompactionPolicy>,
        cancellation: &SemanticCancellationToken,
    ) -> Result<SemanticCompactionReport, SemanticError> {
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "index was opened read-only",
            ));
        }
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            use crate::semantic_resolver::{LiveKeys, LiveResolver};
            use otzaria_semantic_search::semantic::resolve::LiveKeySource;
            use otzaria_semantic_search::semantic::segment_set;
            let vectors_dir = PathBuf::from(&vectors_dir);
            let failed = |err: &SemanticSearchError| {
                vector_set_error(
                    err,
                    &vectors_dir,
                    format!(
                        "failed to compact the semantic vectors at {}: {err}",
                        vectors_dir.display()
                    ),
                    "compacting the vectors",
                )
            };
            let policy =
                compaction_policy(policy.unwrap_or_else(SemanticCompactionPolicy::defaults))?;
            // As an install holds it, and for as long.
            let _work = VectorSetWork::take(&vectors_dir, "compacting the vectors")?;
            let chunking = segment_set::info(&vectors_dir)
                .map_err(|err| failed(&err))?
                .and_then(|info| {
                    semantic_keys::keyed_chunking(info.identity.model.chunking_identity)
                });
            let resolver = LiveResolver::for_chunking(
                self.index_reader.searcher(),
                self.chunk_key_field,
                &self.semantic_resolver,
                chunking
                    .clone()
                    .unwrap_or_else(semantic_keys::production_chunking),
            )
            .map_err(|err| failed(&err.into()))?;
            let live = chunking.and_then(|_| {
                live_library_version.and_then(|version| LiveKeys::new(&resolver, version))
            });
            let report = waiting_for_set_lock(&cancellation.flag, SET_LOCK_WAIT, || {
                segment_set::compact(
                    &vectors_dir,
                    &policy,
                    live.as_ref().map(|live| live as &dyn LiveKeySource),
                    &cancellation.flag,
                )
            })
            .map_err(|err| failed(&err))?;
            if report.compacted {
                self.reload_open_vectors(&vectors_dir)?;
            }
            Ok(SemanticCompactionReport {
                compacted: report.compacted,
                reason: report.reason,
                generation: report.generation,
                bytes_before: report.bytes_before,
                bytes_after: report.bytes_after,
                slots_before: report.slots_before,
                slots_after: report.slots_after,
                records_pruned: report.records_pruned,
                hints_refreshed: report.hints_refreshed,
                elapsed_ms: report.elapsed_ms,
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = (vectors_dir, live_library_version, policy, cancellation);
            Err(Self::semantic_not_in_build())
        }
    }

    /// What is installed at `vectors_dir`, from its small files alone: nothing is opened,
    /// mapped or cleaned up, so it is cheap enough to ask before every download. Nothing
    /// installed is not an error: [`SemanticVectorsInfo::present`] is `false` then.
    /// `ArtifactCorrupt` when what is there does not read.
    pub fn semantic_vectors_info(
        &self,
        vectors_dir: String,
    ) -> Result<SemanticVectorsInfo, SemanticError> {
        #[cfg(feature = "semantic-integration")]
        {
            let vectors_dir = PathBuf::from(&vectors_dir);
            let info = otzaria_semantic_search::semantic::segment_set::info(&vectors_dir).map_err(
                |err| {
                    vector_set_error(
                        &err,
                        &vectors_dir,
                        format!(
                            "failed to read the semantic vectors at {}: {err}",
                            vectors_dir.display()
                        ),
                        "reading the vectors",
                    )
                },
            )?;
            Ok(vectors_info(info))
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = vectors_dir;
            Err(Self::semantic_not_in_build())
        }
    }

    /// Read every block of every segment of the set at `vectors_dir` and check it against
    /// its checksum: the check opening leaves out, to run on demand, such as after a crash
    /// or before reporting a problem. It reads the whole set, so it takes the time a read of
    /// that many bytes takes, and stops at a cancel.
    ///
    /// A damaged segment is marked so that every later open refuses it, and this returns
    /// `ArtifactCorrupt`: install the release again, which repairs the set — downloading it
    /// again if it is gone. On a FAT or exFAT drive on Windows a repair under the same
    /// segment fails while a session holds the set open, since a mapped file cannot be
    /// replaced there: close the session (`disable_semantic`) before installing it. An open
    /// session keeps what it has mapped until it is closed. `ArtifactMissing` when nothing is
    /// installed there.
    ///
    /// An install that replaced bytes the verification had read, while it read them, is
    /// not damage: nothing is condemned, and this returns `VectorsBusy` about `vectors_dir`
    /// — verify again once the install has finished. A cancelled verification records
    /// nothing.
    pub fn verify_semantic_vectors(
        &self,
        vectors_dir: String,
        cancellation: &SemanticCancellationToken,
    ) -> Result<SemanticVectorsVerification, SemanticError> {
        // Scrubbing can persist a damaged-segment marker. It is a sidecar write
        // even though successful verification only reads vector data.
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "index was opened read-only",
            ));
        }
        #[cfg(feature = "semantic-integration")]
        {
            let vectors_dir = PathBuf::from(&vectors_dir);
            let report = otzaria_semantic_search::semantic::segment_set::scrub(
                &vectors_dir,
                &cancellation.flag,
            )
            .map_err(|err| {
                vector_set_error(
                    &err,
                    &vectors_dir,
                    format!(
                        "the semantic vectors at {} did not verify: {err}",
                        vectors_dir.display()
                    ),
                    "verifying the vectors",
                )
            })?;
            verification_of(&report, &vectors_dir)
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = (vectors_dir, cancellation);
            Err(Self::semantic_not_in_build())
        }
    }

    /// How much of the open index the set at `vectors_dir` covers: the live lines the
    /// recipe embeds, and how many of them the set holds a vector for. From the `chunkKey`
    /// column, one pass over it; on an index without the column, every book's keys are
    /// recomputed from its stored text, which reads the whole store. Cancellable.
    ///
    /// A line is covered when any slot of the set holds its text's key, including a slot a
    /// later delta deleted, or an alias shares a neighbour's vector with it; a release
    /// deletes only texts no line of its library holds, so that counts a line of an index
    /// older than the set at most.
    pub fn semantic_coverage(
        &self,
        vectors_dir: String,
        cancellation: &SemanticCancellationToken,
    ) -> Result<SemanticCoverage, SemanticError> {
        #[cfg(feature = "semantic-integration")]
        {
            use crate::semantic_resolver::LiveResolver;
            use otzaria_semantic_search::semantic::oxv::reader::Segment;
            use otzaria_semantic_search::semantic::segment_set;
            let vectors_dir = PathBuf::from(&vectors_dir);
            let cancel = &cancellation.flag;
            let failed = |err: &SemanticSearchError| {
                vector_set_error(
                    err,
                    &vectors_dir,
                    format!(
                        "failed to count what the semantic vectors at {} cover: {err}",
                        vectors_dir.display()
                    ),
                    "counting the coverage",
                )
            };
            let Some(info) = segment_set::info(&vectors_dir).map_err(|err| failed(&err))? else {
                return Err(SemanticError::new(
                    SemanticErrorKind::ArtifactMissing,
                    format!(
                        "no semantic vectors are installed at {}",
                        vectors_dir.display()
                    ),
                )
                .with_field("vectors_dir"));
            };
            let mut keys = Vec::new();
            for segment in &info.segments {
                if cancel.is_cancelled() {
                    return Err(failed(&SemanticSearchError::Cancelled));
                }
                // Where the set keeps its segments, as the sidecar lays it out.
                let path = vectors_dir
                    .join("segments")
                    .join(format!("{}.oxv", segment.id));
                let opened = Segment::open(&path).map_err(|err| failed(&err.into()))?;
                keys.extend((0..opened.slot_count()).map(|slot| opened.key(slot).column_value()));
                // A short line that shares a neighbour's vector is covered by it.
                keys.extend(
                    opened
                        .occurrences()
                        .filter(|row| row.is_alias())
                        .map(|row| row.anchor.column_value()),
                );
            }
            keys.sort_unstable();
            keys.dedup();
            ensure_keyed_chunking(&info.identity.model, &|err| err.to_string())?;
            let coverage = LiveResolver::for_chunking(
                self.index_reader.searcher(),
                self.chunk_key_field,
                &self.semantic_resolver,
                semantic_keys::keyed_chunking(info.identity.model.chunking_identity)
                    .expect("checked recipe"),
            )
            .and_then(|resolver| resolver.coverage(&keys, cancel))
            .map_err(|err| failed(&err.into()))?;
            Ok(SemanticCoverage {
                live_keyed_lines: coverage.keyed_lines,
                covered_lines: coverage.covered_lines,
                books_live: coverage.books_live,
                books_covered: coverage.books_covered,
                vectors_library_version: info.library_version,
                ratio: if coverage.keyed_lines == 0 {
                    0.0
                } else {
                    coverage.covered_lines as f64 / coverage.keyed_lines as f64
                },
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = (vectors_dir, cancellation);
            Err(Self::semantic_not_in_build())
        }
    }

    /// Move the open session onto the generation an install or a compaction made live, when
    /// it serves the set at `vectors_dir`; nothing otherwise.
    #[cfg(feature = "semantic-integration")]
    fn reload_open_vectors(&self, vectors_dir: &Path) -> Result<(), SemanticError> {
        let coordinator = {
            let active = self
                .semantic_runtime
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            match active.as_ref() {
                Some(ConfiguredSemantic {
                    coordinator,
                    source: SemanticSource::Artifact(opened),
                }) if same_directory(&opened.key.vectors_dir, vectors_dir) => {
                    Arc::clone(coordinator)
                }
                _ => return Ok(()),
            }
        };
        // The view of the generation the session leaves, which on Windows would keep its
        // segments from being removed.
        self.forget_set_view();
        coordinator
            .reload_semantic_vectors()
            .map(drop)
            .map_err(|err| {
                vector_set_error(
                    &err,
                    vectors_dir,
                    format!(
                        "the semantic vectors at {} changed, and the open session could not move \
                     onto them, so it serves the generation it had: {err}",
                        vectors_dir.display()
                    ),
                    "reloading the vectors",
                )
            })
    }

    /// The view a search plans its scan by, of the set at `vectors_dir` as its
    /// generation `generation` is: the one kept, or one opened now. `None` when the set has
    /// moved on from that generation — the session follows it, and a later search plans
    /// against it — or its segments do not open, which the session's own open would have
    /// refused; the search then scans the admitted books alone.
    #[cfg(feature = "semantic-integration")]
    fn semantic_set_view(
        &self,
        vectors_dir: &Path,
        generation: u64,
    ) -> Option<Arc<crate::semantic_moves::SetView>> {
        let mut kept = self
            .semantic_set_view
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(view) = kept.as_ref() {
            if view.generation() == generation && same_directory(view.dir(), vectors_dir) {
                return Some(Arc::clone(view));
            }
        }
        *kept = match crate::semantic_moves::SetView::open(vectors_dir, generation) {
            Ok(view) => view.map(Arc::new),
            Err(err) => {
                warn!(
                    "the vector set at {} could not be read to plan searches, which go \
                     unplanned: {err}",
                    vectors_dir.display()
                );
                None
            }
        };
        kept.clone()
    }

    /// Let go of the kept view, and the plans made with it: the session moved to another
    /// generation, or closed.
    #[cfg(feature = "semantic-integration")]
    fn forget_set_view(&self) {
        *self
            .semantic_set_view
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = None;
        self.semantic_resolver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .forget_plans();
    }

    #[cfg(not(feature = "semantic-integration"))]
    fn semantic_not_in_build() -> SemanticError {
        SemanticError::new(
            SemanticErrorKind::FeatureNotInBuild,
            "semantic support is not compiled into this build",
        )
    }

    /// Index or replace semantic vectors for complete books. The caller should
    /// use the same fingerprint it uses in `semantic_index_diff`; line ids must
    /// be the global Tantivy document ids so semantic-only results can hydrate.
    ///
    /// **Development and testing scaffolding**, for a session from
    /// [`Self::configure_semantic`]: this embeds books on the device, which the
    /// application never does for the library. On an artifact opened with
    /// [`Self::open_semantic_artifact`] it is refused as read-only.
    ///
    /// Takes `&self` on purpose. It mutates only the sidecar, which serializes
    /// indexing behind its own mutex and releases the engine lock between
    /// books. Declaring `&mut self` would make flutter_rust_bridge take a write
    /// lock on the whole engine for the entire run, blocking every concurrent
    /// *lexical* search for as long as the library takes to embed.
    ///
    /// The first call with something to embed loads the model, so this is where
    /// a session from [`Self::configure_semantic`] meets a missing or unusable
    /// model, tokenizer or ONNX Runtime, each as its own [`SemanticErrorKind`];
    /// `ReindexRequired` when the session holds vectors from another
    /// configuration, and `ReadOnlySession` on an opened artifact.
    pub fn semantic_index_books(
        &self,
        books: Vec<SemanticBookInput>,
    ) -> Result<SemanticIndexingSummary, SemanticError> {
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "index was opened read-only",
            ));
        }
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            let Some(session) = self.semantic_engine() else {
                return Ok(SemanticIndexingSummary {
                    enabled: false,
                    books_indexed: 0,
                    books_skipped: 0,
                    books_empty: 0,
                    chunks_written: 0,
                });
            };
            let sidecar_books: Vec<SidecarBookForIndexing> = books
                .into_iter()
                .map(|book| SidecarBookForIndexing {
                    source_book_key: book.source_book_key,
                    title: book.title,
                    content_fingerprint: book.content_fingerprint,
                    is_pdf: book.is_pdf,
                    topics: book.topics,
                    extra_facets: book.extra_facets,
                    lines: book
                        .lines
                        .into_iter()
                        .map(|line| SidecarBookLine {
                            line_id: line.line_id,
                            section_id: line.section_id,
                            text: line.text,
                            line_hash: line.line_hash,
                            reference: line.reference,
                            segment: line.segment,
                        })
                        .collect(),
                })
                .collect();
            let summary = session
                .coordinator
                .index_books(&sidecar_books)
                .map_err(|err| {
                    semantic_errors::sidecar_error(
                        &err,
                        session.call(),
                        format!("semantic indexing failed: {err}"),
                    )
                })?;
            let summary = summary.unwrap_or_default();
            Ok(SemanticIndexingSummary {
                enabled: true,
                books_indexed: summary.books_indexed,
                books_skipped: summary.books_skipped,
                books_empty: summary.books_empty,
                chunks_written: summary.chunks_written,
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = books;
            Ok(SemanticIndexingSummary {
                enabled: false,
                books_indexed: 0,
                books_skipped: 0,
                books_empty: 0,
                chunks_written: 0,
            })
        }
    }

    /// Compare the semantic manifest with the book fingerprints stored in the
    /// lexical index. A `contentHash` of zero is deliberately surfaced as
    /// `unverifiable_books` rather than treated as an up-to-date PDF.
    ///
    /// **Development and testing scaffolding**, as [`Self::semantic_index_books`]
    /// is: it answers which books this device should embed. On an opened
    /// artifact it is refused as read-only, since nothing there is re-indexed.
    pub fn semantic_index_diff(&self) -> Result<SemanticIndexDiff, SemanticError> {
        #[cfg(feature = "semantic-integration")]
        {
            let Some(session) = self.semantic_engine() else {
                return Ok(Self::semantic_disabled_diff());
            };
            // Raw lexical hashes, each as the sidecar's own conversion reads one: zero is
            // "no fingerprint", so a PDF lands in `unverifiable_books` and never passes
            // for up to date.
            let fingerprints = self
                .get_book_fingerprints()?
                .into_iter()
                .map(|(book, hash)| (book, SidecarContentFingerprint::from_lexical_hash(hash)))
                .collect();
            // The sidecar now separates "the semantic path is off" from "the comparison
            // itself failed": the first is `Ok(None)`, the second an error. Collapsing
            // them would report a broken manifest as a disabled feature, and the app
            // would offer indexing as the fix.
            let diff = session
                .coordinator
                .semantic_index_diff(&fingerprints)
                .map_err(|err| {
                    semantic_errors::sidecar_error(
                        &err,
                        session.call(),
                        format!("semantic index diff failed: {err}"),
                    )
                })?;
            let Some(diff) = diff else {
                return Ok(Self::semantic_disabled_diff());
            };
            Ok(SemanticIndexDiff {
                enabled: true,
                new_books: diff.new_books,
                changed_books: diff.changed_books,
                unverifiable_books: diff.unverifiable_books,
                removed_books: diff.removed_books,
                model_mismatch: diff.model_mismatch,
                chunking_mismatch: diff.chunking_mismatch,
                normalization_mismatch: diff.normalization_mismatch,
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            Ok(Self::semantic_disabled_diff())
        }
    }

    /// Remove vector records for books previously reported as `removed_books`.
    /// This never deletes lexical Tantivy documents.
    ///
    /// **Development and testing scaffolding**, as [`Self::semantic_index_books`]
    /// is; refused as read-only on an opened artifact.
    ///
    /// `&self` for the same reason as [`Self::semantic_index_books`].
    pub fn remove_semantic_books(
        &self,
        source_book_keys: Vec<String>,
    ) -> Result<SemanticRemoveResult, SemanticError> {
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "index was opened read-only",
            ));
        }
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            let Some(session) = self.semantic_engine() else {
                return Ok(SemanticRemoveResult {
                    enabled: false,
                    vectors_removed: 0,
                });
            };
            let removed = session
                .coordinator
                .remove_semantic_books(&source_book_keys)
                .map_err(|err| {
                    semantic_errors::sidecar_error(
                        &err,
                        session.call(),
                        format!("semantic remove failed: {err}"),
                    )
                })?
                .unwrap_or(0);
            Ok(SemanticRemoveResult {
                enabled: true,
                vectors_removed: removed,
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            let _ = source_book_keys;
            Ok(SemanticRemoveResult {
                enabled: false,
                vectors_removed: 0,
            })
        }
    }

    /// Discard all sidecar vectors and manifest book entries. Lexical Tantivy
    /// documents are untouched, so a full semantic rebuild can follow safely.
    ///
    /// **Development and testing scaffolding**, as [`Self::semantic_index_books`]
    /// is; refused as read-only on an opened artifact.
    ///
    /// `&self` for the same reason as [`Self::semantic_index_books`].
    pub fn reset_semantic_index(&self) -> Result<SemanticResetResult, SemanticError> {
        if self.read_only {
            return Err(SemanticError::new(
                SemanticErrorKind::ReadOnlySession,
                "index was opened read-only",
            ));
        }
        #[cfg(feature = "semantic-integration")]
        {
            // Whatever this changed, no page a search computed before it is served after it.
            let _sessions = crate::semantic_session::InvalidateOnDrop(&self.semantic_sessions);
            let Some(session) = self.semantic_engine() else {
                return Ok(SemanticResetResult {
                    enabled: false,
                    vectors_removed: 0,
                });
            };
            let removed = session
                .coordinator
                .reset_semantic_index()
                .map_err(|err| {
                    semantic_errors::sidecar_error(
                        &err,
                        session.call(),
                        format!("semantic reset failed: {err}"),
                    )
                })?
                .unwrap_or(0);
            Ok(SemanticResetResult {
                enabled: true,
                vectors_removed: removed,
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            Ok(SemanticResetResult {
                enabled: false,
                vectors_removed: 0,
            })
        }
    }

    /// Search through the sidecar. Tantivy supplies scored lexical candidates, beside the
    /// sidecar's semantic half rather than before it; the sidecar's coordinator alone performs
    /// hybrid fusion/grouping. Semantic-only items are hydrated from Tantivy before crossing
    /// FFI. The same for an artifact opened with [`Self::open_semantic_artifact`] and a
    /// development session.
    ///
    /// Pages continue one another: a search's results are kept in the order shown, so each
    /// line and group appears once and a page asked again is the same page; a commit, a
    /// library or semantic change, ten idle minutes or eviction start afresh. A continuation
    /// whose session was lost returns the first page with `session_restarted`, so the caller
    /// replaces its displayed results. A changed page size also starts a new session.
    /// `has_more` says whether a page follows; the counts describe the last candidate window.
    ///
    /// Quoted words are looked up verbatim, with adjacency inside each quoted phrase.
    /// A single quotation covering the whole query uses `Exact`; words outside a quotation
    /// keep the requested lexical mode. An acronym's gershayim (רמב"ם) quotes nothing.
    ///
    /// A semantic path that cannot serve is not an error here: the response falls
    /// back to lexical results and says why, in `fallback_reason` and, as a value
    /// to branch on, `fallback_kind`. What fails the call is the lexical half
    /// failing, which is an `Internal` [`SemanticError`].
    ///
    /// `ranking` replaces, for this search, every parameter hybrid ranking runs on (see
    /// [`SemanticRankingOptions`], whose defaults are unmeasured). `None` ranks by
    /// [`SemanticRankingOptions::defaults`]. An option outside its range is refused before the
    /// search runs, as `InvalidInput` naming it, whether or not a session is open to rank by
    /// it: a build without semantic support ignores the options.
    ///
    /// `cancellation` abandons the search: once it is cancelled, the next look at it
    /// ends the search with a `Cancelled` [`SemanticError`], never with lexical results
    /// in its place. The search looks before its lexical phase, hands the token to the
    /// sidecar, which looks throughout the semantic half (before and after it embeds the
    /// query, every 1,024 records of the vector scan, before and after fusion), and looks
    /// again before it hydrates the sidecar's results and before it paints the page; a
    /// lexical fallback is looked at before it runs and once its page is ready. Required,
    /// since flutter_rust_bridge 2.13 cannot pass an optional borrowed opaque type: an
    /// application with nothing to cancel passes a fresh token, which changes nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn search_semantic(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        lexical_mode: SemanticLexicalMode,
        fuzzy_max_distance: u8,
        retrieval_mode: SemanticRetrievalMode,
        grouping: Option<SemanticGroupingMode>,
        match_nikud: bool,
        match_taamim: bool,
        ranking: Option<SemanticRankingOptions>,
        cancellation: &SemanticCancellationToken,
    ) -> Result<SemanticSearchResponse, SemanticError> {
        let started = Instant::now();
        let cancel = &cancellation.flag;
        // Before anything, the sidecar's own first look included: a search cancelled before
        // it starts costs nothing.
        search_cancellation::look(cancel, SearchCheckpoint::Start)?;
        #[cfg(feature = "semantic-integration")]
        {
            // By the rules the coordinator checks it by, but before the lexical phase rather
            // than after it, and whether or not there is a session to rank: an option out of
            // range is a mistake in the call, and a fallback would hide it.
            let ranking =
                ranking_profile(&ranking.unwrap_or_else(SemanticRankingOptions::defaults))?;
            // Only a quotation covering the whole query is one verbatim lookup. Partial
            // quotations constrain their own words without joining the outside words to them.
            let lexical_mode = if self.whole_query_quoted(&query)? {
                SemanticLexicalMode::Exact
            } else {
                lexical_mode
            };
            let Some(open) = self.semantic_engine() else {
                return self.semantic_lexical_fallback_response(
                    &query,
                    &facets,
                    limit,
                    offset,
                    lexical_mode,
                    fuzzy_max_distance,
                    retrieval_mode,
                    grouping,
                    match_nikud,
                    match_taamim,
                    SemanticError::new(
                        SemanticErrorKind::NotConfigured,
                        "semantic sidecar has not been configured",
                    ),
                    started.elapsed().as_millis() as u64,
                    cancel,
                );
            };
            let request = SemanticRequest {
                query: &query,
                facets: &facets,
                lexical_mode,
                fuzzy_max_distance,
                retrieval_mode,
                grouping,
                match_nikud,
                match_taamim,
                ranking: &ranking,
            };

            let searcher = self.index_reader.searcher();
            let key = request.session_key(
                &searcher,
                open.coordinator
                    .vector_set_info()
                    .map(|info| info.generation),
                self.semantic_sessions.epoch(),
                limit,
            );
            // A search whose semantic half failed is searched again, but its pages still page.
            let kept = self.semantic_sessions.get(&key).filter(|kept| {
                offset > 0
                    || kept
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .state
                        .fallback_reason
                        .is_none()
            });
            let session_restarted = offset > 0 && kept.is_none();
            let offset = if session_restarted { 0 } else { offset };
            let shared = kept.unwrap_or_else(|| {
                Arc::new(Mutex::new(SemanticSearchSession::new(
                    searcher.clone(),
                    SessionState {
                        library_generation: key.library_generation,
                        ..SessionState::default()
                    },
                )))
            });
            let mut guard = shared.lock().unwrap_or_else(PoisonError::into_inner);
            let session = &mut *guard;
            // Same segments in the same order: the current searcher, whose generation the
            // resolver's caches follow, reads every address the session holds alike.
            session.searcher = searcher;

            // One past the page, so that whether another page follows is known.
            let wanted = (offset as usize)
                .saturating_add(limit as usize)
                .saturating_add(1);
            let mut window = session.window;
            let mut exhausted = session.exhausted;
            let mut additions = if session.len() < wanted && !exhausted {
                session.reopen()
            } else {
                Additions::default()
            };
            let mut expanded: Option<Expanded> = None;
            while session.len_with(&additions) < wanted && !exhausted {
                // Doubling: a session asked for page after page fuses a logarithmic number
                // of windows, and the first page's leaves room for the second.
                let requested_window = u64::from(offset)
                    .saturating_add(u64::from(limit))
                    .saturating_mul(2)
                    .saturating_add(1)
                    .max(u64::from(window).saturating_mul(2));
                let capped = requested_window > u64::from(MAX_SEMANTIC_CANDIDATE_WINDOW);
                window = requested_window.min(u64::from(MAX_SEMANTIC_CANDIDATE_WINDOW)) as u32;
                let first = session.window == 0 && expanded.is_none();
                let step = self.expand_semantic_session(
                    &open,
                    &request,
                    session,
                    &mut additions,
                    window,
                    first,
                    cancel,
                )?;
                exhausted = step.exhausted;
                expanded = Some(match expanded.take() {
                    Some(earlier) => earlier.followed_by(step),
                    None => step,
                });
                if capped {
                    let expanded = expanded.as_mut().expect("just set");
                    expanded.capped_request = Some(requested_window);
                }
            }
            search_cancellation::look(cancel, SearchCheckpoint::Painting)?;
            let page_started = Instant::now();
            let mut timings = expanded
                .as_ref()
                .map_or_else(SemanticTimings::default, |expanded| expanded.timings);

            session.commit(additions);
            session.window = window;
            session.exhausted = exhausted;
            if let Some(expanded) = expanded {
                session.state.absorb(expanded);
            }
            self.semantic_sessions.put(key, Arc::clone(&shared));
            let has_more = session.len() > (offset as usize).saturating_add(limit as usize);
            session.show(offset, limit);

            // Built once a session, the first time it has a page to paint.
            if session.state.painter.is_none() && session.len() > offset as usize {
                if let Some(highlight) = session.state.highlight.take() {
                    session.state.painter =
                        Some(self.semantic_snippet_painter(&session.searcher, highlight)?);
                }
            }
            let session = &*session;
            let page = session.page(offset, limit);
            let page_addresses: Vec<DocAddress> = page
                .iter()
                .filter_map(|entry| entry.hydrated.as_ref().map(|(address, _)| *address))
                .collect();
            let page_texts = self.texts_at(&session.searcher, &page_addresses)?;
            let empty_text = DisplayText::default();
            let page_lines: Vec<EmbeddedLine<'_>> = page
                .iter()
                .filter_map(|entry| {
                    let (address, document) = entry.hydrated.as_ref()?;
                    Some((
                        *address,
                        document.file_path.as_str(),
                        entry.item.id,
                        entry.window,
                    ))
                })
                .zip(&page_texts)
                .map(|((address, book, id, window), (text, status))| {
                    let text = if *status == TextStatus::Ok {
                        text
                    } else {
                        &empty_text
                    };
                    (address, book, id, text, window)
                })
                .collect();
            let windows =
                self.embedded_window_texts(&open.chunking, &session.searcher, &page_lines)?;
            let mut page_texts = page_texts.into_iter().zip(windows);
            // Same budget the lexical API's default highlight uses, so both
            // paths bound the display string identically.
            let snippet_budget = HighlightConfig::default().max_chars;

            let mut results = Vec::with_capacity(page.len());
            let mut stale_siblings_dropped = 0u32;
            for entry in page {
                let item = &entry.item;
                let mut page_stale_siblings = 0u32;
                let mut merged = Vec::with_capacity(item.merged.len());
                for (sibling, resolved) in item.merged.iter().zip(&entry.siblings) {
                    let address = match resolved {
                        Some(address) => Some(*address),
                        // A sibling only lexical search found is of the group's book too:
                        // hydrated by that book and its id, never by the id alone.
                        None => {
                            self.address_in_book(&session.searcher, &sibling.file_path, sibling.id)?
                        }
                    };
                    let hydrated = address
                        .map(|address| {
                            self.document_at(&session.searcher, address, sibling.id, false)
                        })
                        .transpose()?;
                    match hydrated {
                        Some(document) => merged.push(MergedSibling {
                            title: document.title,
                            reference: document.reference,
                            id: document.id,
                            segment: document.segment,
                            is_pdf: document.is_pdf,
                            file_path: document.file_path,
                        }),
                        None => page_stale_siblings = page_stale_siblings.saturating_add(1),
                    }
                }
                stale_siblings_dropped = stale_siblings_dropped.saturating_add(page_stale_siblings);

                // Tantivy's copy for a hydrated item, the lexical candidate's otherwise.
                let (title, reference, text, window, segment, is_pdf, file_path, text_status) =
                    match &entry.hydrated {
                        Some((_, document)) => {
                            let ((text, status), window) = page_texts.next().unwrap_or((
                                (DisplayText::default(), TextStatus::Unavailable),
                                None,
                            ));
                            (
                                document.title.clone(),
                                document.reference.clone(),
                                text,
                                window,
                                document.segment,
                                document.is_pdf,
                                document.file_path.clone(),
                                status,
                            )
                        }
                        None => (
                            item.title.clone(),
                            item.reference.clone(),
                            entry.notes.clone().display(item.text.clone(), false),
                            None,
                            item.segment,
                            item.is_pdf,
                            item.file_path.clone(),
                            entry.text_status,
                        ),
                    };
                let (snippet_html, is_highlighted) =
                    match (session.state.painter.as_ref(), text_status) {
                        // A BM25 score is present exactly when Tantivy returned this
                        // line for the lexical query, which is what licenses the
                        // phrase-fallback term painting inside `paint`.
                        (Some(painter), TextStatus::Ok) => painter.paint_display_shown(
                            &text,
                            window.as_ref(),
                            item.lexical_score.is_some(),
                        ),
                        (_, TextStatus::Unavailable) => (String::new(), false),
                        // `SemanticOnly` runs no lexical query, so there is nothing
                        // to paint with — the line still crosses FFI bounded. A stale
                        // line is never painted.
                        _ => (
                            if text_status == TextStatus::Ok {
                                bounded_display_snippet(
                                    window.as_ref().unwrap_or(&text),
                                    snippet_budget,
                                )
                            } else {
                                bounded_plain_snippet(
                                    window.as_deref().unwrap_or(&text),
                                    snippet_budget,
                                )
                            },
                            false,
                        ),
                    };

                results.push(SemanticSearchResult {
                    title,
                    reference,
                    snippet_html,
                    is_highlighted,
                    id: item.id,
                    segment,
                    is_pdf,
                    file_path,
                    // The sidecar intentionally caps the materialized sibling
                    // list. Preserve its full group count and subtract only
                    // stale siblings that were actually observed in that list.
                    merged_count: item.merged_count.saturating_sub(page_stale_siblings).max(1),
                    merged,
                    lexical_score: item.lexical_score,
                    semantic_score: item.semantic_score,
                    fused_score: item.fused_score,
                    source: match item.source {
                        SidecarResultSource::Lexical => SemanticResultSource::Lexical,
                        SidecarResultSource::Semantic => SemanticResultSource::Semantic,
                        SidecarResultSource::Both => SemanticResultSource::Both,
                    },
                    needs_hydration: false,
                    text_status,
                    matched_window: entry.window,
                });
            }

            let state = &session.state;
            let mut fallback_reason = state.fallback_reason.clone();
            // Only a semantic half that failed has a reason, stringified by the coordinator;
            // the notes appended below have no kind.
            let fallback_kind = fallback_reason
                .as_ref()
                .map(|_| SemanticErrorKind::QueryFailed);
            let mut note = |text: String| {
                fallback_reason = Some(match fallback_reason.take() {
                    Some(reason) => format!("{reason}; {text}"),
                    None => text,
                });
            };
            // Primaries are whole cards the session left out; siblings are hydrated per page,
            // so their count is this page's.
            if state.stale_primaries_dropped > 0 {
                note(format!(
                    "dropped {} stale semantic result(s) missing from Tantivy; candidate \
                     counts may still include stale records; rebuild or reconcile the \
                     semantic index",
                    state.stale_primaries_dropped
                ));
            }
            if stale_siblings_dropped > 0 {
                note(format!(
                    "dropped {stale_siblings_dropped} stale grouped sibling(s) missing from \
                     Tantivy on this page; rebuild or reconcile the semantic index"
                ));
            }
            if state.unverified > 0 {
                note(format!(
                    "{} semantic match(es) were not shown as such: their line no longer \
                     holds the text the vector was embedded from",
                    state.unverified
                ));
            }
            if let Some(requested) = state.capped_request {
                note(format!(
                    "semantic candidate window capped at {MAX_SEMANTIC_CANDIDATE_WINDOW} \
                     (requested {requested})"
                ));
            }
            timings.page_ms = elapsed_ms(page_started);
            timings.total_ms = elapsed_ms(started);
            debug!("semantic page at {offset} of {limit}: {timings:?}");
            #[cfg(test)]
            LAST_SEMANTIC_TIMINGS.set(timings);
            Ok(SemanticSearchResponse {
                results,
                // The last expansion's candidate-set counts: stable across the pages it
                // serves, and never the number of results a session will show.
                total_count: state.total_count,
                lexical_total_count: state.lexical_total_count,
                group_count: state.group_count,
                counts_are_exact: false,
                requested_mode: retrieval_mode,
                executed_mode: state.executed_mode,
                semantic_available: state.semantic_available,
                fallback_reason,
                fallback_kind,
                latency_ms: started.elapsed().as_millis() as u64,
                candidate_window_truncated: state.capped_request.is_some(),
                truncated: state.truncated,
                has_more,
                session_restarted,
            })
        }

        #[cfg(not(feature = "semantic-integration"))]
        {
            // No sidecar, so nothing to rank by them, and no rules to check them by.
            let _ = ranking;
            self.semantic_lexical_fallback_response(
                &query,
                &facets,
                limit,
                offset,
                lexical_mode,
                fuzzy_max_distance,
                retrieval_mode,
                grouping,
                match_nikud,
                match_taamim,
                SemanticError::new(
                    SemanticErrorKind::FeatureNotInBuild,
                    "semantic support is not compiled into this build",
                ),
                started.elapsed().as_millis() as u64,
                cancel,
            )
        }
    }

    /// Marks the clause of each target's line nearest `query`, one per target in order; a line
    /// that cannot be marked is not an error. Cancelled, it ends with a `Cancelled` error.
    pub fn semantic_passage_highlights(
        &self,
        query: String,
        targets: Vec<SemanticHighlightTarget>,
        cancellation: &SemanticCancellationToken,
    ) -> Result<Vec<SemanticPassageHighlight>, SemanticError> {
        let cancel = &cancellation.flag;
        search_cancellation::look(cancel, SearchCheckpoint::Start)?;
        #[cfg(feature = "semantic-integration")]
        {
            if let Some(open) = self.semantic_engine() {
                return self.passage_highlights_with(&open, &query, targets, cancel);
            }
        }
        let _ = query;
        Ok(targets
            .into_iter()
            .map(|target| SemanticPassageHighlight {
                file_path: target.file_path,
                id: target.id,
                snippet_html: String::new(),
                is_highlighted: false,
                span_score: None,
            })
            .collect())
    }

    #[cfg(feature = "semantic-integration")]
    fn passage_highlights_with(
        &self,
        open: &OpenSession,
        query: &str,
        targets: Vec<SemanticHighlightTarget>,
        cancel: &SearchCancellation,
    ) -> Result<Vec<SemanticPassageHighlight>, SemanticError> {
        use crate::semantic_highlight::{Highlight, HighlightKey, MAX_CLAUSES_PER_CALL};
        let searcher = self.index_reader.searcher();
        let mut addresses = Vec::with_capacity(targets.len());
        let mut slots = Vec::with_capacity(targets.len());
        for target in &targets {
            let address = self.address_in_book(&searcher, &target.file_path, target.id)?;
            slots.push(address.map(|address| {
                addresses.push(address);
                addresses.len() - 1
            }));
        }
        let mut texts = self.texts_at(&searcher, &addresses)?;
        let empty_text = DisplayText::default();
        let lines: Vec<EmbeddedLine<'_>> = targets
            .iter()
            .zip(&slots)
            .filter_map(|(target, slot)| {
                let slot = (*slot)?;
                let (text, status) = &texts[slot];
                let text = if *status == TextStatus::Ok {
                    text
                } else {
                    &empty_text
                };
                Some((
                    addresses[slot],
                    target.file_path.as_str(),
                    target.id,
                    text,
                    target.window,
                ))
            })
            .collect();
        let windows = self.embedded_window_texts(&open.chunking, &searcher, &lines)?;
        for ((text, _), window) in texts.iter_mut().zip(windows) {
            if let Some(window) = window {
                *text = window;
            }
        }
        let epoch = self.semantic_sessions.epoch();
        let normalized = HebrewNormalizer::new().normalize(query);
        let budget = HighlightConfig::default().max_chars as usize;
        let mut query_side = PassageQuery::default();
        let mut clauses_left = MAX_CLAUSES_PER_CALL;

        let mut out = Vec::with_capacity(targets.len());
        for (target, slot) in targets.into_iter().zip(slots) {
            let text = slot.and_then(|slot| match &texts[slot] {
                (text, TextStatus::Ok) => Some(text),
                _ => None,
            });
            let highlight = match text {
                None => Highlight::none(),
                Some(text) => {
                    let key = HighlightKey {
                        epoch,
                        query: normalized.clone(),
                        file_path: target.file_path.clone(),
                        id: target.id,
                        text_crc: crc32fast::hash(text.as_bytes()),
                        note_hash: text.annotation_hash(),
                    };
                    match self.semantic_highlights.get(&key) {
                        Some(known) => known,
                        None => match self.passage_highlight(
                            open,
                            query,
                            text,
                            budget,
                            &mut query_side,
                            &mut clauses_left,
                            cancel,
                        )? {
                            Some(computed) => {
                                self.semantic_highlights.put(key, computed.clone());
                                computed
                            }
                            None => Highlight::none(),
                        },
                    }
                }
            };
            out.push(SemanticPassageHighlight {
                file_path: target.file_path,
                id: target.id,
                is_highlighted: !highlight.snippet_html.is_empty(),
                snippet_html: highlight.snippet_html,
                span_score: highlight.span_score,
            });
        }
        Ok(out)
    }

    /// One line's highlight; `None` when the call's clauses ran out before it, which is not
    /// remembered.
    #[cfg(feature = "semantic-integration")]
    #[allow(clippy::too_many_arguments)]
    fn passage_highlight(
        &self,
        open: &OpenSession,
        query: &str,
        text: &DisplayText,
        budget: usize,
        query_side: &mut PassageQuery,
        clauses_left: &mut usize,
        cancel: &SearchCancellation,
    ) -> Result<Option<crate::semantic_highlight::Highlight>, SemanticError> {
        use crate::semantic_highlight::{self as passage, Highlight};
        let (clauses, parts): (Vec<_>, Vec<_>) =
            passage::clauses_in_parts(text).into_iter().unzip();
        if clauses.is_empty() {
            return Ok(Some(Highlight::none()));
        }
        let cap = passage::MAX_CLAUSES_PER_LINE.min(*clauses_left);
        if cap < 2 {
            return Ok(None);
        }
        search_cancellation::look(cancel, SearchCheckpoint::Highlight)?;
        let failed = |err: SemanticSearchError| semantic_errors::search_error(&err, open.call());
        if query_side.vector.is_none() {
            let canonical = smart_query::canonical(query);
            let vector = open
                .coordinator
                .embed_query_cached(&canonical, cancel)
                .map_err(failed)?;
            query_side.vector = Some(vector);
        }
        // A cold dictionary lookup costs up to a second: only a line with more clauses than
        // it embeds needs the words to choose by.
        let chosen = if clauses.len() <= cap {
            (0..clauses.len()).collect()
        } else {
            if query_side.words.is_none() {
                query_side.words = Some(self.passage_query_words(query)?);
            }
            let tokens = clauses
                .iter()
                .map(|clause| self.index_token_texts(&text[clause.clone()]))
                .collect::<Result<Vec<_>>>()?;
            passage::preselect(&tokens, query_side.words.as_ref().expect("set above"), cap)
        };
        let inputs: Vec<&str> = chosen
            .iter()
            .map(|&index| &text[clauses[index].clone()])
            .collect();
        let vectors = open
            .coordinator
            .embed_passages(&inputs, cancel)
            .map_err(failed)?;
        *clauses_left -= inputs.len();
        #[cfg(test)]
        passage::EMBEDDED_CLAUSES.with(|count| count.set(count.get() + inputs.len()));
        let query_vector = query_side.vector.as_deref().expect("set above");
        let Some((best, score)) = passage::nearest(query_vector, &vectors) else {
            return Ok(Some(Highlight::none()));
        };
        if query_side.tokens.is_none() {
            query_side.tokens = Some(
                self.index_token_texts(query)?
                    .iter()
                    .flat_map(|token| smart_query::alternatives(token))
                    .collect(),
            );
        }
        let tokens = query_side.tokens.as_ref().expect("set above");
        let span = passage::widen_to_query_words(
            text,
            clauses[chosen[best]].clone(),
            parts[chosen[best]].clone(),
            |word| {
                self.index_token_texts(word)
                    .is_ok_and(|found| found.iter().any(|token| tokens.contains(token)))
            },
        );
        Ok(Some(match passage::marked_snippet_at(text, span, budget) {
            Some((window, snippet_html)) => Highlight {
                snippet_html: passage::with_ellipsis(
                    text.annotate(snippet_html, window.start, "<mark>", "</mark>"),
                    window.start > 0,
                    window.end < text.len(),
                ),
                span_score: Some(score),
            },
            None => Highlight::none(),
        }))
    }

    /// The query's words as the index spells them, quote-free, and their dictionary forms: what
    /// a clause sharing words with the query is told by.
    #[cfg(feature = "semantic-integration")]
    fn passage_query_words(&self, query: &str) -> Result<HashSet<String>> {
        let mut words = HashSet::new();
        for token in self
            .index_token_texts(query)?
            .iter()
            .flat_map(|token| smart_query::alternatives(token))
        {
            if let Some(clean) = Self::quoteless_variant(&token) {
                words.insert(clean);
            }
            if let Some(dict) = self.magic_dict.as_ref() {
                words.extend(dict.highlight_forms(&token, MAX_LEXICAL_FORMS));
            }
            words.insert(token);
        }
        Ok(words)
    }

    /// Fuse `session`'s search over a `window`, its two halves side by side, and take every
    /// result the session lacks into `additions`; `first` also builds the highlight and count.
    #[cfg(feature = "semantic-integration")]
    #[allow(clippy::too_many_arguments)]
    fn expand_semantic_session(
        &self,
        open: &OpenSession,
        request: &SemanticRequest<'_>,
        session: &SemanticSearchSession,
        additions: &mut Additions<SessionEntry>,
        window: u32,
        first: bool,
        cancel: &SearchCancellation,
    ) -> Result<Expanded, SemanticError> {
        #[cfg(test)]
        self.semantic_sessions
            .expansions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let searcher = &session.searcher;
        // The live index a vector set's hits are resolved against, at the generation this
        // search reads. A session built on this device never asks it.
        let unreadable = |err: otzaria_semantic_search::semantic::resolve::ResolveError| {
            if err == otzaria_semantic_search::semantic::resolve::ResolveError::Cancelled {
                return SemanticError::cancelled();
            }
            SemanticError::new(
                SemanticErrorKind::Internal,
                format!("the index could not be read to resolve semantic results: {err}"),
            )
        };
        let mut resolver = crate::semantic_resolver::LiveResolver::for_chunking(
            searcher.clone(),
            self.chunk_key_field,
            &self.semantic_resolver,
            open.chunking.clone(),
        )
        .map_err(unreadable)?;
        resolver.share_keys(session.state.library_generation);
        resolver.set_alias_penalty(request.ranking.alias_penalty);
        let filters = SidecarSearchFilters {
            book_paths: None,
            facets: (!request.facets.is_empty()).then(|| request.facets.to_vec()),
            include_pdf: None,
        };
        let params = HybridSearchParams {
            limit: window as usize,
            offset: 0,
            grouping: request.grouping.map(|value| match value {
                SemanticGroupingMode::SameSection => SidecarGroupingMode::SameSection,
                SemanticGroupingMode::IdenticalText => SidecarGroupingMode::IdenticalText,
            }),
            filters: Some(filters.clone()),
            force_mode: Some(match request.retrieval_mode {
                SemanticRetrievalMode::Hybrid => SidecarSearchMode::Hybrid,
                SemanticRetrievalMode::SemanticOnly => SidecarSearchMode::SemanticOnly,
                SemanticRetrievalMode::LexicalOnly => SidecarSearchMode::LexicalOnly,
            }),
            // The preset and the feature flags, which clamp where `ranking` is refused, stay
            // the sidecar's defaults.
            profile: None,
            feature_flags: None,
            ranking: Some(request.ranking.clone()),
        };

        // The lexical phase on its own thread, the semantic half on this one, where a test's
        // probes see it.
        let semantic_started = Instant::now();
        let canonical = smart_query::canonical(request.query);
        let ((lexical, lexical_ms), (prepared, semantic_ms)) = std::thread::scope(|scope| {
            let lexical = scope.spawn(|| {
                let started = Instant::now();
                let phase = self.semantic_lexical_phase(request, searcher, window, first);
                (phase, elapsed_ms(started))
            });
            let prepared = self.plan_semantic_scan(open, request, &filters, &mut resolver, cancel);
            let prepared = prepared.and_then(|()| {
                // The sidecar's first act is to look at the token, so this crate does not look
                // here itself; a test is told how far the search got.
                search_cancellation::reached(SearchCheckpoint::Sidecar, cancel);
                open.coordinator
                    .prepare_semantic(&canonical, &params, &resolver, cancel)
                    .map_err(|err| semantic_errors::search_error(&err, open.call()))
            });
            let semantic_ms = elapsed_ms(semantic_started);
            let lexical = lexical
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            (lexical, (prepared, semantic_ms))
        });
        let prepared = match prepared {
            Err(error) if error.kind == SemanticErrorKind::Cancelled => return Err(error),
            prepared => prepared,
        };
        let lexical = lexical?;
        let prepared = prepared?;
        if cancel.is_cancelled() {
            return Err(SemanticError::cancelled());
        }

        let SemanticLexicalPhase {
            candidates: lexical_candidates,
            total_count: lexical_total_count,
            truncated,
            highlight,
            text_status: candidate_text_status,
            notes: mut candidate_notes,
        } = lexical;
        let lexical_done = matches!(request.retrieval_mode, SemanticRetrievalMode::SemanticOnly)
            || lexical_candidates.len() < window as usize;
        let semantic_done = !prepared.healthy() || prepared.semantic_hits() < prepared.top_k();
        let fuse_started = Instant::now();
        let result = open
            .coordinator
            .fuse_prepared(lexical_candidates, prepared, &params, cancel)
            .map_err(|err| semantic_errors::search_error(&err, open.call()))?;
        let fuse_ms = elapsed_ms(fuse_started);
        let telemetry = result.telemetry.clone();
        let sidecar_ms =
            |field: fn(&otzaria_semantic_search::telemetry::SearchTelemetry) -> Option<u64>| {
                telemetry.as_ref().and_then(field).unwrap_or(0) as f64
            };
        // Hydration is a lookup per result, and nobody may be waiting for them.
        search_cancellation::look(cancel, SearchCheckpoint::Hydration)?;
        let hydrate_started = Instant::now();
        let fused = result.group_count.unwrap_or(result.total_count);
        let exhausted = (lexical_done && semantic_done && fused <= window)
            || window >= MAX_SEMANTIC_CANDIDATE_WINDOW;

        // Where each line a vector set's hit was resolved to is, by (book, id); empty for a
        // session built on this device, whose lines are hydrated by book and id.
        let records = resolver.records();
        let unverified = resolver.unverified();

        // Existence and metadata only, for results the session lacks; the text is read per
        // page. Only a `needs_hydration` item can be stale: lexical ones came from this searcher.
        let mut stale_primaries_dropped = 0u32;
        for mut item in result.results {
            let line = (item.file_path.clone(), item.id);
            let group = match (request.grouping, item.provenance.as_ref()) {
                (Some(SemanticGroupingMode::SameSection), Some(fused)) => {
                    Some(GroupKey::Section(fused.file_path.clone(), fused.section_id))
                }
                (Some(SemanticGroupingMode::IdenticalText), Some(fused))
                    if fused.line_hash != 0 =>
                {
                    Some(GroupKey::Text(fused.line_hash))
                }
                _ => None,
            };
            if !session.admits(additions, &line, group.as_ref()) {
                continue;
            }
            let siblings: Vec<LineKey> = item
                .merged
                .iter()
                .map(|sibling| (sibling.file_path.clone(), sibling.id))
                .collect();
            let hydrated = if item.needs_hydration {
                let address = match records.get(&line) {
                    // By the address the line was resolved at, in the searcher that
                    // resolved it: two books' lines can share an id, and an address cannot.
                    Some(record) => Some(record.address),
                    None => self.address_in_book(searcher, &item.file_path, item.id)?,
                };
                match address {
                    Some(address) => Some((
                        address,
                        self.document_at(searcher, address, item.id, false)?,
                    )),
                    // Stale: its old metadata never reaches Dart, so a delayed sidecar
                    // cleanup cannot resurrect deleted content.
                    None => {
                        stale_primaries_dropped = stale_primaries_dropped.saturating_add(1);
                        SemanticSearchSession::take(additions, line, &siblings, group, None);
                        continue;
                    }
                }
            } else {
                None
            };
            let sibling_addresses = siblings
                .iter()
                .map(|sibling| records.get(sibling).map(|record| record.address))
                .collect();
            let text_status = candidate_text_status
                .get(&item.id)
                .copied()
                .unwrap_or(TextStatus::Ok);
            // Its fields are the item's own, and its text a second copy of the line.
            item.provenance = None;
            let window = records.get(&line).map_or(0, |record| record.window);
            let entry = SessionEntry {
                item,
                hydrated,
                siblings: sibling_addresses,
                text_status,
                notes: candidate_notes.remove(&line).unwrap_or_default(),
                window,
            };
            SemanticSearchSession::take(additions, line, &siblings, group, Some(entry));
        }

        Ok(Expanded {
            lexical: (first
                || !matches!(request.retrieval_mode, SemanticRetrievalMode::SemanticOnly))
            .then_some((lexical_total_count, truncated)),
            highlight,
            total_count: result.total_count,
            group_count: result.group_count,
            executed_mode: match result.search_mode {
                SidecarSearchMode::Hybrid => SemanticExecutedMode::Hybrid,
                SidecarSearchMode::SemanticOnly => SemanticExecutedMode::SemanticOnly,
                SidecarSearchMode::LexicalOnly => SemanticExecutedMode::LexicalOnly,
            },
            semantic_available: result.semantic_available,
            fallback_reason: result.fallback_reason,
            stale_primaries_dropped,
            unverified,
            exhausted,
            capped_request: None,
            timings: SemanticTimings {
                expansions: 1,
                lexical_ms,
                semantic_ms,
                embed_ms: sidecar_ms(|telemetry| telemetry.embedding_latency_ms),
                scan_ms: sidecar_ms(|telemetry| telemetry.scan_ms),
                resolve_ms: sidecar_ms(|telemetry| telemetry.resolve_ms),
                fuse_ms,
                hydrate_ms: elapsed_ms(hydrate_started),
                ..SemanticTimings::default()
            },
        })
    }

    /// The lexical half of one expansion, in `searcher`: the candidates of a `window`, or for
    /// `SemanticOnly`, which discards them, the count alone, once a session.
    #[cfg(feature = "semantic-integration")]
    fn semantic_lexical_phase(
        &self,
        request: &SemanticRequest<'_>,
        searcher: &Searcher,
        window: u32,
        first: bool,
    ) -> Result<SemanticLexicalPhase, SemanticError> {
        if matches!(request.retrieval_mode, SemanticRetrievalMode::SemanticOnly) {
            let count = if !first {
                None
            } else {
                Some(match request.lexical_mode {
                    SemanticLexicalMode::Exact => self.count_smart_exact(
                        request.query,
                        request.facets,
                        request.match_nikud,
                        request.match_taamim,
                    )?,
                    SemanticLexicalMode::Fuzzy => self.count_smart_fuzzy(
                        request.query,
                        request.facets,
                        request.fuzzy_max_distance,
                        request.match_nikud,
                        request.match_taamim,
                    )?,
                })
            };
            return Ok(SemanticLexicalPhase {
                candidates: Vec::new(),
                total_count: count.as_ref().map_or(0, |count| count.count),
                truncated: count.is_some_and(|count| count.truncated),
                highlight: None,
                text_status: HashMap::new(),
                notes: HashMap::new(),
            });
        }
        Ok(match request.lexical_mode {
            SemanticLexicalMode::Exact => self.semantic_exact_lexical_candidates(
                searcher,
                request.query,
                request.facets,
                window,
                request.match_nikud,
                request.match_taamim,
                first,
            )?,
            SemanticLexicalMode::Fuzzy => self.semantic_fuzzy_lexical_candidates(
                searcher,
                request.query,
                request.facets,
                window,
                request.fuzzy_max_distance,
                request.match_nikud,
                request.match_taamim,
                first,
            )?,
        })
    }

    /// Plan an opened vector set's scan (see `crate::semantic_moves`) for the search's filter
    /// and the foundational books' query; a filter that cannot be planned goes unplanned.
    #[cfg(feature = "semantic-integration")]
    fn plan_semantic_scan(
        &self,
        open: &OpenSession,
        request: &SemanticRequest<'_>,
        filters: &SidecarSearchFilters,
        resolver: &mut crate::semantic_resolver::LiveResolver<'_>,
        cancel: &SearchCancellation,
    ) -> Result<(), SemanticError> {
        use otzaria_semantic_search::semantic::resolve::ResolveError;
        let Some(vectors_dir) = &open.vectors_dir else {
            return Ok(());
        };
        if matches!(request.retrieval_mode, SemanticRetrievalMode::LexicalOnly)
            || !resolver.has_column()
        {
            return Ok(());
        }
        let Some(view) = open
            .coordinator
            .vector_set_info()
            .and_then(|info| self.semantic_set_view(vectors_dir, info.generation))
        else {
            return Ok(());
        };
        // A plan that cannot be made leaves the search unplanned, as an unfiltered one: the
        // app's whole-library search is filtered by "/", and must not fail with it.
        match resolver.plan(filters, &view, cancel) {
            Ok(_) => {}
            Err(ResolveError::Cancelled) => return Err(SemanticError::cancelled()),
            Err(error) => warn!("a filtered semantic search goes unplanned: {error}"),
        }
        if !PLAN_FOUNDATIONAL_QUERY
            || request.ranking.foundational_candidate_share <= 0.0
            || is_foundational(request.facets)
        {
            return Ok(());
        }
        // The very filter the coordinator's foundational query passes, so the resolver finds
        // its plan by it.
        let mut foundational = filters.clone();
        foundational
            .facets
            .get_or_insert_with(Vec::new)
            .push(FOUNDATIONAL_FACET.to_string());
        let planning = Instant::now();
        match resolver.plan(&foundational, &view, cancel) {
            Ok(_) => {}
            Err(ResolveError::Cancelled) => return Err(SemanticError::cancelled()),
            // Unplanned, the foundational query scans the books it admits, without the texts
            // that moved into them: worse recall for it alone, not a failed search.
            Err(error) => warn!("the foundational books' semantic query goes unplanned: {error}"),
        }
        debug!(
            "planned the foundational books' semantic query in {} ms",
            planning.elapsed().as_millis()
        );
        Ok(())
    }

    #[cfg(not(feature = "semantic-integration"))]
    fn semantic_disabled_status() -> SemanticStatus {
        Self::semantic_closed_status(
            SemanticState::NotInBuild,
            SemanticError::new(
                SemanticErrorKind::FeatureNotInBuild,
                "semantic support is not compiled into this build",
            ),
        )
    }

    #[cfg(feature = "semantic-integration")]
    fn semantic_not_configured_status() -> SemanticStatus {
        Self::semantic_closed_status(
            SemanticState::NotConfigured,
            SemanticError::new(
                SemanticErrorKind::NotConfigured,
                "semantic sidecar has not been configured",
            ),
        )
    }

    /// The status when no session is open, in `state`, for the reason `why` gives.
    fn semantic_closed_status(state: SemanticState, why: SemanticError) -> SemanticStatus {
        SemanticStatus {
            state,
            enabled: false,
            available: false,
            model_loaded: false,
            indexed_book_count: 0,
            vector_count: 0,
            model_id: String::new(),
            embedding_dim: 0,
            embedding_backend: None,
            vector_backend: String::new(),
            vectors_persisted: false,
            needs_full_reindex: None,
            last_error: Some(why.message),
            error_kind: Some(why.kind),
            vectors_library_version: None,
            vector_segments: 0,
            needs_compaction: false,
        }
    }

    fn semantic_disabled_diff() -> SemanticIndexDiff {
        SemanticIndexDiff {
            enabled: false,
            new_books: Vec::new(),
            changed_books: Vec::new(),
            unverifiable_books: Vec::new(),
            removed_books: Vec::new(),
            model_mismatch: false,
            chunking_mismatch: false,
            normalization_mismatch: false,
        }
    }

    /// The lexical results a search falls back to when the sidecar is not asked, with `why`
    /// as the response's `fallback_reason` and `fallback_kind`. The search's token is looked
    /// at once the page is ready, so a search cancelled while its fallback ran is cancelled
    /// rather than served.
    #[allow(clippy::too_many_arguments)]
    fn semantic_lexical_fallback_response(
        &self,
        query: &str,
        facets: &[String],
        limit: u32,
        offset: u32,
        lexical_mode: SemanticLexicalMode,
        fuzzy_max_distance: u8,
        requested_mode: SemanticRetrievalMode,
        grouping: Option<SemanticGroupingMode>,
        match_nikud: bool,
        match_taamim: bool,
        why: SemanticError,
        latency_ms: u64,
        cancel: &SearchCancellation,
    ) -> Result<SemanticSearchResponse, SemanticError> {
        let fallback_reason = Some(why.message);
        let fallback_kind = Some(why.kind);
        if matches!(requested_mode, SemanticRetrievalMode::SemanticOnly) {
            let count = match lexical_mode {
                SemanticLexicalMode::Exact => {
                    self.count_smart_exact(query, facets, match_nikud, match_taamim)?
                }
                SemanticLexicalMode::Fuzzy => self.count_smart_fuzzy(
                    query,
                    facets,
                    fuzzy_max_distance,
                    match_nikud,
                    match_taamim,
                )?,
            };
            search_cancellation::look(cancel, SearchCheckpoint::Fallback)?;
            return Ok(SemanticSearchResponse {
                results: Vec::new(),
                total_count: 0,
                lexical_total_count: count.count,
                group_count: None,
                counts_are_exact: true,
                requested_mode,
                executed_mode: SemanticExecutedMode::SemanticOnly,
                semantic_available: false,
                fallback_reason,
                fallback_kind,
                latency_ms,
                candidate_window_truncated: false,
                truncated: count.truncated,
                has_more: false,
                session_restarted: false,
            });
        }

        let grouping = grouping.map(|value| match value {
            SemanticGroupingMode::SameSection => ResultGrouping::SameSection,
            SemanticGroupingMode::IdenticalText => ResultGrouping::IdenticalText,
        });
        let page = match lexical_mode {
            SemanticLexicalMode::Exact => self.search_and_count_smart_exact(
                query,
                facets,
                limit,
                offset,
                match_nikud,
                match_taamim,
                grouping,
            )?,
            SemanticLexicalMode::Fuzzy => self.search_and_count_smart_fuzzy(
                query,
                facets,
                limit,
                offset,
                fuzzy_max_distance,
                match_nikud,
                match_taamim,
                grouping,
            )?,
        };
        let hl = HighlightConfig::default();
        let results: Vec<SemanticSearchResult> = page
            .results
            .into_iter()
            .enumerate()
            .map(|(rank, item)| {
                // `SearchResult::text` is snippet HTML when the highlighter
                // painted the line, and the raw stored line when it painted
                // nothing (a bare fuzzy automaton exposes no static terms to the
                // generator). Only the latter needs escaping and bounding, so
                // the semantic envelope carries the same kind of value on both
                // paths. Detection is by the prefix the highlighter inserts:
                // painted output has every literal `<` escaped, so the marker
                // can only be real markup — and a corpus line that happens to
                // contain it verbatim is passed through exactly as the existing
                // lexical API already passes it through.
                let is_highlighted =
                    item.text_status == TextStatus::Ok && item.text.contains(&hl.highlight_prefix);
                let snippet_html = if is_highlighted
                    || item.text_status != TextStatus::Ok
                    || item.text.contains(note_spans::NOTE_OPEN)
                    || item.text.contains(note_spans::MARKER_OPEN)
                {
                    // A stale line arrives already escaped and bounded.
                    item.text
                } else {
                    bounded_plain_snippet(&item.text, hl.max_chars)
                };
                SemanticSearchResult {
                    title: item.title,
                    reference: item.reference,
                    snippet_html,
                    is_highlighted,
                    id: item.id,
                    segment: item.segment,
                    is_pdf: item.is_pdf,
                    file_path: item.file_path,
                    merged_count: item.merged_count,
                    merged: item.merged,
                    lexical_score: None,
                    semantic_score: None,
                    // Tantivy's legacy display API does not expose its score.
                    // Keep the existing relevance order observable without
                    // pretending a synthetic value is a BM25 score.
                    fused_score: 1.0 / (rank.saturating_add(1) as f32),
                    source: SemanticResultSource::Lexical,
                    needs_hydration: false,
                    text_status: item.text_status,
                    matched_window: 0,
                }
            })
            .collect();
        search_cancellation::look(cancel, SearchCheckpoint::Fallback)?;
        let shown = u64::from(offset) + results.len() as u64;
        let has_more = shown < u64::from(page.group_count.unwrap_or(page.total_count));
        Ok(SemanticSearchResponse {
            total_count: page.total_count,
            lexical_total_count: page.total_count,
            group_count: page.group_count,
            counts_are_exact: !page.truncated,
            results,
            requested_mode,
            executed_mode: SemanticExecutedMode::LexicalOnly,
            semantic_available: false,
            fallback_reason,
            fallback_kind,
            latency_ms,
            candidate_window_truncated: false,
            truncated: page.truncated,
            has_more,
            session_restarted: false,
        })
    }

    // ── Write API ──────────────────────────────────────────────────────────────

    /// Add a single line of a book. Does not commit. The id is the engine's: line `ordinal`
    /// (0-based) of the book `file_path`, whose number is assigned on its first write.
    /// Line 0 records the book's `metadataHash`; no fingerprint (`contentHash` = 0): batch
    /// ingestion via [`Self::add_documents_batch`] is the fingerprint-aware path.
    ///
    /// `text` is normalized here ([`normalize_text_for_indexing`]) — the API
    /// is exposed over FFI, so the "input is already normalized" assumption
    /// is enforced rather than documented. Already-normalized text passes
    /// through the fast paths at negligible cost.
    pub fn add_document(
        &mut self,
        ordinal: u32,
        title: &str,
        reference: &str,
        topics: &str,
        text: &str,
        segment: u64,
        is_pdf: bool,
        file_path: &str,
        section_ordinal: Option<u32>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<()> {
        self.ensure_write_access()?;
        let document = Self::single_input(
            ordinal,
            title,
            reference,
            topics,
            text,
            segment,
            is_pdf,
            file_path,
            section_ordinal,
            extra_facets,
        );
        self.write_inputs_with(vec![document], false, Ids::Composed, Stamping::LineZero)
    }

    /// The [`DocumentInput`] of [`Self::add_document`]: a line carrying marks also gets its
    /// vocalized copy, normalized from the raw text.
    #[allow(clippy::too_many_arguments)]
    fn single_input(
        ordinal: u32,
        title: &str,
        reference: &str,
        topics: &str,
        text: &str,
        segment: u64,
        is_pdf: bool,
        file_path: &str,
        section_ordinal: Option<u32>,
        extra_facets: Option<Vec<String>>,
    ) -> DocumentInput {
        DocumentInput {
            ordinal,
            title: title.to_owned(),
            reference: reference.to_owned(),
            topics: topics.to_owned(),
            text: text.to_owned(),
            segment,
            is_pdf,
            file_path: file_path.to_owned(),
            content_hash: None,
            text_hash: None,
            source_hash: None,
            text_vocalized: hebrew_query::contains_attached_marks(text).then(|| text.to_owned()),
            section_ordinal,
            extra_facets,
            text_storage: None,
        }
    }

    /// Add many documents in a single FFI call. Does not commit.
    /// For initial bulk loads – no duplicate checking. Each document's id is composed by the
    /// engine from its book's number and `ordinal`, as for [`Self::add_document`]; the
    /// stamps of a book are recorded on its first document in the batch.
    ///
    /// `text` is normalized here ([`normalize_text_for_indexing`]) and a
    /// supplied `text_vocalized` through its vocalized counterpart — the API
    /// is exposed over FFI, so the "input is already normalized" assumption
    /// is enforced rather than documented. Already-normalized input passes
    /// through the fast paths at negligible cost.
    pub fn add_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        self.ensure_write_access()?;
        self.write_inputs(docs, false)
    }

    /// Writes ready-made documents; `upsert` first deletes the documents holding their ids
    /// (of two documents with one id the later wins, as when each deleted its own id).
    fn write_inputs(&mut self, docs: Vec<DocumentInput>, upsert: bool) -> Result<()> {
        self.write_inputs_with(docs, upsert, Ids::Composed, Stamping::FirstInBatch)
    }

    fn write_inputs_with(
        &mut self,
        docs: Vec<DocumentInput>,
        upsert: bool,
        ids: Ids,
        stamping: Stamping,
    ) -> Result<()> {
        let (
            title_f,
            reference_f,
            text_f,
            id_f,
            segment_f,
            is_pdf_f,
            file_path_f,
            topics_f,
            text_vocalized_f,
            section_id_f,
            line_hash_f,
        ) = self.all_fields()?;
        let stamp_fields = StampFields::of(&self.schema)?;
        let chunk_key_f = self.chunk_key_field;
        let stored_fields = self.stored_text_fields()?;
        let note_field = self.schema.get_field(note_spans::FIELD).ok();
        // Validated before anything is written, so a bad document cannot leave half
        // a batch in the writer.
        refuse_library_storage(&docs)?;
        self.ensure_writer()?;
        let mut bases: HashMap<String, u64> = HashMap::new();
        let mut docs: Vec<(u64, DocumentInput)> = docs
            .into_iter()
            .map(|doc| {
                #[cfg(test)]
                if let Ids::Raw(id) = ids {
                    return Ok((id, doc));
                }
                let base = match bases.get(&doc.file_path) {
                    Some(&base) => base,
                    None => {
                        let base = self.id_base(&doc.file_path)?;
                        bases.insert(doc.file_path.clone(), base);
                        base
                    }
                };
                Ok((base + u64::from(doc.ordinal) + 1, doc))
            })
            .collect::<Result<_>>()?;
        if upsert {
            // One deletion for the batch, which scans the `id` column once.
            let mut seen = HashSet::new();
            docs.reverse();
            docs.retain(|(id, _)| seen.insert(*id));
            docs.reverse();
            if !seen.is_empty() {
                self.writer_mut()?
                    .delete_query(Box::new(IdSetQuery::new(seen)))?;
            }
        }
        let mut stamped: HashMap<String, Stamps> = HashMap::new();
        let writer = self.writer_mut()?;
        for (id, doc) in docs {
            let base = id & !0xFFFF_FFFF;
            let topics_facet = Facet::from_text(&doc.topics)?;
            let normalized_text = hebrew_query::normalize_text_for_indexing(&doc.text);
            let line_hash = line_dedup_hash(&normalized_text);
            let section_id = match ids {
                Ids::Composed => doc
                    .section_ordinal
                    .map_or(id, |section| base + u64::from(section) + 1),
                #[cfg(test)]
                Ids::Raw(_) => doc.section_ordinal.map_or(id, u64::from),
            };
            let stamps = Stamps {
                content: doc.content_hash.unwrap_or(0),
                text: doc.text_hash.unwrap_or(0),
                metadata: metadata_fingerprint(
                    &doc.title,
                    &doc.topics,
                    doc.extra_facets.clone().unwrap_or_default(),
                ),
                source: doc.source_hash.unwrap_or(0),
            };
            let mut document = doc!(
                title_f        => doc.title,
                reference_f    => doc.reference,
                text_f         => normalized_text.as_str(),
                id_f           => id,
                is_pdf_f       => doc.is_pdf,
                file_path_f    => doc.file_path.as_str(),
                topics_f       => topics_facet,
                section_id_f   => section_offset(id, section_id),
                line_hash_f    => line_hash
            );
            if let Some(stored) = segment_to_store(id, doc.segment) {
                document.add_u64(segment_f, stored);
            }
            // Not a line of a book's text, so no key: only `add_text_book` knows a line's
            // neighbours.
            set_chunk_key(&mut document, chunk_key_f, 0);
            let stamp_here = match stamping {
                Stamping::FirstInBatch => stamped.get(doc.file_path.as_str()) != Some(&stamps),
                Stamping::LineZero => doc.ordinal == 0,
            };
            if stamp_here {
                stamp_book(&mut document, stamp_fields, stamps);
                stamped.insert(doc.file_path.clone(), stamps);
            }
            for facet in doc.extra_facets.iter().flatten() {
                document.add_facet(topics_f, Facet::from_text(facet)?);
            }
            let vocalized = doc
                .text_vocalized
                .as_deref()
                .filter(|vocalized| !vocalized.is_empty())
                .map(hebrew_query::normalize_vocalized_text_for_indexing);
            if let Some(field) = note_field {
                let notes = NoteSpans::from_renderings(
                    &doc.text,
                    doc.text_vocalized.as_deref().unwrap_or(&doc.text),
                    &normalized_text,
                    vocalized.as_deref(),
                );
                if let Some(encoded) = notes.encode() {
                    document.add_text(field, encoded);
                }
            }
            if let Some(vocalized) = &vocalized {
                document.add_text(text_vocalized_f, vocalized);
            }
            add_stored_text(
                &mut document,
                stored_fields,
                &normalized_text,
                vocalized.as_deref(),
            );
            writer.add_document(document)?;
        }
        Ok(())
    }

    /// Indexes a whole text book in ONE FFI call. Does not commit.
    ///
    /// Splits `text` into lines, tracks the `<h…>` heading reference trail,
    /// normalizes each line ([`normalize_text_for_indexing`]), records the
    /// book's stamps ([`compute_book_fingerprint`] — text + metadata,
    /// [`compute_content_fingerprint`], [`compute_book_metadata_fingerprint`] and
    /// `source_hash` as given) on its first document, and adds one document per line.
    /// Returns the number of documents added (0 for empty text — the caller writes its
    /// empty-book marker in that case).
    ///
    /// This is the whole-book replacement for the app's per-line pipeline
    /// (Dart isolate → per-batch FFI normalize → SendPort copy → batch add):
    /// the raw text crosses the bridge exactly once and only a count comes
    /// back. Document ids are `(bookNumber << 32) + ordinal+1`, the book's number
    /// assigned by the engine on its first write (see [`Self::get_book_stamps`]); the
    /// book's place in the catalogue is [`Self::set_book_order`]'s.
    ///
    /// `text_storage`: [`TextStorage::LibraryDb`] only when `text` is the
    /// newline-joined rows of an official book read from the library database
    /// (then `file_path` must be `id:<bookId>`), or of a personal book read from the
    /// user-books database (`uid:<bookId>`) or an attached database
    /// (`db:<slug>:<bookId>`); the lines are then not stored in
    /// the index and results read them back from that database. The book is
    /// stored as [`TextStorage::InIndex`] instead (and a warning logged) when the
    /// book's configured line source does not hold exactly as many rows for it as `text`
    /// has lines — a row containing `\n`, a different database — or cannot be read
    /// (unconfigured, suspended, busy).
    ///
    /// `stored_lines`: 0-based line ordinals of a `LibraryDb` book whose text is stored
    /// in the index anyway, as for `InIndex` — the rows that held embedded `data:` images,
    /// which a result would otherwise read back raw (megabytes) on every window. Ignored
    /// for `InIndex`; ordinals past the last line are ignored.
    ///
    /// `source_hash`: an opaque hash of the book's source (the library database's
    /// `book.contentHash`), stored as given; `None`/0 for none.
    pub fn add_text_book(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        text: String,
        extra_facets: Option<Vec<String>>,
        text_storage: TextStorage,
        stored_lines: Option<Vec<u32>>,
        source_hash: Option<u64>,
    ) -> Result<u32> {
        self.ensure_write_access()?;
        self.add_text_book_impl(
            TextBookMeta {
                title,
                topics,
                file_path,
                extra_facets: extra_facets.unwrap_or_default(),
                source_hash: source_hash.unwrap_or(0),
            },
            &text,
            text_storage,
            stored_lines.unwrap_or_default(),
        )
    }

    /// [`Self::add_text_book`] over raw UTF-8 bytes. The app reads book
    /// content from SQLite, which stores UTF-8 — passing the bytes through
    /// (SQLite BLOB → `Uint8List` → here) skips the UTF-8→UTF-16→UTF-8
    /// round-trip a Dart `String` costs on the bridge (~180ms/MB measured).
    /// Invalid UTF-8 is replaced (lossy), never an error — matching what the
    /// Dart decode would have produced.
    pub fn add_text_book_bytes(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        text: Vec<u8>,
        extra_facets: Option<Vec<String>>,
        text_storage: TextStorage,
        stored_lines: Option<Vec<u32>>,
        source_hash: Option<u64>,
    ) -> Result<u32> {
        self.ensure_write_access()?;
        let text = match String::from_utf8_lossy(&text) {
            std::borrow::Cow::Borrowed(_) => {
                // Valid UTF-8 — take ownership without re-copying.
                unsafe { String::from_utf8_unchecked(text) }
            }
            std::borrow::Cow::Owned(fixed) => fixed,
        };
        self.add_text_book_impl(
            TextBookMeta {
                title,
                topics,
                file_path,
                extra_facets: extra_facets.unwrap_or_default(),
                source_hash: source_hash.unwrap_or(0),
            },
            &text,
            text_storage,
            stored_lines.unwrap_or_default(),
        )
    }

    fn add_text_book_impl(
        &mut self,
        book: TextBookMeta,
        text: &str,
        text_storage: TextStorage,
        stored_lines: Vec<u32>,
    ) -> Result<u32> {
        let library_book = match text_storage {
            TextStorage::LibraryDb => Some(require_library_book_id(&book.file_path)?),
            TextStorage::InIndex => None,
        };
        if text.is_empty() {
            return Ok(0);
        }
        self.ensure_writer()?;
        let started = Instant::now();
        let text_storage = match library_book {
            Some(source) => library_storage_or_fallback(source, line_count(text), &book.title),
            None => TextStorage::InIndex,
        };
        let mut prepared = self.prepare_text_book(&book, text, text_storage, &stored_lines)?;
        let writer = self.writer_mut()?;
        let enqueue_started = Instant::now();
        let docs = prepared.enqueue(writer)?;
        info!(
            "add_text_book '{}': {docs} docs, {} bytes in {:?} \
             (prepare {:?}, keys {:?}, enqueue {:?})",
            book.title,
            text.len(),
            started.elapsed(),
            prepared.prepare_time,
            prepared.keys_time,
            enqueue_started.elapsed()
        );
        Ok(docs)
    }

    /// The documents of a text book's lines, built without touching the writer: everything
    /// [`Self::add_text_book`] does before it enqueues them. `text_storage` is final here.
    pub(crate) fn prepare_text_book(
        &self,
        book: &TextBookMeta,
        text: &str,
        text_storage: TextStorage,
        stored_lines: &[u32],
    ) -> Result<PreparedTextBook> {
        if self.normal_fast_preparation(text) {
            return self.prepare_text_book_fast(
                book,
                text,
                text_storage,
                stored_lines,
                self.library_preparation_policy().document_bytes,
                &|| false,
            );
        }
        let mut sorted = std::borrow::Cow::Borrowed(stored_lines);
        if !stored_lines.windows(2).all(|pair| pair[0] <= pair[1]) {
            sorted.to_mut().sort_unstable();
        }
        let mut stored = sorted.iter().copied().map(Ok);
        let mut notes = note_spans::BookNotes::new(&self.index_path);
        if self.schema.get_field(note_spans::FIELD).is_ok()
            && (text_storage == TextStorage::InIndex || !stored_lines.is_empty())
        {
            notes.push_text(text)?;
        }
        self.prepare_text_book_reader(
            book,
            &mut std::io::Cursor::new(text),
            &mut notes,
            book_fingerprint(text, &book.title, &book.topics, book.extra_facets.clone()),
            content_fingerprint(text),
            text_storage,
            &mut stored,
            self.library_preparation_policy().document_bytes,
            &|| false,
        )
    }

    pub(crate) fn prepare_text_book_buffer(
        &self,
        book: &TextBookMeta,
        text: &mut crate::indexing_buffers::TextBuffer,
        text_storage: TextStorage,
        stored_lines: &mut crate::indexing_buffers::OrdinalBuffer,
        document_budget: usize,
        stop: &dyn Fn() -> bool,
    ) -> Result<PreparedTextBook> {
        use std::io::Read;
        if let Some(source) = text.as_str()? {
            if self.normal_fast_preparation(source) {
                let stored: Vec<u32> = stored_lines.iter()?.collect::<Result<_>>()?;
                return self.prepare_text_book_fast(
                    book,
                    source,
                    text_storage,
                    &stored,
                    document_budget,
                    stop,
                );
            }
        }
        let mut content = Fnv::new();
        content.feed(&text.len().to_le_bytes());
        let mut text_hash = Fnv::new();
        let mut notes = note_spans::BookNotes::new(&self.index_path);
        let collect_notes = self.schema.get_field(note_spans::FIELD).is_ok()
            && (text_storage == TextStorage::InIndex
                || stored_lines.iter()?.next().transpose()?.is_some());
        let mut reader = text.reader()?;
        let mut buffer = [0u8; 64 << 10];
        loop {
            anyhow::ensure!(!stop(), "indexing preparation interrupted");
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            content.feed(&buffer[..count]);
            text_hash.feed(&buffer[..count]);
            if collect_notes {
                notes.push(&buffer[..count])?;
            }
        }
        drop(reader);
        feed_metadata(
            &mut content,
            &book.title,
            &book.topics,
            book.extra_facets.clone(),
        );
        let mut stored = stored_lines.iter()?;
        self.prepare_text_book_reader(
            book,
            &mut text.reader()?,
            &mut notes,
            content.finish(),
            text_hash.finish(),
            text_storage,
            &mut stored,
            document_budget,
            stop,
        )
    }

    // Bound the original full-book parallel preparation by both bytes and rows.
    // Oversized books retain the streaming safety path; economy always streams.
    fn normal_fast_preparation(&self, text: &str) -> bool {
        !self.economy_indexing
            && text.len() <= self.library_preparation_policy().text_bytes
            && text
                .bytes()
                .filter(|byte| *byte == b'\n')
                .take(65_536)
                .count()
                < 65_536
    }

    fn prepare_text_book_fast(
        &self,
        book: &TextBookMeta,
        text: &str,
        text_storage: TextStorage,
        stored_lines: &[u32],
        document_budget: usize,
        stop: &dyn Fn() -> bool,
    ) -> Result<PreparedTextBook> {
        anyhow::ensure!(!stop(), "indexing preparation interrupted");
        let TextBookMeta {
            title,
            topics,
            file_path,
            extra_facets,
            source_hash,
        } = book;
        let lines: Vec<&str> = text.split('\n').collect();
        let (
            title_f,
            reference_f,
            text_f,
            id_f,
            segment_f,
            is_pdf_f,
            file_path_f,
            topics_f,
            text_vocalized_f,
            section_id_f,
            line_hash_f,
        ) = self.all_fields()?;
        let stamp_fields = StampFields::of(&self.schema)?;
        let topics_facet = Facet::from_text(&topics)?;
        // נתיבי הממדים (מחבר/תקופה/יסוד) נפרסים פעם אחת לספר ומוטבעים
        // כערכי facet נוספים על כל שורה.
        let extra_facet_values: Vec<Facet> = extra_facets
            .iter()
            .map(Facet::from_text)
            .collect::<std::result::Result<_, _>>()?;
        let stamps = Stamps {
            content: book_fingerprint(text, title, topics, extra_facets.clone()),
            text: content_fingerprint(text),
            metadata: metadata_fingerprint(title, topics, extra_facets.clone()),
            source: *source_hash,
        };
        let book_number = self.book_number(file_path)?;
        let id_base = u64::from(book_number) << 32;
        let chunk_key_f = self.chunk_key_field;
        let library = text_storage == TextStorage::LibraryDb;
        let mut kept_in_index = Vec::new();
        if library && !stored_lines.is_empty() {
            kept_in_index = vec![false; lines.len()];
            for &ordinal in stored_lines {
                if let Some(kept) = kept_in_index.get_mut(ordinal as usize) {
                    *kept = true;
                }
            }
        }
        let stores_text = |segment: usize| !library || kept_in_index.get(segment) == Some(&true);
        let stored_fields = self.stored_text_fields()?;
        let note_field = self.schema.get_field(note_spans::FIELD).ok();
        let line_check_f = self.schema.get_field(LINE_CHECK_FIELD)?;
        let edge_fields = cross_line::EdgeFields::from_schema(&self.schema)?;
        let edge_analyzer = self.text_index_analyzer()?;

        // "prepare" — the pure-CPU phase (trail + normalization); "enqueue" —
        // writer.add_document (queue push; grows only when tantivy's indexing
        // threads apply backpressure).
        let prepare_started = Instant::now();

        // Sequential cheap pass: the reference trail is stateful across
        // lines, so resolve each line to its reference index first. The
        // stripped trail is recomputed only when a heading changes it.
        let mut trail: Vec<&str> = Vec::new();
        let mut references: Vec<String> = vec![String::new()];
        let mut reference_of_line: Vec<u32> = Vec::with_capacity(lines.len());
        for raw_line in &lines {
            if raw_line.starts_with("<h") {
                update_reference_trail(&mut trail, raw_line);
                // כיווץ אחרי ההסרה: תג הכותרת עצמו (תג שבירה) הופך לרווח.
                references.push(
                    trail
                        .iter()
                        .map(|part| {
                            hebrew_query::collapse_whitespace(
                                &hebrew_query::strip_html_for_indexing(part),
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            reference_of_line.push((references.len() - 1) as u32);
        }

        // Normalize on the bounded indexing pool, independently of search workers.
        use rayon::prelude::*;
        type Prepared = (
            String,
            Option<String>,
            u64,
            Option<u32>,
            Option<cross_line::LineEdges>,
            Option<String>,
        );
        anyhow::ensure!(!stop(), "indexing preparation interrupted");
        let mut source_notes = note_spans::BookNotes::new(&self.index_path);
        if note_field.is_some() && (!library || !stored_lines.is_empty()) {
            source_notes.push_text(text)?;
        }
        let mut note_reader = source_notes.reader()?;
        let mut source_offset = 0;
        let line_notes = lines
            .iter()
            .map(|line| {
                let notes = note_reader.line(source_offset, line.len());
                source_offset += line.len() + 1;
                notes
            })
            .collect::<Result<Vec<_>>>()?;
        let normalized: Vec<Prepared> = crate::indexing_buffers::preparation_cpu(
            self.library_preparation_policy().cpu_threads,
            || {
                lines
                    .par_iter()
                    .enumerate()
                    .map_init(
                        || edge_analyzer.clone(),
                        |analyzer, (segment, raw_line)| {
                            let (plain, vocalized) =
                                hebrew_query::normalize_line_for_indexing(raw_line);
                            let line_hash = line_dedup_hash(&plain);
                            let line_check = (!stores_text(segment))
                                .then(|| crate::line_source::line_check(raw_line));
                            // A heading interrupts the text: no phrase continues into or out of it.
                            let edges = (!raw_line.starts_with("<h"))
                                .then(|| cross_line::line_edges(analyzer, &plain))
                                .flatten();
                            let notes = (note_field.is_some() && stores_text(segment))
                                .then(|| {
                                    line_notes[segment]
                                        .normalize(raw_line, &plain, vocalized.as_deref())
                                        .encode()
                                })
                                .flatten();
                            (plain, vocalized, line_hash, line_check, edges, notes)
                        },
                    )
                    .collect()
            },
        )?;
        let prepare_time = prepare_started.elapsed();

        // The key each line's vector is stored under: the production recipe over the text
        // the index stores and the sections the headings open, which the passes above just
        // made. Only for a column in use — one written under another recipe counts as
        // absent.
        anyhow::ensure!(!stop(), "indexing preparation interrupted");
        let keys_started = Instant::now();
        let chunk_keys: Vec<u64> = crate::indexing_buffers::preparation_cpu(
            self.library_preparation_policy().cpu_threads,
            || match chunk_key_f {
                Some(_) => {
                    let lines: Vec<LineRef<'_>> = normalized
                        .iter()
                        .zip(&reference_of_line)
                        .map(|((plain, _, _, _, _, _), section)| LineRef {
                            text: plain,
                            section: u64::from(*section),
                        })
                        .collect();
                    semantic_keys::column_values(&lines)
                }
                None => Vec::new(),
            },
        )?;
        let keys_time = keys_started.elapsed();
        anyhow::ensure!(!stop(), "indexing preparation interrupted");

        let mut documents = Vec::with_capacity(lines.len());
        let mut spooled_documents: Option<crate::indexing_buffers::DocumentSpool> = None;
        let mut buffered_documents = crate::indexing_diagnostics::BufferedBytes::default();
        let mut document_bytes = 0usize;
        for (segment, (normalized_line, vocalized_line, line_hash, line_check, edges, notes)) in
            normalized.into_iter().enumerate()
        {
            anyhow::ensure!(!stop(), "indexing preparation interrupted");
            let reference = references[reference_of_line[segment] as usize].as_str();
            let id = id_base + segment as u64 + 1;
            let mut document = doc!(
                title_f        => title.as_str(),
                reference_f    => reference,
                text_f         => normalized_line.as_str(),
                id_f           => id,
                is_pdf_f       => false,
                file_path_f    => file_path.as_str(),
                topics_f       => topics_facet.clone(),
                // כל השורות של אותו בלוק כותרת חולקות ערך; id_base מבדל
                // בין ספרים, אז sectionId הנגזר ייחודי גלובלית.
                section_id_f   => u64::from(reference_of_line[segment]),
                line_hash_f    => line_hash
            );
            if let Some(stored) = segment_to_store(id, segment as u64) {
                document.add_u64(segment_f, stored);
            }
            let chunk_key = chunk_keys.get(segment).copied().unwrap_or(0);
            set_chunk_key(&mut document, chunk_key_f, chunk_key);
            if segment == 0 {
                stamp_book(&mut document, stamp_fields, stamps);
            }
            for facet in &extra_facet_values {
                document.add_facet(topics_f, facet.clone());
            }
            if let Some(vocalized) = &vocalized_line {
                document.add_text(text_vocalized_f, vocalized);
            }
            if let Some(edges) = &edges {
                edge_fields.add(&mut document, edges, true, true);
            }
            if let (Some(field), Some(notes)) = (note_field, notes) {
                document.add_text(field, notes);
            }
            if let Some(check) = line_check {
                document.add_u64(line_check_f, u64::from(check));
            } else {
                add_stored_text(
                    &mut document,
                    stored_fields,
                    &normalized_line,
                    vocalized_line.as_deref(),
                );
            }
            let bytes = document.node_data.capacity()
                + document.len() * 16
                + std::mem::size_of::<TantivyDocument>();
            if spooled_documents.is_none() && document_bytes.saturating_add(bytes) > document_budget
            {
                let mut spool =
                    crate::indexing_buffers::DocumentSpool::new(&self.schema, &self.index_path)?;
                for previous in documents.drain(..) {
                    spool.push(&previous)?;
                }
                documents = Vec::new();
                buffered_documents.set(64 << 10);
                spooled_documents = Some(spool);
            }
            if let Some(spool) = &mut spooled_documents {
                spool.push(&document)?;
            } else {
                document_bytes += bytes;
                documents.push(document);
                buffered_documents.set_document_bytes(document_bytes);
            }
        }
        crate::indexing_diagnostics::prepare(prepare_started.elapsed());
        Ok(PreparedTextBook {
            documents,
            spooled_documents,
            buffered_documents,
            source_generation: None,
            book_number,
            content_hash: stamps.content,
            text_hash: stamps.text,
            metadata_hash: stamps.metadata,
            source_hash: stamps.source,
            text_storage,
            prepare_time,
            keys_time,
        })
    }

    fn prepare_text_book_reader(
        &self,
        book: &TextBookMeta,
        reader: &mut dyn std::io::BufRead,
        source_notes: &mut note_spans::BookNotes,
        content_hash: u64,
        text_hash: u64,
        text_storage: TextStorage,
        stored_lines: &mut dyn Iterator<Item = Result<u32>>,
        document_budget: usize,
        stop: &dyn Fn() -> bool,
    ) -> Result<PreparedTextBook> {
        use rayon::prelude::*;
        use std::sync::Arc;
        struct Line {
            plain: String,
            vocalized: Option<String>,
            hash: u64,
            check: Option<u32>,
            edges: Option<cross_line::LineEdges>,
            notes: Option<String>,
            reference: Arc<String>,
            section: u32,
            ordinal: usize,
        }
        let prepare_started = Instant::now();
        let (
            title_f,
            reference_f,
            text_f,
            id_f,
            segment_f,
            is_pdf_f,
            file_path_f,
            topics_f,
            text_vocalized_f,
            section_id_f,
            line_hash_f,
        ) = self.all_fields()?;
        let stamp_fields = StampFields::of(&self.schema)?;
        let topics_facet = Facet::from_text(&book.topics)?;
        let extra_facets: Vec<Facet> = book
            .extra_facets
            .iter()
            .map(Facet::from_text)
            .collect::<std::result::Result<_, _>>()?;
        let stamps = Stamps {
            content: content_hash,
            text: text_hash,
            metadata: metadata_fingerprint(&book.title, &book.topics, book.extra_facets.clone()),
            source: book.source_hash,
        };
        let book_number = self.book_number(&book.file_path)?;
        let id_base = u64::from(book_number) << 32;
        let stored_fields = self.stored_text_fields()?;
        let note_field = self.schema.get_field(note_spans::FIELD).ok();
        let mut note_reader = source_notes.reader()?;
        let mut source_offset = 0;
        let line_check_f = self.schema.get_field(LINE_CHECK_FIELD)?;
        let edge_fields = cross_line::EdgeFields::from_schema(&self.schema)?;
        let edge_analyzer = self.text_index_analyzer()?;
        let mut next_stored = stored_lines.next().transpose()?;
        let mut documents = Vec::new();
        let mut spooled_documents: Option<crate::indexing_buffers::DocumentSpool> = None;
        let mut buffered_documents = crate::indexing_diagnostics::BufferedBytes::default();
        let mut document_bytes = 0usize;
        let mut trail: Vec<String> = Vec::new();
        let mut reference = Arc::new(String::new());
        let mut section = 0u32;
        let mut ordinal = 0usize;
        let mut window: Vec<Line> = Vec::new();
        let mut previous = 0usize;
        let mut finished = false;
        let mut keys_time = std::time::Duration::ZERO;
        let reach = semantic_keys::production_chunking().context_window_lines;
        let policy = self.library_preparation_policy();
        while !finished || previous < window.len() {
            anyhow::ensure!(!stop(), "indexing preparation interrupted");
            let mut raw_lines = Vec::new();
            let mut raw_bytes = 0usize;
            while !finished
                && raw_lines.len() < policy.window_lines
                && raw_bytes < policy.window_bytes
            {
                let mut raw = String::new();
                let read = reader.read_line(&mut raw)?;
                finished = !raw.ends_with('\n');
                if raw.ends_with('\n') {
                    raw.pop();
                }
                if raw.starts_with("<h") {
                    let mut borrowed: Vec<&str> = trail.iter().map(String::as_str).collect();
                    update_reference_trail(&mut borrowed, &raw);
                    trail = borrowed.into_iter().map(str::to_owned).collect();
                    reference = Arc::new(
                        trail
                            .iter()
                            .map(|part| {
                                hebrew_query::collapse_whitespace(
                                    &hebrew_query::strip_html_for_indexing(part),
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                    section += 1;
                }
                while next_stored.is_some_and(|next| (next as usize) < ordinal) {
                    next_stored = stored_lines.next().transpose()?;
                }
                let stores_text =
                    text_storage != TextStorage::LibraryDb || next_stored == Some(ordinal as u32);
                let notes = note_reader.line(source_offset, raw.len())?;
                source_offset += read;
                raw_bytes += raw.len();
                raw_lines.push((raw, reference.clone(), section, ordinal, stores_text, notes));
                ordinal += 1;
                if read == 0 {
                    finished = true;
                }
            }
            anyhow::ensure!(!stop(), "indexing preparation interrupted");
            let normalized: Vec<Line> = crate::indexing_buffers::preparation_cpu(
                self.library_preparation_policy().cpu_threads,
                || {
                    raw_lines
                        .par_iter()
                        .map_init(
                            || edge_analyzer.clone(),
                            |analyzer, (raw, reference, section, ordinal, stores_text, source_notes)| {
                                let (plain, vocalized) =
                                    hebrew_query::normalize_line_for_indexing(raw);
                                let hash = line_dedup_hash(&plain);
                                let check =
                                    (!stores_text).then(|| crate::line_source::line_check(raw));
                                let edges = (!raw.starts_with("<h"))
                                    .then(|| cross_line::line_edges(analyzer, &plain))
                                    .flatten();
                                let notes = (note_field.is_some() && *stores_text)
                                    .then(|| {
                                        source_notes.normalize(raw, &plain, vocalized.as_deref())
                                            .encode()
                                    })
                                    .flatten();
                                Line {
                                    plain,
                                    vocalized,
                                    hash,
                                    check,
                                    edges,
                                    notes,
                                    reference: reference.clone(),
                                    section: *section,
                                    ordinal: *ordinal,
                                }
                            },
                        )
                        .collect()
                },
            )?;
            drop(raw_lines);
            window.extend(normalized);
            let end = if finished {
                window.len()
            } else {
                window.len().saturating_sub(reach)
            };
            let keys_started = Instant::now();
            let keys = if self.chunk_key_field.is_some() {
                crate::indexing_buffers::preparation_cpu(
                    self.library_preparation_policy().cpu_threads,
                    || {
                        let lines: Vec<LineRef<'_>> = window
                            .iter()
                            .map(|line| LineRef {
                                text: &line.plain,
                                section: u64::from(line.section),
                            })
                            .collect();
                        semantic_keys::column_values(&lines)
                    },
                )?
            } else {
                Vec::new()
            };
            keys_time += keys_started.elapsed();
            for (offset, line) in window.iter().enumerate().take(end).skip(previous) {
                anyhow::ensure!(!stop(), "indexing preparation interrupted");
                let segment = line.ordinal;
                let id = id_base + segment as u64 + 1;
                let mut document = doc!(title_f => book.title.as_str(),
                    reference_f => line.reference.as_str(), text_f => line.plain.as_str(),
                    id_f => id, is_pdf_f => false, file_path_f => book.file_path.as_str(),
                    topics_f => topics_facet.clone(), section_id_f => u64::from(line.section),
                    line_hash_f => line.hash);
                if let Some(stored) = segment_to_store(id, segment as u64) {
                    document.add_u64(segment_f, stored);
                }
                set_chunk_key(
                    &mut document,
                    self.chunk_key_field,
                    keys.get(offset).copied().unwrap_or(0),
                );
                if segment == 0 {
                    stamp_book(&mut document, stamp_fields, stamps);
                }
                for facet in &extra_facets {
                    document.add_facet(topics_f, facet.clone());
                }
                if let Some(vocalized) = &line.vocalized {
                    document.add_text(text_vocalized_f, vocalized);
                }
                if let Some(edges) = &line.edges {
                    edge_fields.add(&mut document, edges, true, true);
                }
                if let (Some(field), Some(notes)) = (note_field, &line.notes) {
                    document.add_text(field, notes);
                }
                if let Some(check) = line.check {
                    document.add_u64(line_check_f, u64::from(check));
                } else {
                    add_stored_text(
                        &mut document,
                        stored_fields,
                        &line.plain,
                        line.vocalized.as_deref(),
                    );
                }
                let bytes = document.node_data.capacity()
                    + document.len() * 16
                    + std::mem::size_of::<TantivyDocument>();
                if spooled_documents.is_none()
                    && document_bytes.saturating_add(bytes) > document_budget
                {
                    let mut spool = crate::indexing_buffers::DocumentSpool::new(
                        &self.schema,
                        &self.index_path,
                    )?;
                    for previous in documents.drain(..) {
                        spool.push(&previous)?;
                    }
                    documents = Vec::new();
                    buffered_documents.set(64 << 10);
                    spooled_documents = Some(spool);
                }
                if let Some(spool) = &mut spooled_documents {
                    spool.push(&document)?;
                } else {
                    document_bytes += bytes;
                    documents.push(document);
                    buffered_documents.set_document_bytes(document_bytes);
                }
            }
            if finished {
                break;
            }
            previous = reach.min(end);
            window.drain(..end.saturating_sub(reach));
        }
        let prepare_time = prepare_started.elapsed();
        crate::indexing_diagnostics::prepare(prepare_time);
        Ok(PreparedTextBook {
            documents,
            spooled_documents,
            buffered_documents,
            source_generation: None,
            book_number,
            content_hash: stamps.content,
            text_hash: stamps.text,
            metadata_hash: stamps.metadata,
            source_hash: stamps.source,
            text_storage,
            prepare_time,
            keys_time,
        })
    }

    /// Indexes books whose text the engine reads itself, from the library database
    /// ([`configure_line_source`]), the user-books database
    /// ([`configure_user_books_line_source`]) or an attached database
    /// ([`configure_attached_line_source`]). Does not commit.
    ///
    /// Each book is indexed exactly as `add_text_book_bytes` indexes the text the app reads
    /// for it: rows in `lineIndex` order joined with `\n`, embedded `data:` images removed
    /// (those rows then stored in the index, as `stored_lines`), the same documents,
    /// `contentHash` and `textHash`. Several books are read and prepared at once; they
    /// are added in input order, each whole or not at all.
    ///
    /// Takes the engine by shared reference, so searches are not held up while the batch
    /// prepares; it needs an open writer ([`Self::open_index_writer`]). `cancel` stops the
    /// batch before its next book. One result per input book, in order. An `Err` means
    /// the writer failed: books of this batch may be in it, as after a failed
    /// `add_text_book_bytes`.
    pub fn add_library_books(
        &self,
        books: Vec<LibraryBookInput>,
        cancel: &IndexingCancellationToken,
    ) -> Result<Vec<LibraryBookResult>> {
        self.ensure_write_access()?;
        let writer = self.index_writer.as_ref().context(
            "add_library_books needs an open index writer: call open_index_writer first",
        )?;
        crate::library_indexing::add_library_books(self, writer, books, Some(cancel))
    }

    /// Opens the writer on demand, but fails after [`Self::close_index_writer`].
    /// [`Self::add_library_books`] does not open it itself.
    pub fn open_index_writer(&mut self) -> Result<()> {
        self.ensure_write_access()?;
        self.ensure_writer()
    }

    /// Permanently closes this engine's writer without committing, before disposal.
    /// Runs off the UI; queued writes cannot reopen the writer after it closes.
    pub fn close_index_writer(&mut self) {
        self.writer_closed = true;
        drop(self.index_writer.take());
    }

    /// Indexes a whole PDF book in ONE FFI call. Does not commit.
    ///
    /// The whole-book replacement for the app's per-page PDF pipeline (Dart
    /// isolate → per-window FFI normalize → SendPort copy → batch add), which
    /// copied the extracted text four-five times per book. Each page's text is
    /// split into lines; every line is normalised
    /// ([`normalize_pdf_text_for_indexing`]) and dropped when the garbage
    /// heuristic ([`is_probably_garbage_pdf_text`]) flags it — exactly the
    /// per-line logic of [`normalize_pdf_texts_for_indexing`]. One document is
    /// added per surviving line, with `segment` = the page's `page_index` and
    /// ids composed like [`Self::add_text_book`]'s. PDFs record their
    /// `metadataHash` and no content fingerprint (`contentHash` = 0, their text is
    /// not in the library DB).
    ///
    /// Returns the number of documents added; 0 means the PDF yielded no
    /// usable text (scanned/garbage) — the caller falls back to a sidecar or
    /// writes its empty-book marker.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pdf_book(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        pages: Vec<PdfPageInput>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<u32> {
        self.ensure_write_access()?;
        let pages = pages
            .into_iter()
            .map(|page| PdfPageText {
                reference: Cow::Owned(page.reference),
                text: Cow::Owned(page.text),
                page_index: page.page_index,
            })
            .collect();
        self.add_pdf_pages(title, topics, file_path, pages, extra_facets)
    }

    fn add_pdf_pages(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        pages: Vec<PdfPageText<'_>>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<u32> {
        if pages.is_empty() {
            return Ok(0);
        }
        let started = Instant::now();
        let text_bytes: usize = pages.iter().map(|p| p.text.len()).sum();
        let page_count = pages.len();
        let (
            title_f,
            reference_f,
            text_f,
            id_f,
            segment_f,
            is_pdf_f,
            file_path_f,
            topics_f,
            // PDF אינו משתתף בחיפוש מנוקד: ניקוד שמגיע מ-OCR אינו אמין,
            // והנרמול של PDF ממילא מוחק אותו.
            _text_vocalized_f,
            section_id_f,
            line_hash_f,
        ) = self.all_fields()?;
        let stamp_fields = StampFields::of(&self.schema)?;
        let extra_facets = extra_facets.unwrap_or_default();
        let stamps = Stamps {
            metadata: metadata_fingerprint(&title, &topics, extra_facets.clone()),
            ..Stamps::default()
        };
        let topics_facet = Facet::from_text(&topics)?;
        let extra_facet_values: Vec<Facet> = extra_facets
            .iter()
            .map(Facet::from_text)
            .collect::<std::result::Result<_, _>>()?;
        self.ensure_writer()?;
        let id_base = self.id_base(&file_path)?;
        let chunk_key_f = self.chunk_key_field;
        let stored_fields = self.stored_text_fields()?;
        let edge_fields = cross_line::EdgeFields::from_schema(&self.schema)?;
        let edge_analyzer = self.text_index_analyzer()?;
        let preparation_threads = self.library_preparation_policy().cpu_threads;
        let writer = self.writer_mut()?;

        // Normalize on the bounded indexing pool, then enqueue sequentially.
        use rayon::prelude::*;
        let prepare_started = Instant::now();
        let lines: Vec<(usize, &str)> = pages
            .iter()
            .enumerate()
            .flat_map(|(page_idx, page)| page.text.split('\n').map(move |line| (page_idx, line)))
            .collect();
        type Prepared = (usize, String, bool, Option<cross_line::LineEdges>);
        let prepared: Vec<Prepared> =
            crate::indexing_buffers::preparation_cpu(preparation_threads, || {
                lines
                    .par_iter()
                    .map_init(
                        || edge_analyzer.clone(),
                        |analyzer, (page_idx, raw_line)| {
                            let normalized =
                                hebrew_query::normalize_pdf_text_for_indexing(raw_line);
                            let is_garbage =
                                hebrew_query::is_probably_garbage_pdf_text(&normalized);
                            let edges = (!is_garbage)
                                .then(|| cross_line::line_edges(analyzer, &normalized))
                                .flatten();
                            (*page_idx, normalized, is_garbage, edges)
                        },
                    )
                    .collect()
            })?;
        let prepare_time = prepare_started.elapsed();
        // A dropped line sits between its neighbours' consecutive ids: no
        // phrase may continue across it.
        let garbage: Vec<bool> = prepared.iter().map(|p| p.2).collect();

        let enqueue_started = Instant::now();
        let mut ordinal: u64 = 0;
        let mut garbage_lines: u64 = 0;
        for (line, (page_idx, normalized, is_garbage, edges)) in prepared.into_iter().enumerate() {
            if is_garbage {
                garbage_lines += 1;
                continue;
            }
            let page = &pages[page_idx];
            let id = id_base + ordinal + 1;
            let line_hash = line_dedup_hash(&normalized);
            let mut document = doc!(
                title_f        => title.as_str(),
                reference_f    => page.reference.as_ref(),
                text_f         => normalized.as_str(),
                id_f           => id,
                is_pdf_f       => true,
                file_path_f    => file_path.as_str(),
                topics_f       => topics_facet.clone(),
                // ב-PDF אין שרשרת כותרות — עמוד = סעיף.
                section_id_f   => u64::from(page.page_index),
                line_hash_f    => line_hash
            );
            if ordinal == 0 {
                stamp_book(&mut document, stamp_fields, stamps);
            }
            if let Some(stored) = segment_to_store(id, u64::from(page.page_index)) {
                document.add_u64(segment_f, stored);
            }
            // A PDF's lines are not the library's text, so no vector is built from them.
            set_chunk_key(&mut document, chunk_key_f, 0);
            for facet in &extra_facet_values {
                document.add_facet(topics_f, facet.clone());
            }
            if let Some(edges) = &edges {
                let can_start = line == 0 || !garbage[line - 1];
                let can_end = !garbage.get(line + 1).copied().unwrap_or(false);
                edge_fields.add(&mut document, edges, can_start, can_end);
            }
            add_stored_text(&mut document, stored_fields, &normalized, None);
            writer.add_document(document)?;
            ordinal += 1;
        }
        let enqueue_time = enqueue_started.elapsed();
        info!(
            "add_pdf_book '{title}': {ordinal} docs from {page_count} pages \
             ({garbage_lines} garbage lines), {text_bytes} bytes in {:?} \
             (prepare {prepare_time:?}, enqueue {enqueue_time:?})",
            started.elapsed()
        );
        Ok(ordinal as u32)
    }

    /// [`Self::add_pdf_book`] with the pages packed into one UTF-8 buffer:
    /// page `i` is `ends[2i]` (end of its reference) and `ends[2i + 1]` (end of
    /// its text), each slice starting where the previous one ended, and its
    /// index is `page_indexes[i]`.
    ///
    /// The bridge decodes arguments on the caller's thread, and decoding a large
    /// `Vec<PdfPageInput>` there froze the app's UI (Otzaria/otzaria#3443). A
    /// buffer the app packs off its UI isolate crosses as a copy; UTF-8 is
    /// checked here, on the worker. Valid slices borrow the input buffer without
    /// per-page text copies. Invalid UTF-8 (including a split code point) is
    /// replaced. End offsets must be nondecreasing and cover the entire buffer;
    /// equal offsets allow empty references/texts. Malformed offsets return an
    /// error before any document is added.
    ///
    /// Packing off the UI isolate does not remove the bridge's synchronous
    /// O(bytes) transfer. Use Uint8List/Uint32List on Dart to avoid list conversion;
    /// large books can still block the caller during the transfer.
    #[allow(clippy::too_many_arguments)]
    pub fn add_pdf_book_bytes(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        bytes: Vec<u8>,
        ends: Vec<u32>,
        page_indexes: Vec<u32>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<u32> {
        self.ensure_write_access()?;
        let pages = pdf_pages_from_bytes(&bytes, &ends, &page_indexes)?;
        self.add_pdf_pages(title, topics, file_path, pages, extra_facets)
    }

    /// Delete then re-insert a single line, by the id [`Self::add_document`] composes for
    /// it. Does not commit.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_document(
        &mut self,
        ordinal: u32,
        title: &str,
        reference: &str,
        topics: &str,
        text: &str,
        segment: u64,
        is_pdf: bool,
        file_path: &str,
        section_ordinal: Option<u32>,
        extra_facets: Option<Vec<String>>,
    ) -> Result<()> {
        self.ensure_write_access()?;
        let document = Self::single_input(
            ordinal,
            title,
            reference,
            topics,
            text,
            segment,
            is_pdf,
            file_path,
            section_ordinal,
            extra_facets,
        );
        self.write_inputs_with(vec![document], true, Ids::Composed, Stamping::LineZero)
    }

    /// Upsert many documents in a single FFI call. Does not commit.
    /// Normalizes `text`/`text_vocalized` exactly like
    /// [`Self::add_documents_batch`].
    pub fn upsert_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        self.ensure_write_access()?;
        self.write_inputs(docs, true)
    }

    /// Whether the index this engine opened is one this build reads.
    ///
    /// `pub(crate)`: the FFI already exposes [`check_index_compatibility`] by path. This is
    /// the same answer for the directory already open, so a build does not have to be told
    /// again where it is.
    #[cfg(feature = "semantic-integration")]
    pub(crate) fn index_compatibility(&self) -> IndexCompatibility {
        check_index_compatibility_path(&self.index_path)
    }

    /// A snapshot of the index for the semantic builder to read the corpus from.
    ///
    /// `pub(crate)`, so flutter_rust_bridge never sees it: Dart has no use for a
    /// `Searcher`, and the corpus port is a Rust-to-Rust contract with the sidecar. One
    /// call is one snapshot — see [`TantivyCorpus`](crate::semantic_corpus::TantivyCorpus),
    /// which holds it for a whole build precisely so a reload cannot move the corpus
    /// underneath one.
    #[cfg(any(test, feature = "semantic-integration"))]
    pub(crate) fn corpus_searcher(&self) -> Searcher {
        self.index_reader.searcher()
    }

    /// Delete a document by its numeric id. Does not commit.
    pub fn delete_document_by_id(&mut self, id: u64) -> Result<()> {
        self.ensure_write_access()?;
        self.writer_mut()?
            .delete_query(Box::new(IdSetQuery::new([id])))?;
        Ok(())
    }

    /// Delete every document of one book, addressed by its `filePath` value —
    /// the stable book key the app stamps on all of a book's documents at
    /// indexing time. `filePath` is a raw STRING field, so this is a single
    /// exact `delete_term` and cannot touch other books (unlike
    /// [`Self::remove_documents_by_title`], which matches any book sharing the
    /// title). Does not commit.
    pub fn delete_documents_by_file_path(&mut self, file_path: &str) -> Result<()> {
        self.ensure_write_access()?;
        let file_path_f = self.schema.get_field("filePath")?;
        self.writer_mut()?
            .delete_term(Term::from_field_text(file_path_f, file_path));
        Ok(())
    }

    /// Batch form of [`Self::delete_documents_by_file_path`] — one FFI call
    /// for e.g. removing a whole custom folder of personal books. Does not
    /// commit.
    pub fn delete_documents_by_file_paths(&mut self, file_paths: Vec<String>) -> Result<()> {
        self.ensure_write_access()?;
        let file_path_f = self.schema.get_field("filePath")?;
        let writer = self.writer_mut()?;
        for path in file_paths {
            writer.delete_term(Term::from_field_text(file_path_f, &path));
        }
        Ok(())
    }

    /// Delete all documents matching a title. Does not commit.
    /// Kept for backward compatibility – prefer delete_document_by_id.
    pub fn remove_documents_by_title(&mut self, title: &str) -> Result<()> {
        self.ensure_write_access()?;
        let title_field = self.schema.get_field("title")?;
        self.writer_mut()?
            .delete_term(Term::from_field_text(title_field, title));
        Ok(())
    }

    /// Delete all documents. Does not commit.
    pub fn clear(&mut self) -> Result<()> {
        self.ensure_write_access()?;
        self.writer_mut()?.delete_all_documents()?;
        Ok(())
    }

    /// Bulk-indexing mode: while enabled, the live writer skips background
    /// segment merges (`NoMergePolicy`). During a full-library build, even the
    /// size-aware policy can repeatedly merge intermediate segments before
    /// the caller runs `optimize`. Call with `true` before a bulk
    /// build and `false` when done — `optimize` does NOT reset the flag, and
    /// while it is set every (re)opened writer keeps `NoMergePolicy`. Off by
    /// default; incremental indexing keeps normal merging.
    pub fn set_bulk_indexing(&mut self, enabled: bool) -> Result<()> {
        self.ensure_write_access()?;
        self.bulk_indexing = enabled;
        let index_path = self.index_path.clone();
        let writer = self.writer_mut()?;
        if enabled {
            writer.set_merge_policy(Box::new(NoMergePolicy));
        } else {
            writer.set_merge_policy(Box::new(SizeAwareMergePolicy { index_path }));
        }
        debug!("bulk_indexing={enabled}");
        Ok(())
    }

    pub(crate) fn preparation_directory(&self) -> &Path {
        &self.index_path
    }

    pub(crate) fn library_preparation_policy(&self) -> crate::indexing_buffers::PreparationPolicy {
        crate::indexing_buffers::PreparationPolicy::new(
            self.economy_indexing,
            std::thread::available_parallelism().map_or(1, |n| n.get()),
        )
    }

    /// Economy mode uses one preparation and one writer thread, small spilling buffers
    /// and a 50MB writer. Normal mode limits CPU concurrency for foreground work and
    /// adapts preparation workers to available CPUs and platform. At most four workers
    /// reserve 384 MiB of source/document buffers, excluding normalized text, SQLite
    /// and the writer; this is not a process RSS limit. Bulk merge policy is independent.
    /// Live writer changes commit pending documents before replacement.
    pub fn set_economy_indexing(&mut self, enabled: bool) -> Result<()> {
        self.ensure_write_access()?;
        self.set_economy_indexing_with_reload(enabled, |reader| Ok(reader.reload()?))
    }

    fn set_economy_indexing_with_reload(
        &mut self,
        enabled: bool,
        reload: impl FnOnce(&IndexReader) -> Result<()>,
    ) -> Result<()> {
        let target = if enabled {
            ECONOMY_WRITER_HEAP_SIZE
        } else {
            DEFAULT_WRITER_HEAP_SIZE
        };
        if target != self.writer_heap_size {
            if let Some(mut writer) = self.index_writer.take() {
                if let Err(error) = writer.commit() {
                    self.index_writer = Some(writer);
                    return Err(error.into());
                }
                writer.wait_merging_threads()?;
                let previous = self.writer_heap_size;
                let previous_economy = self.economy_indexing;
                self.writer_heap_size = target;
                self.economy_indexing = enabled;
                if let Err(error) = self.restore_writer() {
                    self.writer_heap_size = previous;
                    self.economy_indexing = previous_economy;
                    return Err(error);
                }
                if let Err(error) = reload(&self.index_reader) {
                    self.writer_heap_size = previous;
                    self.economy_indexing = previous_economy;
                    let closing = self
                        .index_writer
                        .take()
                        .expect("the replacement writer")
                        .wait_merging_threads();
                    let restoring = self.restore_writer();
                    if let Err(restore_error) = restoring {
                        return Err(error.context(format!(
                            "restoring the previous writer failed: {restore_error:#}"
                        )));
                    }
                    if let Err(close_error) = closing {
                        return Err(error.context(format!(
                            "closing the replacement writer failed: {close_error}"
                        )));
                    }
                    return Err(error);
                }
            } else {
                self.writer_heap_size = target;
            }
        }
        self.economy_indexing = enabled;
        debug!("economy_indexing={enabled} (writer budget {target} bytes)");
        Ok(())
    }

    /// Flush pending writes to disk and refresh the reader.
    pub fn commit(&mut self) -> Result<()> {
        self.ensure_write_access()?;
        let started = Instant::now();
        self.writer_mut()?.commit()?;
        let commit_elapsed = started.elapsed();
        let reload_started = Instant::now();
        self.index_reader.reload()?;
        self.page_cache.clear();
        self.highlight_cache.clear();
        info!(
            "commit: {commit_elapsed:?} (reader reload {:?})",
            reload_started.elapsed()
        );
        Ok(())
    }

    /// Discard all pending writes since the last commit.
    pub fn rollback(&mut self) -> Result<()> {
        self.ensure_write_access()?;
        self.writer_mut()?.rollback()?;
        // Tantivy recreates its writer and resets the merge policy on rollback.
        self.set_bulk_indexing(self.bulk_indexing)?;
        Ok(())
    }

    // ── Book numbers and order ─────────────────────────────────────────────────

    /// Replaces the whole order mapping: the catalogue and generation position of each book,
    /// by its `filePath`. Catalogue and generation searches sort by it from the next call on,
    /// with no reindexing; a book absent from it sorts after every book in it. Stored in the
    /// index directory, so every engine that opens the index sorts the same way.
    ///
    /// Returns the mapping's revision, which grows on every change; the same mapping again
    /// changes nothing. A `file_path` given twice is an error, and the mapping is kept.
    pub fn set_book_order(&self, entries: Vec<BookOrderEntry>) -> Result<u64> {
        self.ensure_write_access()?;
        let mut places = HashMap::with_capacity(entries.len());
        for entry in entries {
            let place = BookPlace {
                catalogue_order: entry.catalogue_order,
                generation_order: entry.generation_order,
            };
            if places.insert(entry.file_path.clone(), place).is_some() {
                anyhow::bail!("set_book_order: {:?} is given twice", entry.file_path);
            }
        }
        let mut state = self
            .book_order
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // Independent of the index writer: a reader may replace catalogue order while
        // another engine indexes. All instances/processes serialize the whole transaction.
        // Lock order is local book_order, then this file lock; refreshing takes only the
        // local lock, and neither operation takes book_numbers or the index writer.
        let directory = IndexDirectory::open(&self.index_path)?;
        let _order_lock = directory.acquire_lock(&Lock {
            filepath: PathBuf::from("otzaria_book_order.lock"),
            is_blocking: true,
        })?;
        let mut mapping = OrderMapping::load(&self.index_path);
        if mapping.places != places {
            mapping.revision = mapping
                .revision
                .checked_add(1)
                .context("book order revision exhausted")?;
            mapping.places = places;
            mapping.save(&self.index_path)?;
        }
        let revision = mapping.revision;
        if state.revision != revision || *state.places != mapping.places {
            *state = BookOrderState::of(mapping, order_file_stamp(&self.index_path));
            drop(state);
            self.page_cache.clear();
        }
        Ok(revision)
    }

    /// The revision of the order mapping: 0 before any [`Self::set_book_order`].
    /// Asynchronous because refreshing can read the mapping file or wait for
    /// an engine or book-order write lock.
    pub fn book_order_revision(&self) -> u64 {
        self.refresh_book_order();
        self.book_order
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .revision
    }

    /// Prepares the order catalogue and generation searches sort by — reading the book
    /// numbers of the index and building the rank tables — so the first such search does
    /// not pay for it. For after the window is shown; searches do it themselves otherwise.
    pub fn warm_book_order(&self) -> Result<()> {
        self.order_tables().map(|_| ())
    }

    /// Takes in the mapping file when another engine replaced it.
    fn refresh_book_order(&self) {
        let stamp = order_file_stamp(&self.index_path);
        let current = self
            .book_order
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .stamp;
        if stamp == current {
            return;
        }
        let mut state = self
            .book_order
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        // Read under the local write guard: a delayed refresh must not install an older
        // mapping after this engine's set_book_order has successfully saved a newer one.
        let read = BookOrderState::read(&self.index_path);
        if read.revision == state.revision && read.places == state.places {
            state.stamp = read.stamp;
            return;
        }
        *state = read;
        drop(state);
        self.page_cache.clear();
    }

    /// The number of `file_path`, handed out on the book's first write. Only while this
    /// engine holds the writer, which no other engine can then hold.
    pub(crate) fn book_number(&self, file_path: &str) -> Result<u32> {
        anyhow::ensure!(
            self.index_writer.is_some(),
            "a book number is handed out only while the index writer is held"
        );
        let mut numbers = self
            .book_numbers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let numbers = numbers.get_or_insert_with(|| BookNumbers::new(&self.index_path));
        if self.numbers_stale.load(Ordering::Acquire) {
            self.index_reader.reload()?;
            numbers.catch_up(&self.index_reader.searcher())?;
            // Keep the takeover pending if either read fails: retry before using an id.
            self.numbers_stale.store(false, Ordering::Release);
        }
        numbers.number_for(file_path)
    }

    /// The high half of every id of the book `file_path`.
    fn id_base(&self, file_path: &str) -> Result<u64> {
        Ok(u64::from(self.book_number(file_path)?) << 32)
    }

    /// The epoch of the current order mapping.
    fn order_epoch(&self) -> u64 {
        self.book_order
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .epoch
    }

    /// The rank tables catalogue and generation searches sort by, with the epoch of the
    /// mapping they were built from. The segments not read yet are read, and the tables
    /// built, without holding the locks writers take.
    pub(crate) fn order_tables(&self) -> Result<(u64, Arc<OrderTables>)> {
        self.refresh_book_order();
        let (epoch, places) = {
            let state = self
                .book_order
                .read()
                .unwrap_or_else(PoisonError::into_inner);
            (state.epoch, Arc::clone(&state.places))
        };
        let searcher = self.index_reader.searcher();
        let lock_numbers = || {
            self.book_numbers
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
        };
        let unread = lock_numbers()
            .get_or_insert_with(|| BookNumbers::new(&self.index_path))
            .unread_segments(&searcher);
        if !unread.is_empty() {
            let found = numbers_in_segments(&searcher, &unread)?;
            lock_numbers()
                .get_or_insert_with(|| BookNumbers::new(&self.index_path))
                .absorb(&searcher, &unread, found);
        }
        let (version, paths) = {
            let numbers = lock_numbers();
            let numbers = numbers.as_ref().expect("created above");
            let version = numbers.version();
            let cached = self
                .order_tables
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = cached
                .as_ref()
                .filter(|c| c.epoch == epoch && c.numbers_version == version)
            {
                return Ok((epoch, Arc::clone(&cached.tables)));
            }
            (version, numbers.paths().clone())
        };
        let tables = Arc::new(OrderTables::build(&paths, &places));
        *self
            .order_tables
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(CachedOrderTables {
            epoch,
            numbers_version: version,
            tables: Arc::clone(&tables),
        });
        Ok((epoch, tables))
    }

    /// מזהה אטום של התוכן, המיון, המילונים ומקורות הטקסט לחיפוש הנוכחי.
    pub fn search_snapshot(&self) -> String {
        self.refresh_book_order();
        format!(
            "{:016x}:{:016x}:{:016x}:{:?}",
            page_cache::snapshot_of(&self.index_reader.searcher()),
            self.order_epoch(),
            self.dictionary_epoch,
            crate::line_source::search_snapshot_generation()
        )
    }

    /// משחרר מטמוני חיפוש וטקסט שאפשר לבנות מחדש בלחץ זיכרון.
    pub fn release_memory(&self) {
        self.memory_epoch.fetch_add(1, Ordering::AcqRel);
        self.page_cache.clear();
        self.grouped_cache.clear();
        self.highlight_cache.clear();
        self.term_cache.lock().unwrap().clear();
        self.term_work.clear_idle();
        if let Some(dictionary) = &self.magic_dict {
            dictionary.release_memory();
        }
        crate::user_regex::release_memory();
        crate::line_source::release_memory();
        #[cfg(feature = "semantic-integration")]
        {
            self.semantic_sessions.prune_expired();
            self.semantic_highlights.clear();
            *self
                .semantic_resolver
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Default::default();
        }
    }

    // ── Search API ─────────────────────────────────────────────────────────────

    /// Paged regex search. Drops the single-word truncation flag: a broad
    /// query (e.g. `.*ספר`) that overflows its collection budget serves
    /// partial results with no signal. Not suitable for UI that must tell the
    /// user the result is partial — use [`Self::search_and_count`]
    /// ([`SearchPageResult::truncated`]) instead.
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
        cancel_request_id: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let (query, _) = self.build_query(regex_terms, facets, slop, max_expansions)?;
            let hl = highlight.unwrap_or_else(HighlightConfig::default);
            self.run_search(
                query,
                |_| Ok(HighlightPlan::none()),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &hl,
                None,
            )
        })
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
        cancel_request_id: Option<u32>,
    ) -> Result<SearchPageResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let (query, truncated) = self.build_query(regex_terms, facets, slop, max_expansions)?;
            let hl = highlight.unwrap_or_else(HighlightConfig::default);
            self.run_search_and_count(
                query,
                |_| Ok(HighlightPlan::none()),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &hl,
                truncated,
                None,
            )
        })
    }

    /// Bare hit count. Drops the single-word truncation flag: a broad query
    /// (e.g. `.*ספר`) that overflows its collection budget returns a partial
    /// count with no signal. Not suitable for UI that must tell the user the
    /// result is partial — use [`Self::count_with_status`], the combined
    /// stream, or [`SearchPageResult::truncated`] there instead.
    pub fn count(
        &self,
        regex_terms: Vec<String>,
        facets: &[String],
        slop: u32,
        max_expansions: u32,
    ) -> Result<u32> {
        Ok(self
            .count_with_status(regex_terms, facets, slop, max_expansions)?
            .count)
    }

    /// Like [`Self::count`] but also reports whether single-word collection
    /// truncated, so a UI consumer can flag a partial count.
    pub fn count_with_status(
        &self,
        regex_terms: Vec<String>,
        facets: &[String],
        slop: u32,
        max_expansions: u32,
    ) -> Result<CountResult> {
        let (query, truncated) =
            self.build_query(regex_terms, facets.to_vec(), slop, max_expansions)?;
        Ok(CountResult {
            count: self.run_count(query)?,
            truncated,
        })
    }

    /// Per-book hit counts. Drops the truncation flag — see [`Self::count`];
    /// use [`Self::count_by_book_with_status`] when partiality must surface.
    pub fn count_by_book(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<HashMap<String, u32>> {
        Ok(self
            .count_by_book_with_status(regex_terms, facets, slop, max_expansions)?
            .counts)
    }

    /// Like [`Self::count_by_book`] but also reports single-word truncation.
    pub fn count_by_book_with_status(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<BookCountResult> {
        let (query, truncated) = self.build_query(regex_terms, facets, slop, max_expansions)?;
        Ok(BookCountResult {
            counts: self.run_count_by_book(query)?,
            truncated,
        })
    }

    /// Return per-child facet counts for a given prefix (e.g. "/"). Drops the
    /// truncation flag — see [`Self::count`]; use
    /// [`Self::get_facet_counts_with_status`] when partiality must surface.
    ///
    /// שימו לב: ממדי הסינון חיים באותו שדה facet כמו עץ הקטגוריות, ולכן
    /// תחת prefix `/` מופיעים גם השורשים השמורים [`FACET_DIMENSION_ROOTS`]
    /// (`/author`, `/era`, `/base`) לצד קטגוריות-העל. לקוח שמונה ילדים
    /// כדי לבנות עץ קטגוריות חייב לסנן אותם (באפליקציה:
    /// `FacetHelper.isDimensionFacet`); לקוח שקורא ספירות לפי נתיבים
    /// ידועים מראש אינו מושפע.
    pub fn get_facet_counts(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        facet_prefix: String,
        slop: u32,
        max_expansions: u32,
    ) -> Result<Vec<FacetCount>> {
        Ok(self
            .get_facet_counts_with_status(regex_terms, facets, facet_prefix, slop, max_expansions)?
            .counts)
    }

    /// Like [`Self::get_facet_counts`] but also reports single-word truncation.
    pub fn get_facet_counts_with_status(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        facet_prefix: String,
        slop: u32,
        max_expansions: u32,
    ) -> Result<FacetCountsResult> {
        let (query, truncated) = self.build_query(regex_terms, facets, slop, max_expansions)?;
        Ok(FacetCountsResult {
            counts: self.run_facet_counts(query, facet_prefix)?,
            truncated,
        })
    }

    // ── Operational API ────────────────────────────────────────────────────────

    /// Compact the index without collapsing it. Pending changes are committed
    /// first (only committed segments take part in manual merges); then the
    /// searchable segments are brought toward `MAX_SEGMENTS_AFTER_OPTIMIZE`
    /// by merging similarly sized small segments. The target may be exceeded
    /// when its only alternative is rewriting a much larger healthy segment.
    /// Segments with a high deleted-doc share are compacted independently.
    pub fn optimize(&mut self) -> Result<()> {
        self.ensure_write_access()?;
        let started = Instant::now();
        let before_count = self.index.searchable_segment_ids()?.len();
        debug!("optimize: before={before_count}");
        // If another process owns the writer, this empty reader has no pending
        // documents of its own to flush. Preserve the harmless no-op behavior.
        if before_count == 0 && self.index_writer.is_none() {
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
        self.collect_garbage_after_optimize();
        let after_count = self.index.searchable_segment_ids()?.len();
        info!(
            "optimize: {before_count} → {after_count} segments in {:?}",
            started.elapsed()
        );
        Ok(())
    }

    /// Tantivy's post-merge GC keeps merged-away segments while the old searcher
    /// holds their metas/mmaps; run it again after `reload` released them.
    fn collect_garbage_after_optimize(&mut self) {
        let gc = match self.writer_mut() {
            Ok(writer) => writer.garbage_collect_files().wait(),
            Err(err) => {
                warn!("optimize: writer unavailable for garbage collection: {err:#}");
                return;
            }
        };
        match gc {
            Ok(result) => debug!(
                "optimize: garbage collection deleted {} files ({} could not be deleted)",
                result.deleted_files.len(),
                result.failed_to_delete_files.len()
            ),
            Err(err) => warn!("optimize: garbage collection failed: {err}"),
        }
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

    /// Content fingerprint per distinct `filePath`, read columnar from the
    /// live documents (like [`Self::count_documents_by_file_path`], no stored
    /// fields are touched).
    ///
    /// A value of 0 means "unverifiable": either the book was indexed without
    /// a fingerprint (e.g. PDF), or its live documents disagree (partial
    /// reindex) — callers should treat such books as changed or skip them.
    pub fn get_book_fingerprints(&self) -> Result<HashMap<String, u64>> {
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(
            &AllQuery,
            &BookFingerprintCollector {
                hash_column: "contentHash",
            },
        )?)
    }

    /// Text-only fingerprint per distinct `filePath` — the `textHash` column
    /// ([`compute_content_fingerprint`] over the raw book text). Unlike
    /// [`Self::get_book_fingerprints`] (the canonical fingerprint, which also
    /// covers the title, topics and extra facets), this value shifts only when
    /// the book's text itself changed — the right signal for content-drift
    /// checks that must not be invalidated by adding/removing other books.
    ///
    /// A value of 0 means "unverifiable", same semantics as
    /// [`Self::get_book_fingerprints`].
    pub fn get_book_text_fingerprints(&self) -> Result<HashMap<String, u64>> {
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(
            &AllQuery,
            &BookFingerprintCollector {
                hash_column: "textHash",
            },
        )?)
    }

    /// Text-only fingerprint of **one** book, by its `filePath` key — the
    /// single-book form of [`Self::get_book_text_fingerprints`]. A drift check
    /// for one open book must not pay O(total documents): this runs a
    /// `TermQuery` on `filePath` and reads `textHash` columnar from the
    /// matching live documents only.
    ///
    /// `0` means "unverifiable", exactly as in the map form: the book has no
    /// document in the index, was indexed without a text fingerprint (PDF), or
    /// its live documents disagree (a partial reindex).
    pub fn get_book_text_fingerprint(&self, file_path: String) -> Result<u64> {
        let file_path_f = self.schema.get_field("filePath")?;
        let query = TermQuery::new(
            Term::from_field_text(file_path_f, &file_path),
            IndexRecordOption::Basic,
        );
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&query, &SingleBookFingerprintCollector)?)
    }

    /// Every book of the index with its number, live documents and stamps, read columnar in
    /// one pass, by `file_path`. What a check after a library update compares against: the
    /// metadata and source stamps need no book text.
    pub fn get_book_stamps(&self) -> Result<Vec<BookStamps>> {
        let searcher = self.index_reader.searcher();
        let collector = book_order::BookStampsCollector {
            columns: [
                "contentHash",
                "textHash",
                METADATA_HASH_FIELD,
                SOURCE_HASH_FIELD,
            ],
        };
        let mut books: Vec<BookStamps> = searcher
            .search(&AllQuery, &collector)?
            .into_iter()
            .map(|(file_path, book)| {
                let [content_hash, text_hash, metadata_hash, source_hash] = book.stamps;
                BookStamps {
                    file_path,
                    book_number: book.number,
                    documents: book.documents,
                    content_hash,
                    text_hash,
                    metadata_hash,
                    source_hash,
                }
            })
            .collect();
        books.sort_by(|a, b| a.file_path.cmp(&b.file_path));
        Ok(books)
    }

    /// Fetch a single document by its numeric id. Returns None if not found.
    /// The `text` field contains the raw text (no snippet/highlight) — for a
    /// [`TextStorage::LibraryDb`] document, the library row; see `text_status`.
    pub fn get_document_by_id(&self, id: u64) -> Result<Option<SearchResult>> {
        let query = IdSetQuery::new([id]);
        let searcher = self.index_reader.searcher();

        let top_docs = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, addr)) = top_docs.into_iter().next() else {
            return Ok(None);
        };
        self.document_at(&searcher, addr, id, true).map(Some)
    }

    /// Where the line `id` of the book `file_path` is in `searcher`: found by the book and
    /// the id together, because two books' lines can share an id — an index updated book by
    /// book can give two books one catalogue position. `None` when the book holds no such
    /// line.
    #[cfg(feature = "semantic-integration")]
    fn address_in_book(
        &self,
        searcher: &Searcher,
        file_path: &str,
        id: u64,
    ) -> Result<Option<DocAddress>> {
        let book = TermQuery::new(
            Term::from_field_text(self.schema.get_field("filePath")?, file_path),
            IndexRecordOption::Basic,
        );
        let line = IdSetQuery::new([id]);
        let query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(book) as Box<dyn Query>),
            (Occur::Must, Box::new(line) as Box<dyn Query>),
        ]);
        let top_docs = searcher.search(&query, &TopDocs::with_limit(1).order_by_score())?;
        Ok(top_docs.into_iter().next().map(|(_, address)| address))
    }

    /// The stored fields of the document at `address` in `searcher`, as a result: what a
    /// result is hydrated from. `id` is the document's, as the caller found it.
    /// `with_text: false` skips the text (`text` empty, `text_status` Ok) for callers that
    /// need only the location, or read the texts of many results at once ([`Self::texts_at`]).
    fn document_at(
        &self,
        searcher: &Searcher,
        address: DocAddress,
        id: u64,
        with_text: bool,
    ) -> Result<SearchResult> {
        let doc = load_document(searcher, address)?;
        let title_f = self.schema.get_field("title")?;
        let reference_f = self.schema.get_field("reference")?;
        let segment_f = self.schema.get_field("segment")?;
        let is_pdf_f = self.schema.get_field("isPdf")?;
        let file_path_f = self.schema.get_field("filePath")?;
        let str_field = |field: Field| {
            doc.get_first(field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let mut result = SearchResult {
            title: str_field(title_f),
            reference: str_field(reference_f),
            text: String::new(),
            id,
            segment: doc
                .get_first(segment_f)
                .and_then(|v| v.as_u64())
                .unwrap_or_default(),
            is_pdf: doc
                .get_first(is_pdf_f)
                .and_then(|v| v.as_bool())
                .unwrap_or_default(),
            file_path: str_field(file_path_f),
            merged_count: 1,
            merged: Vec::new(),
            text_status: TextStatus::Ok,
            continues_to_next_line: false,
        };
        if with_text {
            let documents = [(address, doc)];
            if let Some(hit) =
                Self::resolve_hit_texts(&self.schema, searcher, &documents, HitRendering::Plain)?
                    .pop()
            {
                result.text = hit.plain;
                result.text_status = hit.status;
            }
        }
        Ok(result)
    }

    /// The raw text of the documents at `addresses`, as [`Self::document_at`] reads it, with
    /// every library row read in one transaction.
    #[cfg(feature = "semantic-integration")]
    fn texts_at(
        &self,
        searcher: &Searcher,
        addresses: &[DocAddress],
    ) -> Result<Vec<(DisplayText, TextStatus)>> {
        let mut loader = DocumentLoader::new(searcher);
        let documents = addresses
            .iter()
            .map(|&address| Ok((address, loader.load(address)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(
            Self::resolve_hit_texts(&self.schema, searcher, &documents, HitRendering::Plain)?
                .into_iter()
                .map(|hit| {
                    let status = hit.status;
                    (hit.display_notes(false), status)
                })
                .collect(),
        )
    }

    /// The embedded text to show, spaces collapsed: a short line's capped context, or a long
    /// line's matched window past its first (see `long_line_window_text`); `None` otherwise.
    #[cfg(feature = "semantic-integration")]
    fn embedded_window_texts(
        &self,
        chunking: &otzaria_semantic_search::semantic::chunker::ChunkerConfig,
        searcher: &Searcher,
        lines: &[EmbeddedLine<'_>],
    ) -> Result<Vec<Option<DisplayText>>> {
        use otzaria_semantic_search::semantic::oxv::occurrence::ALIAS_WINDOW;
        let chunking = chunking.clone();
        let chunker = otzaria_semantic_search::semantic::chunker::Chunker::new(chunking.clone())?;
        let short: Vec<usize> = (0..lines.len())
            .filter(|&at| {
                let length = lines[at].3.trim().chars().count();
                lines[at].4 != ALIAS_WINDOW
                    && (chunking.min_embeddable_chars..chunking.min_meaningful_chars)
                        .contains(&length)
            })
            .collect();
        let mut out: Vec<Option<DisplayText>> = lines
            .iter()
            .map(|&(_, _, _, text, window)| long_line_window_text(text, window, &chunker))
            .collect();
        if short.is_empty() {
            return Ok(out);
        }
        let resolver = crate::semantic_resolver::LiveResolver::for_chunking(
            searcher.clone(),
            self.chunk_key_field,
            &self.semantic_resolver,
            chunking.clone(),
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        let asked: Vec<(&str, u64, DocAddress)> = short
            .iter()
            .map(|&at| (lines[at].1, lines[at].2, lines[at].0))
            .collect();
        let windows = resolver
            .short_line_windows(&asked)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let mut known: HashMap<DocAddress, (DisplayText, TextStatus)> = short
            .iter()
            .map(|&at| (lines[at].0, (lines[at].3.clone(), TextStatus::Ok)))
            .collect();
        let mut unread: Vec<DocAddress> = windows
            .iter()
            .flatten()
            .flatten()
            .filter(|address| !known.contains_key(address))
            .copied()
            .collect();
        unread.sort_unstable();
        unread.dedup();
        let texts = self.texts_at(searcher, &unread)?;
        known.extend(unread.into_iter().zip(texts));
        for (&at, window) in short.iter().zip(windows) {
            let Some(window) = window else { continue };
            let texts: Option<Vec<&DisplayText>> = window
                .iter()
                .map(|address| match &known[address] {
                    (text, TextStatus::Ok) => Some(text),
                    _ => None,
                })
                .collect();
            out[at] = texts.map(|texts| {
                DisplayText::join(&texts, " ")
                    .trimmed()
                    .take_chars(chunking.max_chunk_chars)
                    .collapse_whitespace()
            });
        }
        Ok(out)
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
        let query = self.build_fuzzy_search_query(&terms, &[], &facets, max_distance, rank)?;
        let hl = highlight.unwrap_or_else(HighlightConfig::default);
        self.run_search(
            query,
            |s| self.fuzzy_highlight_plan(s, &terms, &[], max_distance),
            self.schema.get_field("text")?,
            limit,
            offset,
            &order,
            &hl,
            None,
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
    ///
    /// Drops the single-word truncation flag — see [`Self::search`]; use
    /// [`Self::search_and_count`] ([`SearchPageResult::truncated`]) when
    /// partiality must surface.
    #[cfg(feature = "flutter-bridge")]
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
        let result = (|| {
            let (query, _) = self.build_query(regex_terms, facets, slop, max_expansions)?;
            let hl = highlight.unwrap_or_else(HighlightConfig::default);
            self.run_search_stream(
                query,
                |_| Ok(HighlightPlan::none()),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &hl,
                chunk_size,
                None,
                &self.lexical_cancellations.begin(None),
                &sink,
            )
        })();
        Self::surface_stream_error(&sink, result)
    }

    // ── High-level mode-specific search API ──────────────────────────────────────
    //
    // These are the methods the otzaria app calls through its SearchEngineGateway.
    // Each builds the query for its mode (exact = Term/PhraseQuery, advanced =
    // morphological regex, fuzzy = FuzzyTermQuery) then routes through the shared
    // `run_*` executors. Snippet-returning methods apply the default `<font>`
    // highlight, which the app's snippet parser expects.

    // -- Exact -------------------------------------------------------------------

    /// Smart lexical matching without a model or a ranked semantic candidate window.
    /// Pagination and ordering are applied after all literal quotation constraints.
    #[allow(clippy::too_many_arguments)]
    pub fn search_smart_lexical(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        lexical_mode: SemanticLexicalMode,
        fuzzy_max_distance: u8,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            request.check()?;
            if limit == 0 {
                return Ok(Vec::new());
            }
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let exact = matches!(lexical_mode, SemanticLexicalMode::Exact)
                || self.whole_query_quoted(&query)?;
            let (search_query, _) = if exact {
                self.build_smart_exact_query(&query, &facets, &voc, true)?
            } else {
                self.build_smart_fuzzy_query(
                    &query,
                    &facets,
                    fuzzy_max_distance,
                    &voc,
                    matches!(order, ResultsOrder::Relevance),
                )?
            };
            self.run_search(
                search_query,
                |searcher| {
                    if exact {
                        self.smart_exact_highlight_plan(searcher, &query, &voc)
                    } else {
                        self.smart_fuzzy_highlight_plan(searcher, &query, fuzzy_max_distance, &voc)
                    }
                },
                self.search_text_field(&voc)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                None,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search_exact(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let (q, _) = self.build_exact_query(&query, &facets, &voc)?;
            self.run_search(
                q,
                |s| self.exact_highlight_plan(s, &query, &voc),
                self.search_text_field(&voc)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                grouping.as_ref(),
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search_and_count_exact(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
    ) -> Result<SearchPageResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let (q, truncated) = self.build_exact_query(&query, &facets, &voc)?;
            self.run_search_and_count(
                q,
                |s| self.exact_highlight_plan(s, &query, &voc),
                self.search_text_field(&voc)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                truncated,
                grouping.as_ref(),
            )
        })
    }

    fn search_and_count_smart_exact(
        &self,
        query: &str,
        facets: &[String],
        limit: u32,
        offset: u32,
        match_nikud: bool,
        match_taamim: bool,
        grouping: Option<ResultGrouping>,
    ) -> Result<SearchPageResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, truncated) = self.build_smart_exact_query(query, facets, &voc, true)?;
        self.run_search_and_count(
            q,
            |s| self.smart_exact_highlight_plan(s, query, &voc),
            self.search_text_field(&voc)?,
            limit,
            offset,
            &ResultsOrder::Relevance,
            &HighlightConfig::default(),
            truncated,
            grouping.as_ref(),
        )
    }

    fn count_smart_exact(
        &self,
        query: &str,
        facets: &[String],
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<CountResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (query, truncated) = self.build_smart_exact_query(query, facets, &voc, true)?;
        Ok(CountResult {
            count: self.run_count(query)?,
            truncated,
        })
    }

    /// Collect scored, ungrouped Tantivy hits for the semantic coordinator.
    /// This bypasses snippet construction: the coordinator needs original line
    /// text and actual BM25 scores, while painting happens after fusion and
    /// pagination — see [`SemanticLexicalPhase::highlight`].
    #[cfg(feature = "semantic-integration")]
    fn semantic_exact_lexical_candidates(
        &self,
        searcher: &Searcher,
        query: &str,
        facets: &[String],
        limit: u32,
        match_nikud: bool,
        match_taamim: bool,
        with_highlight: bool,
    ) -> Result<SemanticLexicalPhase> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (search_query, truncated) = self.build_smart_exact_query(query, facets, &voc, false)?;
        if !with_highlight {
            return self.semantic_candidates_from_query(
                searcher,
                search_query,
                limit,
                truncated,
                None,
            );
        }

        // Retrieval honours the vocalization flags, but painting is always
        // mark-free over the stored `text` field: that is the copy the sidecar
        // indexes and hydration reads back, so a vocalized highlight query would
        // expose no terms for it and leave every line unpainted. Facets are
        // dropped here — they filter documents, never highlights.
        let plain = VocalizedFlags::new(false, false);
        let (display_query, _) = self.build_smart_exact_query(query, &[], &plain, false)?;
        let HighlightPlan {
            query: plan_query,
            phrase,
        } = Self::resolve_highlight(searcher, |s| {
            self.smart_exact_highlight_plan(s, query, &plain)
        });
        let highlight = SemanticHighlight {
            query: plan_query.unwrap_or(display_query),
            phrase,
        };
        self.semantic_candidates_from_query(
            searcher,
            search_query,
            limit,
            truncated,
            Some(highlight),
        )
    }

    #[cfg(feature = "semantic-integration")]
    fn semantic_fuzzy_lexical_candidates(
        &self,
        searcher: &Searcher,
        query: &str,
        facets: &[String],
        limit: u32,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
        with_highlight: bool,
    ) -> Result<SemanticLexicalPhase> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (search_query, truncated) =
            self.build_smart_fuzzy_query(query, facets, max_distance, &voc, true)?;

        if !with_highlight {
            return self.semantic_candidates_from_query(
                searcher,
                search_query,
                limit,
                truncated,
                None,
            );
        }
        // Mark-free for the same reason as the exact path. A fuzzy automaton
        // exposes no static terms, so without the materialized highlight query
        // there is nothing to paint with and the page falls back to bounded
        // plain snippets.
        let HighlightPlan {
            query: plan_query,
            phrase,
        } = Self::resolve_highlight(searcher, |s| {
            self.smart_fuzzy_highlight_plan(
                s,
                query,
                max_distance,
                &VocalizedFlags::new(false, false),
            )
        });
        let highlight = plan_query.map(|query| SemanticHighlight { query, phrase });
        self.semantic_candidates_from_query(searcher, search_query, limit, truncated, highlight)
    }

    /// Build the painter for one page of sidecar results. Separate from
    /// candidate collection on purpose: creating the generator resolves the
    /// doc-frequency of every highlight term, so it happens once, after
    /// pagination, and only when there is a page to paint.
    #[cfg(feature = "semantic-integration")]
    fn semantic_snippet_painter(
        &self,
        searcher: &Searcher,
        highlight: SemanticHighlight,
    ) -> Result<SemanticSnippetPainter> {
        let hl = HighlightConfig::default();
        let generator = Self::make_snippet_generator(
            searcher,
            highlight.query.as_ref(),
            self.schema.get_field("text")?,
            &hl,
        )?;
        Ok(SemanticSnippetPainter {
            searcher: searcher.clone(),
            generator,
            phrase: highlight.phrase,
            hl,
        })
    }

    #[cfg(feature = "semantic-integration")]
    fn semantic_candidates_from_query(
        &self,
        searcher: &Searcher,
        query: Box<dyn Query>,
        limit: u32,
        truncated: bool,
        highlight: Option<SemanticHighlight>,
    ) -> Result<SemanticLexicalPhase> {
        let collector = TopDocs::with_limit(limit as usize).order_by_score();
        let (hits, total_count): (Vec<(Score, DocAddress)>, usize) =
            searcher.search(&*query, &(collector, Count))?;

        let title_f = self.schema.get_field("title")?;
        let reference_f = self.schema.get_field("reference")?;
        let id_f = self.schema.get_field("id")?;
        let segment_f = self.schema.get_field("segment")?;
        let is_pdf_f = self.schema.get_field("isPdf")?;
        let file_path_f = self.schema.get_field("filePath")?;

        let mut loader = DocumentLoader::new(searcher);
        let documents = hits
            .iter()
            .map(|&(_, address)| Ok((address, loader.load(address)?)))
            .collect::<Result<Vec<_>>>()?;
        // The whole candidate window's library rows in one transaction.
        let texts =
            Self::resolve_hit_texts(&self.schema, searcher, &documents, HitRendering::Plain)?;

        let mut candidates = Vec::with_capacity(hits.len());
        let mut text_status = HashMap::new();
        let mut notes = HashMap::new();
        // A book's facets are the book's, as the resolver reads them: once per book.
        let mut facet_readers: HashMap<SegmentOrdinal, tantivy::fastfield::FacetReader> =
            HashMap::new();
        let mut book_facets: HashMap<String, Vec<String>> = HashMap::new();
        for (((score, _), (address, document)), text) in hits.iter().zip(&documents).zip(texts) {
            let reader = searcher.segment_reader(address.segment_ord);
            let fast = reader.fast_fields();
            let section_id = SectionIdColumn::open(reader)?
                .first(address.doc_id)
                .unwrap_or_default();
            let line_hash = fast
                .u64("lineHash")?
                .first(address.doc_id)
                .unwrap_or_default();
            let line_id = document
                .get_first(id_f)
                .and_then(|value| value.as_u64())
                .unwrap_or_default();
            if text.status != TextStatus::Ok {
                text_status.insert(line_id, text.status);
            }
            let file_path = document
                .get_first(file_path_f)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            let facets = match book_facets.get(&file_path) {
                Some(facets) => facets.clone(),
                None => {
                    let facet_reader = match facet_readers.entry(address.segment_ord) {
                        std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                        std::collections::hash_map::Entry::Vacant(entry) => {
                            entry.insert(reader.facet_reader("topics")?)
                        }
                    };
                    let mut facet = Facet::default();
                    let mut facets = Vec::new();
                    for ord in facet_reader.facet_ords(address.doc_id) {
                        facet_reader.facet_from_ord(ord, &mut facet)?;
                        facets.push(facet.to_string());
                    }
                    facets.sort();
                    facets.dedup();
                    book_facets.insert(file_path.clone(), facets.clone());
                    facets
                }
            };
            if text.status == TextStatus::Ok && !text.notes.is_empty() {
                notes.insert((file_path.clone(), line_id), text.notes);
            }
            candidates.push(SidecarLexicalCandidate {
                title: document
                    .get_first(title_f)
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                reference: document
                    .get_first(reference_f)
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                text: text.plain,
                line_id,
                section_id,
                line_hash,
                segment: document
                    .get_first(segment_f)
                    .and_then(|value| value.as_u64())
                    .unwrap_or_default(),
                is_pdf: document
                    .get_first(is_pdf_f)
                    .and_then(|value| value.as_bool())
                    .unwrap_or_default(),
                file_path,
                bm25_score: *score,
                facets,
            });
        }
        Ok(SemanticLexicalPhase {
            candidates,
            total_count: total_count as u32,
            truncated,
            highlight,
            text_status,
            notes,
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "flutter-bridge")]
    pub fn search_exact_stream(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        chunk_size: u32,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let result = request.scope(|| {
            request.check()?;
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let (q, _) = self.build_exact_query(&query, &facets, &voc)?;
            self.run_search_stream(
                q,
                |s| self.exact_highlight_plan(s, &query, &voc),
                self.search_text_field(&voc)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                grouping.as_ref(),
                &request,
                &sink,
            )
        });
        Self::surface_stream_error(&sink, request.finish(result))
    }

    /// Like [`Self::search_exact_stream`] but the first event also carries
    /// the total hit count and per-book counts from the same index pass —
    /// replacing the separate `count_exact` + `count_by_book_exact` calls a
    /// search screen would otherwise issue for the same query.
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "flutter-bridge")]
    pub fn search_exact_stream_with_counts(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        chunk_size: u32,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
        sink: StreamSink<SearchStreamUpdate>,
    ) -> Result<()> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let result = request.scope(|| {
            request.check()?;
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            // The mark-free exact path never degrades under a collection
            // budget; the vocalized single-word path can (its term set is
            // materialized like an advanced word).
            let (q, truncated) = self.build_exact_query(&query, &facets, &voc)?;
            self.run_search_stream_with_counts(
                q,
                |s| self.exact_highlight_plan(s, &query, &voc),
                self.search_text_field(&voc)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                truncated,
                grouping.as_ref(),
                &request,
                &mut |update| sink.add(update).is_ok(),
            )
        });
        Self::surface_stream_error(&sink, request.finish(result))
    }

    #[cfg(test)]
    pub(crate) fn exact_stream_into(
        &self,
        query: String,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        grouping: Option<ResultGrouping>,
        chunk_size: u32,
        cancel_request_id: Option<u32>,
        emit: &mut dyn FnMut(SearchStreamUpdate) -> bool,
    ) -> Result<()> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let voc = VocalizedFlags::new(false, false);
            let (query_built, truncated) = self.build_exact_query(&query, &[], &voc)?;
            self.run_search_stream_with_counts(
                query_built,
                |s| self.exact_highlight_plan(s, &query, &voc),
                self.search_text_field(&voc)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                truncated,
                grouping.as_ref(),
                &request,
                emit,
            )
        })
    }

    /// Exact-mode hit count. Drops the truncation flag: the mark-free path
    /// never truncates, but the vocalized single-word path can (its term set
    /// is materialized like an advanced word) — use
    /// [`Self::count_exact_with_status`] when partiality must surface.
    pub fn count_exact(
        &self,
        query: String,
        facets: Vec<String>,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<u32> {
        Ok(self
            .count_exact_with_status(query, facets, match_nikud, match_taamim, cancel_request_id)?
            .count)
    }

    /// Like [`Self::count_exact`] but also reports whether the vocalized
    /// single-word term collection truncated.
    pub fn count_exact_with_status(
        &self,
        query: String,
        facets: Vec<String>,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<CountResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let _scope = request.enter();
        request.check()?;
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, truncated) = self.build_exact_query(&query, &facets, &voc)?;
        Ok(CountResult {
            count: request.finish(self.run_count(request.wrap(q)))?,
            truncated,
        })
    }

    /// Exact-mode per-book counts. Drops the truncation flag — see
    /// [`Self::count_exact`]; use [`Self::count_by_book_exact_with_status`].
    pub fn count_by_book_exact(
        &self,
        query: String,
        facets: Vec<String>,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<HashMap<String, u32>> {
        Ok(self
            .count_by_book_exact_with_status(
                query,
                facets,
                match_nikud,
                match_taamim,
                cancel_request_id,
            )?
            .counts)
    }

    /// Like [`Self::count_by_book_exact`] but also reports whether the
    /// vocalized single-word term collection truncated.
    pub fn count_by_book_exact_with_status(
        &self,
        query: String,
        facets: Vec<String>,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<BookCountResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let _scope = request.enter();
        request.check()?;
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, truncated) = self.build_exact_query(&query, &facets, &voc)?;
        Ok(BookCountResult {
            counts: request.finish(self.run_count_by_book(request.wrap(q)))?,
            truncated,
        })
    }

    /// Exact-mode facet counts. Drops the truncation flag — see
    /// [`Self::count_exact`]; use [`Self::get_facet_counts_exact_with_status`].
    /// על שורשי הממדים תחת prefix `/` ראו [`Self::get_facet_counts`].
    pub fn get_facet_counts_exact(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<Vec<FacetCount>> {
        Ok(self
            .get_facet_counts_exact_with_status(
                query,
                facets,
                facet_prefix,
                match_nikud,
                match_taamim,
            )?
            .counts)
    }

    /// Like [`Self::get_facet_counts_exact`] but also reports whether the
    /// vocalized single-word term collection truncated.
    pub fn get_facet_counts_exact_with_status(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<FacetCountsResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, truncated) = self.build_exact_query(&query, &facets, &voc)?;
        Ok(FacetCountsResult {
            counts: self.run_facet_counts(q, facet_prefix)?,
            truncated,
        })
    }

    // -- Advanced ----------------------------------------------------------------

    pub fn search_advanced(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        grouping: Option<ResultGrouping>,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let match_mode = WordMatch::from_api(word_match_mode, word_match_count);
            // מצב-שדה: הדגלים הגלובליים או אפשרות "ניקוד"/"טעמים" פר-מילה —
            // בחירת השדה וה-analyzer של ההדגשה, בעוד הדרישה פר-תו נגזרת פר-מילה
            // בתוך בניית השאילתה.
            let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
            let (q, regex_terms, proximity, truncated, acronym_alts) = self
                .build_advanced_query_ranked(
                    &query,
                    distance,
                    &custom_spacing,
                    any_order.unwrap_or(false),
                    &alternative_words,
                    &search_options,
                    facets,
                    &voc,
                    &scope,
                    &match_mode,
                    true,
                    matches!(order, ResultsOrder::Relevance),
                )?;
            let (q, _) = self.apply_advanced_negative_query(
                q,
                truncated,
                &negative_query,
                negative_distance,
                &negative_custom_spacing,
                &negative_alternative_words,
                &negative_search_options,
                &voc,
                &negative_scope,
                any_order.unwrap_or(false),
            )?;
            self.run_search(
                q,
                |s| {
                    self.advanced_highlight_plan_for_scope(
                        s,
                        &regex_terms,
                        &proximity,
                        &voc_mode,
                        &scope,
                        &match_mode,
                        &acronym_alts,
                    )
                },
                self.search_text_field(&voc_mode)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                grouping.as_ref(),
            )
        })
    }

    pub fn search_and_count_advanced(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        grouping: Option<ResultGrouping>,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
    ) -> Result<SearchPageResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let match_mode = WordMatch::from_api(word_match_mode, word_match_count);
            let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
            let (q, regex_terms, proximity, truncated, acronym_alts) = self
                .build_advanced_query_ranked(
                    &query,
                    distance,
                    &custom_spacing,
                    any_order.unwrap_or(false),
                    &alternative_words,
                    &search_options,
                    facets,
                    &voc,
                    &scope,
                    &match_mode,
                    true,
                    matches!(order, ResultsOrder::Relevance),
                )?;
            let (q, truncated) = self.apply_advanced_negative_query(
                q,
                truncated,
                &negative_query,
                negative_distance,
                &negative_custom_spacing,
                &negative_alternative_words,
                &negative_search_options,
                &voc,
                &negative_scope,
                any_order.unwrap_or(false),
            )?;
            self.run_search_and_count(
                q,
                |s| {
                    self.advanced_highlight_plan_for_scope(
                        s,
                        &regex_terms,
                        &proximity,
                        &voc_mode,
                        &scope,
                        &match_mode,
                        &acronym_alts,
                    )
                },
                self.search_text_field(&voc_mode)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                truncated,
                grouping.as_ref(),
            )
        })
    }

    /// Advanced-query result stream. Drops the single-word truncation flag —
    /// see [`Self::search`]; use [`Self::search_advanced_stream_with_counts`]
    /// (its first [`SearchStreamUpdate`] carries `truncated`) when the UI
    /// must flag partial results.
    #[cfg(feature = "flutter-bridge")]
    pub fn search_advanced_stream(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        chunk_size: u32,
        grouping: Option<ResultGrouping>,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let result = request.scope(|| {
            request.check()?;
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let match_mode = WordMatch::from_api(word_match_mode, word_match_count);
            let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
            let (q, regex_terms, proximity, truncated, acronym_alts) = self
                .build_advanced_query_ranked(
                    &query,
                    distance,
                    &custom_spacing,
                    any_order.unwrap_or(false),
                    &alternative_words,
                    &search_options,
                    facets,
                    &voc,
                    &scope,
                    &match_mode,
                    true,
                    matches!(order, ResultsOrder::Relevance),
                )?;
            let (q, _) = self.apply_advanced_negative_query(
                q,
                truncated,
                &negative_query,
                negative_distance,
                &negative_custom_spacing,
                &negative_alternative_words,
                &negative_search_options,
                &voc,
                &negative_scope,
                any_order.unwrap_or(false),
            )?;
            self.run_search_stream(
                q,
                |s| {
                    self.advanced_highlight_plan_for_scope(
                        s,
                        &regex_terms,
                        &proximity,
                        &voc_mode,
                        &scope,
                        &match_mode,
                        &acronym_alts,
                    )
                },
                self.search_text_field(&voc_mode)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                grouping.as_ref(),
                &request,
                &sink,
            )
        });
        Self::surface_stream_error(&sink, request.finish(result))
    }

    /// Like [`Self::search_advanced_stream`] but the first event also carries
    /// the total hit count and per-book counts from the same index pass —
    /// replacing the separate `count_advanced` + `count_by_book_advanced`
    /// calls a search screen would otherwise issue for the same query.
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "flutter-bridge")]
    pub fn search_advanced_stream_with_counts(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        chunk_size: u32,
        grouping: Option<ResultGrouping>,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
        sink: StreamSink<SearchStreamUpdate>,
    ) -> Result<()> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let result = request.scope(|| {
            request.check()?;
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let match_mode = WordMatch::from_api(word_match_mode, word_match_count);
            let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
            let (q, regex_terms, proximity, truncated, acronym_alts) = self
                .build_advanced_query_ranked(
                    &query,
                    distance,
                    &custom_spacing,
                    any_order.unwrap_or(false),
                    &alternative_words,
                    &search_options,
                    facets,
                    &voc,
                    &scope,
                    &match_mode,
                    true,
                    matches!(order, ResultsOrder::Relevance),
                )?;
            let (q, truncated) = self.apply_advanced_negative_query(
                q,
                truncated,
                &negative_query,
                negative_distance,
                &negative_custom_spacing,
                &negative_alternative_words,
                &negative_search_options,
                &voc,
                &negative_scope,
                any_order.unwrap_or(false),
            )?;
            self.run_search_stream_with_counts(
                q,
                |s| {
                    self.advanced_highlight_plan_for_scope(
                        s,
                        &regex_terms,
                        &proximity,
                        &voc_mode,
                        &scope,
                        &match_mode,
                        &acronym_alts,
                    )
                },
                self.search_text_field(&voc_mode)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                truncated,
                grouping.as_ref(),
                &request,
                &mut |update| sink.add(update).is_ok(),
            )
        });
        Self::surface_stream_error(&sink, request.finish(result))
    }

    /// Advanced-query hit count. Drops the single-word truncation flag — see
    /// [`Self::count`]; use [`Self::count_advanced_with_status`] for UI.
    #[allow(clippy::too_many_arguments)]
    pub fn count_advanced(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
    ) -> Result<u32> {
        Ok(self
            .count_advanced_with_status(
                query,
                negative_query,
                facets,
                distance,
                negative_distance,
                custom_spacing,
                negative_custom_spacing,
                alternative_words,
                negative_alternative_words,
                search_options,
                negative_search_options,
                match_nikud,
                match_taamim,
                scope,
                negative_scope,
                word_match_mode,
                word_match_count,
                any_order,
                cancel_request_id,
            )?
            .count)
    }

    /// Like [`Self::count_advanced`] but also reports single-word truncation.
    #[allow(clippy::too_many_arguments)]
    pub fn count_advanced_with_status(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
    ) -> Result<CountResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let _scope = request.enter();
        request.check()?;
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _, _, truncated, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            any_order.unwrap_or(false),
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
            &WordMatch::from_api(word_match_mode, word_match_count),
            true,
        )?;
        let (q, truncated) = self.apply_advanced_negative_query(
            q,
            truncated,
            &negative_query,
            negative_distance,
            &negative_custom_spacing,
            &negative_alternative_words,
            &negative_search_options,
            &voc,
            &negative_scope,
            any_order.unwrap_or(false),
        )?;
        Ok(CountResult {
            count: request.finish(self.run_count(request.wrap(q)))?,
            truncated,
        })
    }

    /// Advanced-query per-book counts. Drops the truncation flag — see
    /// [`Self::count`]; use [`Self::count_by_book_advanced_with_status`].
    #[allow(clippy::too_many_arguments)]
    pub fn count_by_book_advanced(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
    ) -> Result<HashMap<String, u32>> {
        Ok(self
            .count_by_book_advanced_with_status(
                query,
                negative_query,
                facets,
                distance,
                negative_distance,
                custom_spacing,
                negative_custom_spacing,
                alternative_words,
                negative_alternative_words,
                search_options,
                negative_search_options,
                match_nikud,
                match_taamim,
                scope,
                negative_scope,
                word_match_mode,
                word_match_count,
                any_order,
                cancel_request_id,
            )?
            .counts)
    }

    /// Like [`Self::count_by_book_advanced`] but also reports truncation.
    #[allow(clippy::too_many_arguments)]
    pub fn count_by_book_advanced_with_status(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
    ) -> Result<BookCountResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let _scope = request.enter();
        request.check()?;
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _, _, truncated, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            any_order.unwrap_or(false),
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
            &WordMatch::from_api(word_match_mode, word_match_count),
            true,
        )?;
        let (q, truncated) = self.apply_advanced_negative_query(
            q,
            truncated,
            &negative_query,
            negative_distance,
            &negative_custom_spacing,
            &negative_alternative_words,
            &negative_search_options,
            &voc,
            &negative_scope,
            any_order.unwrap_or(false),
        )?;
        Ok(BookCountResult {
            counts: request.finish(self.run_count_by_book(request.wrap(q)))?,
            truncated,
        })
    }

    /// Advanced-query facet counts. Drops the truncation flag — see
    /// [`Self::count`]; use [`Self::get_facet_counts_advanced_with_status`].
    /// על שורשי הממדים תחת prefix `/` ראו [`Self::get_facet_counts`].
    #[allow(clippy::too_many_arguments)]
    pub fn get_facet_counts_advanced(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        facet_prefix: String,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
    ) -> Result<Vec<FacetCount>> {
        Ok(self
            .get_facet_counts_advanced_with_status(
                query,
                negative_query,
                facets,
                facet_prefix,
                distance,
                negative_distance,
                custom_spacing,
                negative_custom_spacing,
                alternative_words,
                negative_alternative_words,
                search_options,
                negative_search_options,
                match_nikud,
                match_taamim,
                scope,
                negative_scope,
                word_match_mode,
                word_match_count,
                any_order,
            )?
            .counts)
    }

    /// Like [`Self::get_facet_counts_advanced`] but also reports truncation.
    #[allow(clippy::too_many_arguments)]
    pub fn get_facet_counts_advanced_with_status(
        &self,
        query: String,
        negative_query: String,
        facets: Vec<String>,
        facet_prefix: String,
        distance: u32,
        negative_distance: u32,
        custom_spacing: HashMap<String, String>,
        negative_custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        negative_alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        negative_search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
        negative_scope: SearchScope,
        word_match_mode: Option<WordMatchMode>,
        word_match_count: Option<u32>,
        any_order: Option<bool>,
    ) -> Result<FacetCountsResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _, _, truncated, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            any_order.unwrap_or(false),
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
            &WordMatch::from_api(word_match_mode, word_match_count),
            true,
        )?;
        let (q, truncated) = self.apply_advanced_negative_query(
            q,
            truncated,
            &negative_query,
            negative_distance,
            &negative_custom_spacing,
            &negative_alternative_words,
            &negative_search_options,
            &voc,
            &negative_scope,
            any_order.unwrap_or(false),
        )?;
        Ok(FacetCountsResult {
            counts: self.run_facet_counts(q, facet_prefix)?,
            truncated,
        })
    }

    // -- Fuzzy -------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn search_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
    ) -> Result<Vec<SearchResult>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            if voc.any() {
                // The vocalized query is a materialized TermSetQuery per token —
                // it exposes its terms to the snippet generator by itself.
                let (q, _) =
                    self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?;
                return self.run_search(
                    q,
                    |_| Ok(HighlightPlan::none()),
                    self.search_text_field(&voc)?,
                    limit,
                    offset,
                    &order,
                    &HighlightConfig::default(),
                    grouping.as_ref(),
                );
            }
            let token_texts = self.index_token_texts(&query)?;
            let phrases = self.quoted_phrase_token_groups(&query)?;
            let rank = matches!(order, ResultsOrder::Relevance);
            let q =
                self.build_fuzzy_search_query(&token_texts, &phrases, &facets, max_distance, rank)?;
            self.run_search(
                q,
                |s| self.fuzzy_highlight_plan(s, &token_texts, &phrases, max_distance),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                grouping.as_ref(),
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn search_and_count_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
    ) -> Result<SearchPageResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            if voc.any() {
                let (q, truncated) =
                    self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?;
                return self.run_search_and_count(
                    q,
                    |_| Ok(HighlightPlan::none()),
                    self.search_text_field(&voc)?,
                    limit,
                    offset,
                    &order,
                    &HighlightConfig::default(),
                    truncated,
                    grouping.as_ref(),
                );
            }
            let token_texts = self.index_token_texts(&query)?;
            let phrases = self.quoted_phrase_token_groups(&query)?;
            let rank = matches!(order, ResultsOrder::Relevance);
            let q =
                self.build_fuzzy_search_query(&token_texts, &phrases, &facets, max_distance, rank)?;
            self.run_search_and_count(
                q,
                |s| self.fuzzy_highlight_plan(s, &token_texts, &phrases, max_distance),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                false,
                grouping.as_ref(),
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "flutter-bridge")]
    pub fn search_fuzzy_stream(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        chunk_size: u32,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let result = request.scope(|| {
            request.check()?;
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            if voc.any() {
                let (q, _) =
                    self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?;
                return self.run_search_stream(
                    q,
                    |_| Ok(HighlightPlan::none()),
                    self.search_text_field(&voc)?,
                    limit,
                    offset,
                    &order,
                    &HighlightConfig::default(),
                    chunk_size,
                    grouping.as_ref(),
                    &request,
                    &sink,
                );
            }
            let token_texts = self.index_token_texts(&query)?;
            let phrases = self.quoted_phrase_token_groups(&query)?;
            let rank = matches!(order, ResultsOrder::Relevance);
            let q =
                self.build_fuzzy_search_query(&token_texts, &phrases, &facets, max_distance, rank)?;
            self.run_search_stream(
                q,
                |s| self.fuzzy_highlight_plan(s, &token_texts, &phrases, max_distance),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                grouping.as_ref(),
                &request,
                &sink,
            )
        });
        Self::surface_stream_error(&sink, request.finish(result))
    }

    /// Like [`Self::search_fuzzy_stream`] but the first event also carries
    /// the total hit count and per-book counts from the same index pass —
    /// replacing the separate `count_fuzzy` + `count_by_book_fuzzy` calls a
    /// search screen would otherwise issue for the same query.
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "flutter-bridge")]
    pub fn search_fuzzy_stream_with_counts(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        max_distance: u8,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        chunk_size: u32,
        grouping: Option<ResultGrouping>,
        cancel_request_id: Option<u32>,
        sink: StreamSink<SearchStreamUpdate>,
    ) -> Result<()> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let result = request.scope(|| {
            request.check()?;
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            if voc.any() {
                let (q, truncated) =
                    self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?;
                return self.run_search_stream_with_counts(
                    q,
                    |_| Ok(HighlightPlan::none()),
                    self.search_text_field(&voc)?,
                    limit,
                    offset,
                    &order,
                    &HighlightConfig::default(),
                    chunk_size,
                    truncated,
                    grouping.as_ref(),
                    &request,
                    &mut |update| sink.add(update).is_ok(),
                );
            }
            let token_texts = self.index_token_texts(&query)?;
            let phrases = self.quoted_phrase_token_groups(&query)?;
            let rank = matches!(order, ResultsOrder::Relevance);
            let q =
                self.build_fuzzy_search_query(&token_texts, &phrases, &facets, max_distance, rank)?;
            self.run_search_stream_with_counts(
                q,
                |s| self.fuzzy_highlight_plan(s, &token_texts, &phrases, max_distance),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                // The fuzzy path uses its own automaton budgets, not the
                // single-word degrade mechanism.
                false,
                grouping.as_ref(),
                &request,
                &mut |update| sink.add(update).is_ok(),
            )
        });
        Self::surface_stream_error(&sink, request.finish(result))
    }

    /// Builds the fuzzy-mode query for the count family: the vocalized path
    /// carries its truncation flag; the mark-free path uses its own automaton
    /// budgets, not the single-word degrade mechanism, so it never truncates.
    fn build_fuzzy_count_query(
        &self,
        query: &str,
        facets: &[String],
        max_distance: u8,
        voc: &VocalizedFlags,
    ) -> Result<(Box<dyn Query>, bool)> {
        if voc.any() {
            self.build_fuzzy_query_vocalized(query, facets, max_distance, voc)
        } else {
            Ok((self.build_fuzzy_query(query, facets, max_distance)?, false))
        }
    }

    /// Fuzzy-mode hit count. Drops the truncation flag: the mark-free path
    /// never truncates, but the vocalized path can (its term set is
    /// materialized like an advanced word) — use
    /// [`Self::count_fuzzy_with_status`] when partiality must surface.
    pub fn count_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<u32> {
        Ok(self
            .count_fuzzy_with_status(
                query,
                facets,
                max_distance,
                match_nikud,
                match_taamim,
                cancel_request_id,
            )?
            .count)
    }

    /// Like [`Self::count_fuzzy`] but also reports whether the vocalized
    /// term collection truncated.
    pub fn count_fuzzy_with_status(
        &self,
        query: String,
        facets: Vec<String>,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<CountResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let _scope = request.enter();
        request.check()?;
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, truncated) = self.build_fuzzy_count_query(&query, &facets, max_distance, &voc)?;
        Ok(CountResult {
            count: request.finish(self.run_count(request.wrap(q)))?,
            truncated,
        })
    }

    /// Fuzzy-mode per-book counts. Drops the truncation flag — see
    /// [`Self::count_fuzzy`]; use [`Self::count_by_book_fuzzy_with_status`].
    pub fn count_by_book_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<HashMap<String, u32>> {
        Ok(self
            .count_by_book_fuzzy_with_status(
                query,
                facets,
                max_distance,
                match_nikud,
                match_taamim,
                cancel_request_id,
            )?
            .counts)
    }

    /// Like [`Self::count_by_book_fuzzy`] but also reports whether the
    /// vocalized term collection truncated.
    pub fn count_by_book_fuzzy_with_status(
        &self,
        query: String,
        facets: Vec<String>,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
        cancel_request_id: Option<u32>,
    ) -> Result<BookCountResult> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        let _scope = request.enter();
        request.check()?;
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, truncated) = self.build_fuzzy_count_query(&query, &facets, max_distance, &voc)?;
        Ok(BookCountResult {
            counts: request.finish(self.run_count_by_book(request.wrap(q)))?,
            truncated,
        })
    }

    /// Fuzzy-mode facet counts. Drops the truncation flag — see
    /// [`Self::count_fuzzy`]; use [`Self::get_facet_counts_fuzzy_with_status`].
    /// על שורשי הממדים תחת prefix `/` ראו [`Self::get_facet_counts`].
    pub fn get_facet_counts_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<Vec<FacetCount>> {
        Ok(self
            .get_facet_counts_fuzzy_with_status(
                query,
                facets,
                facet_prefix,
                max_distance,
                match_nikud,
                match_taamim,
            )?
            .counts)
    }

    /// Like [`Self::get_facet_counts_fuzzy`] but also reports whether the
    /// vocalized term collection truncated.
    pub fn get_facet_counts_fuzzy_with_status(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<FacetCountsResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, truncated) = self.build_fuzzy_count_query(&query, &facets, max_distance, &voc)?;
        Ok(FacetCountsResult {
            counts: self.run_facet_counts(q, facet_prefix)?,
            truncated,
        })
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    /// `(textStored, textVocalizedStored)` — the display copies of
    /// [`TextStorage::InIndex`] documents.
    #[cfg(test)]
    pub(crate) fn text_analyzer_for_tests(&self) -> TextAnalyzer {
        self.text_index_analyzer().unwrap()
    }

    #[cfg(test)]
    pub(crate) fn searcher_for_tests(&self) -> Searcher {
        self.index_reader.searcher()
    }

    /// Reloads the reader now, as `OnCommitWithDelay` does shortly after another engine's
    /// commit.
    #[cfg(test)]
    pub(crate) fn reload_reader_for_tests(&self) {
        self.index_reader.reload().unwrap();
    }

    /// Whether this engine holds the index writer.
    #[cfg(test)]
    pub(crate) fn holds_writer_for_tests(&self) -> bool {
        self.index_writer.is_some()
    }

    /// The exact query, or the advanced one at `distance` when given, with or
    /// without its cross-line part.
    #[cfg(test)]
    pub(crate) fn phrase_query_for_tests(
        &self,
        query: &str,
        distance: Option<u32>,
        cross_line: bool,
    ) -> Box<dyn Query> {
        self.filtered_phrase_query_for_tests(query, distance, cross_line, &[])
    }

    #[cfg(test)]
    pub(crate) fn filtered_phrase_query_for_tests(
        &self,
        query: &str,
        distance: Option<u32>,
        cross_line: bool,
        facets: &[String],
    ) -> Box<dyn Query> {
        let plain = VocalizedFlags::new(false, false);
        match distance {
            None => {
                self.build_exact_query_with(query, facets, &plain, cross_line)
                    .unwrap()
                    .0
            }
            Some(distance) => {
                self.build_advanced_query(
                    query,
                    distance,
                    &HashMap::new(),
                    false,
                    &HashMap::new(),
                    &HashMap::new(),
                    facets.to_vec(),
                    &plain,
                    &SearchScope::WordDistance,
                    &WordMatch::All,
                    cross_line,
                )
                .unwrap()
                .0
            }
        }
    }

    /// The `text` field's indexing analyzer (with quote-free twins).
    /// The analyzer `lineEdge` terms are made with: twins included, as up to schema 5.
    fn text_index_analyzer(&self) -> Result<TextAnalyzer> {
        self.index
            .tokenizers()
            .get(DISPLAY_ANALYZER)
            .context("the hebrew display analyzer is not registered")
    }

    fn stored_text_fields(&self) -> Result<(Field, Field)> {
        Ok((
            self.schema.get_field(TEXT_STORED_FIELD)?,
            self.schema.get_field(TEXT_VOCALIZED_STORED_FIELD)?,
        ))
    }

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
            self.schema.get_field("textVocalized")?,
            self.schema.get_field(SECTION_OFFSET_FIELD)?,
            self.schema.get_field("lineHash")?,
        ))
    }

    fn ensure_write_access(&self) -> Result<()> {
        anyhow::ensure!(!self.read_only, "index was opened read-only");
        Ok(())
    }

    fn ensure_writer(&mut self) -> Result<()> {
        if self.index_writer.is_none() {
            debug!("writer: reopening lazily");
            self.index_writer = Some(self.open_writer()?);
            self.numbers_stale.store(true, Ordering::Release);
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
        self.ensure_write_access()?;
        anyhow::ensure!(!self.writer_closed, "index writer is permanently closed");
        let threads = crate::indexing_buffers::writer_threads(
            self.economy_indexing,
            std::thread::available_parallelism().map_or(1, |n| n.get()),
            cfg!(any(target_os = "android", target_os = "ios")),
        );
        let writer = self
            .index
            .writer_with_num_threads(threads, self.writer_heap_size)?;
        crate::indexing_buffers::cleanup_abandoned(&self.index_path);
        if self.bulk_indexing {
            writer.set_merge_policy(Box::new(NoMergePolicy));
        } else {
            writer.set_merge_policy(Box::new(SizeAwareMergePolicy {
                index_path: self.index_path.clone(),
            }));
        }
        Ok(writer)
    }

    fn open_writer_no_merge(&self) -> Result<IndexWriter> {
        let writer = self.open_writer()?;
        writer.set_merge_policy(Box::new(NoMergePolicy));
        Ok(writer)
    }

    fn optimize_committed_segments(&self) -> Result<()> {
        let mut maintenance_writer = self.open_writer_no_merge()?;
        loop {
            let metas = self.index.searchable_segment_metas()?;
            let sized_metas = sized_segment_metas(&self.index_path, metas)?;
            let Some(merge_ids) = select_segments_to_compact(&sized_metas) else {
                debug!(
                    "optimize: {} segments, nothing to compact",
                    sized_metas.len()
                );
                break;
            };
            debug!(
                "optimize: merging {} of {} segments",
                merge_ids.len(),
                sized_metas.len()
            );
            maintenance_writer.merge(&merge_ids).wait()?;
        }
        let wait_result = maintenance_writer.wait_merging_threads();
        wait_result?;
        Ok(())
    }

    fn restore_writer(&mut self) -> Result<()> {
        self.index_writer = Some(self.open_writer()?);
        self.numbers_stale.store(true, Ordering::Release);
        Ok(())
    }

    /// String-API entry: terms arriving as raw regex strings (the public
    /// `search`/`count` family) are split on their top-level alternation so a
    /// single-word query compiles per branch, exactly like the advanced path.
    /// `slop` here means what it means everywhere in this engine: the
    /// intermediate-word allowance between *each* adjacent pair (uniform, in
    /// order) — not tantivy's cumulative unordered budget.
    fn build_query(
        &self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<(Box<dyn Query>, bool)> {
        let patterns: Vec<hebrew_query::WordPattern> = regex_terms
            .iter()
            .map(|t| hebrew_query::WordPattern::parse(t))
            .collect();
        let proximity = Proximity::Ordered(vec![slop; patterns.len().saturating_sub(1)]);
        let text_field = self.schema.get_field("text")?;
        self.build_query_from_patterns(
            patterns,
            &[],
            &[],
            facets,
            &proximity,
            max_expansions,
            text_field,
            &SearchScope::WordDistance,
            &WordMatch::All,
            true,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_advanced_negative_query(
        &self,
        positive_query: Box<dyn Query>,
        positive_truncated: bool,
        negative_query: &str,
        negative_distance: u32,
        negative_custom_spacing: &HashMap<String, String>,
        negative_alternative_words: &HashMap<u32, Vec<String>>,
        negative_search_options: &HashMap<String, HashMap<String, bool>>,
        voc: &VocalizedFlags,
        negative_scope: &SearchScope,
        any_order: bool,
    ) -> Result<(Box<dyn Query>, bool)> {
        if hebrew_query::split_query_words(negative_query).is_empty() {
            return Ok((positive_query, positive_truncated));
        }

        // שלילה בהתאמה חלקית הייתה פוסלת כל תוצאה שנושאת ולו מילת-שלילה
        // אחת — לכן צירוף מילות השלילה נשאר תמיד "כל המילים".
        let (negative, _, _, negative_truncated, _) = self.build_advanced_query(
            negative_query,
            negative_distance,
            negative_custom_spacing,
            any_order,
            negative_alternative_words,
            negative_search_options,
            Vec::new(),
            voc,
            negative_scope,
            &WordMatch::All,
            false,
        )?;

        // שלילה בטווח "תחת אותה כותרת" פוסלת *סעיפים שלמים*: השאילתה שנבנתה
        // מחזירה רק את השורות שנושאות מילת שלילה, כך שב-MustNot היא הייתה
        // מותירה את שאר שורות הסעיף. במקומה נאספים ה-sectionId שהיא חותכת
        // וכל שורה בסעיף כזה נחסמת.
        let negative: Box<dyn Query> = if matches!(negative_scope, SearchScope::SameSection) {
            let sections = self
                .index_reader
                .searcher()
                .search(negative.as_ref(), &SectionIdsCollector)?;
            if sections.is_empty() {
                return Ok((positive_query, positive_truncated || negative_truncated));
            }
            Box::new(SectionFilteredQuery::new(
                Box::new(AllQuery),
                Arc::new(sections),
            ))
        } else {
            negative
        };

        Ok((
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, positive_query),
                (
                    Occur::MustNot,
                    Box::new(crate::negative_query::NegativeQuery::new(negative)),
                ),
            ])),
            positive_truncated || negative_truncated,
        ))
    }

    /// `text_field` selects the dictionary the patterns run against: the
    /// plain `text` field, or `textVocalized` on the vocalized paths (whose
    /// patterns carry free-mark runs and must never hit the plain field).
    ///
    /// `proximity` holds the per-pair intermediate-word allowance (`gaps[i]`
    /// between words `i` and `i+1`) or the any-order window. The phrase branch
    /// goes through [`Self::phrase_query_with_degrade`]: on its exact path tantivy gets
    /// the *sum* as its slop — tantivy spends slop cumulatively across the
    /// phrase, so the max would reject a match using its allowance in two
    /// different gaps — wrapped in [`GapVerifiedPhraseQuery`], which
    /// re-checks candidates against the positional postings so only
    /// in-order, per-pair-within-allowance occurrences survive; on its
    /// degrade path [`TermListPhraseQuery`] applies the same per-pair
    /// verification directly.
    #[allow(clippy::too_many_arguments)]
    fn build_query_from_patterns(
        &self,
        regex_terms: Vec<hebrew_query::WordPattern>,
        source_words: &[String],
        typo_tokens: &[String],
        facets: Vec<String>,
        proximity: &Proximity,
        max_expansions: u32,
        text_field: Field,
        scope: &SearchScope,
        match_mode: &WordMatch,
        cross_line: bool,
        ranking_roots: Option<&[String]>,
    ) -> Result<(Box<dyn Query>, bool)> {
        // Resolved up front: the same facet filter both narrows the section
        // pre-pass (fewer candidate sections to intersect) and gates the
        // final result set. סמנטיקת הממדים (OR בתוך ממד, AND ביניהם)
        // נבנית ב-facet_filter_query.
        let facets_query: Option<Box<dyn Query>> = if facets.is_empty() {
            None
        } else {
            Some(self.facet_filter_query(&facets)?)
        };

        // Every word-materialization path (single word / paragraph / section
        // scopes / the phrase degrade path) degrades under its collection
        // budgets and reports truncation; only the empty branch matches
        // nothing and reports none.
        let (main_query, truncated): (Box<dyn Query>, bool) = match regex_terms.len() {
            0 => (Box::new(EmptyQuery), false),
            1 => match ranking_roots {
                Some(roots) => self.ranked_term_query(
                    regex_terms[0].branches(),
                    typo_tokens,
                    text_field,
                    max_expansions,
                    roots,
                )?,
                None => self.single_regex_term_query(
                    regex_terms[0].branches(),
                    typo_tokens,
                    text_field,
                    max_expansions,
                )?,
            },
            _ => match (scope, match_mode) {
                // הטווחים "פסקה"/"כותרת" מוותרים על סדר ומרווח, וכך גם כל
                // מצב התאמה שאינו "כל המילים" (מילים עשויות לחסור) — כל
                // מילה מתממשת ל-TermSetQuery משלה והצירוף נעשה בין מסמכים
                // (פסקה) או בין סעיפים (כותרת).
                (SearchScope::SameParagraph | SearchScope::SameSection, _)
                | (_, WordMatch::AtLeast(_) | WordMatch::Most) => self.scoped_words_query(
                    &regex_terms,
                    source_words,
                    text_field,
                    max_expansions,
                    scope,
                    facets_query.as_deref(),
                    match_mode,
                )?,
                (SearchScope::WordDistance, WordMatch::All) => self.phrase_query_with_degrade(
                    &regex_terms,
                    proximity,
                    text_field,
                    max_expansions,
                    cross_line,
                    facets_query.as_deref(),
                )?,
            },
        };

        let Some(facets_query) = facets_query else {
            return Ok((main_query, truncated));
        };

        Ok((
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, main_query),
                (Occur::Must, facets_query),
            ])),
            truncated,
        ))
    }

    /// The ordered-phrase path (`WordDistance` + all words), with graceful
    /// degradation instead of tantivy's hard expansion error.
    ///
    /// The joined patterns are first counted with *Tantivy's own semantics*:
    /// one cumulative expansion count per segment. Only a phrase that would
    /// exceed that limit (or whose joined DFA cannot compile) materializes
    /// per-position term sets for the fallback engine. This keeps the normal
    /// `RegexPhraseQuery` path exact — including its BM25 phrase scoring —
    /// and avoids turning a large multi-segment union into a false fallback.
    ///
    /// - **Exact path** — every segment's cumulative count fits
    ///   `max_expansions`, and every joined pattern compiles as one DFA: the
    ///   historical `RegexPhraseQuery` (+ [`GapVerifiedPhraseQuery`] when
    ///   slop > 0), with identical scoring and behavior.
    /// - **Degrade path** — otherwise: a [`TermListPhraseQuery`] built from
    ///   the materialized (possibly truncated) term sets. No joined DFA, no
    ///   expansion ceiling to overflow — a phrase that used to die with
    ///   tantivy's `InvalidArgument("Phrase query exceeded max expansions")`
    ///   now serves complete results when only the *cumulative* ceiling was
    ///   the problem, and budget-truncated (highest-priority-first) results
    ///   with `truncated: true` when a single position outgrew its own caps.
    ///
    /// An any-order proximity always takes the degrade path: tantivy's slop is
    /// no superset of every word order.
    fn phrase_query_with_degrade(
        &self,
        regex_terms: &[hebrew_query::WordPattern],
        proximity: &Proximity,
        text_field: Field,
        max_expansions: u32,
        cross_line: bool,
        facets_query: Option<&dyn Query>,
    ) -> Result<(Box<dyn Query>, bool)> {
        let Proximity::Ordered(gaps) = proximity else {
            // Cross-line matching assumes an order; any-order stays within a line.
            return self.term_list_phrase_query(regex_terms, proximity, text_field, false, None);
        };
        debug_assert_eq!(gaps.len() + 1, regex_terms.len());
        let joined: Vec<String> = regex_terms
            .iter()
            .map(hebrew_query::WordPattern::joined)
            .collect();
        let slop_budget = gaps.iter().fold(0u32, |acc, &g| acc.saturating_add(g));
        // One quote-transparent DFA per word; the checks below and the query's weight share them.
        let automata: Option<Vec<Arc<QuoteTransparent<TermDfa>>>> = joined
            .iter()
            .map(|pattern| {
                term_automaton::compile_pattern(pattern)
                    .ok()
                    .map(|dfa| Arc::new(QuoteTransparent::from_arc(dfa)))
            })
            .collect();
        let joined_compiles = automata.is_some();
        let exceeds = match &automata {
            Some(automata) => {
                self.phrase_exceeds_max_expansions(automata, text_field, max_expansions)?
            }
            None => false,
        };
        if let (Some(automata), false) = (automata, exceeds) {
            let terms = joined
                .into_iter()
                .zip(automata)
                .enumerate()
                .map(|(offset, (pattern, automaton))| (offset, pattern, automaton))
                .collect();
            let mut phrase_query = RegexPhraseQuery::from_automata(text_field, terms, slop_budget);
            phrase_query.set_max_expansions(max_expansions);
            let inline: Box<dyn Query> = if slop_budget == 0 {
                Box::new(phrase_query)
            } else {
                Box::new(GapVerifiedPhraseQuery::new(
                    phrase_query,
                    text_field,
                    gaps.to_vec(),
                ))
            };
            if !cross_line {
                return Ok((inline, false));
            }
            let words: Vec<cross_line::CrossLineWord> = regex_terms
                .iter()
                .map(|p| cross_line::CrossLineWord::Pattern(p.joined()))
                .collect();
            return Ok((
                self.with_cross_line(inline, text_field, &words, gaps, facets_query)?,
                false,
            ));
        }

        if !joined_compiles {
            info!(
                "joined phrase pattern exceeds the DFA limits; serving the term-list degrade path"
            );
        } else {
            info!(
                "phrase expansions exceed the exact RegexPhraseQuery ceiling ({max_expansions}) \
                 in at least one segment; serving the term-list degrade path"
            );
        }

        self.term_list_phrase_query(regex_terms, proximity, text_field, cross_line, facets_query)
    }

    /// The degrade path of [`Self::phrase_query_with_degrade`]: a
    /// [`TermListPhraseQuery`] over the materialized term sets.
    fn term_list_phrase_query(
        &self,
        regex_terms: &[hebrew_query::WordPattern],
        proximity: &Proximity,
        text_field: Field,
        cross_line: bool,
        facets_query: Option<&dyn Query>,
    ) -> Result<(Box<dyn Query>, bool)> {
        // Materialize every *distinct* branch set once, in parallel, only
        // after the exact engine is known to be unusable. This is the costly
        // work the fallback actually needs; doing it for all phrases both
        // duplicated Tantivy's normal FST scan and changed valid phrases'
        // ranking on multi-segment indexes.
        use rayon::prelude::*;
        let mut unique_branches: Vec<&[String]> = Vec::new();
        let mut set_index: Vec<usize> = Vec::with_capacity(regex_terms.len());
        for pattern in regex_terms {
            let branches = pattern.branches();
            match unique_branches.iter().position(|u| *u == branches) {
                Some(i) => set_index.push(i),
                None => {
                    set_index.push(unique_branches.len());
                    unique_branches.push(branches);
                }
            }
        }
        let unique_sets: Vec<CachedTermSet> = unique_branches
            .par_iter()
            .copied()
            .map(|branches| {
                self.materialize_term_set(
                    branches,
                    &[],
                    text_field,
                    PHRASE_POSITION_MAX_EXPANSIONS,
                    PHRASE_POSITION_POSTINGS_BUDGET,
                )
            })
            .collect::<Result<_>>()?;

        let mut truncated = false;
        let mut position_terms: Vec<Vec<Term>> = Vec::with_capacity(regex_terms.len());
        for &set_i in &set_index {
            let entry = &unique_sets[set_i];
            if entry.terms.is_empty() {
                return Ok((Box::new(EmptyQuery), false));
            }
            truncated |= entry.truncated;
            position_terms.push(entry.terms.as_ref().clone());
        }
        let words: Vec<cross_line::CrossLineWord> = position_terms
            .iter()
            .map(|terms| {
                cross_line::CrossLineWord::Terms(
                    terms
                        .iter()
                        .filter_map(|t| t.value().as_str().map(str::to_string))
                        .collect(),
                )
            })
            .collect();
        let inline: Box<dyn Query> = Box::new(TermListPhraseQuery::new(
            text_field,
            position_terms,
            proximity.clone(),
        ));
        let (true, Proximity::Ordered(gaps)) = (cross_line, proximity) else {
            return Ok((inline, truncated));
        };
        Ok((
            self.with_cross_line(inline, text_field, &words, gaps, facets_query)?,
            truncated,
        ))
    }

    /// Mirrors `RegexPhraseWeight`'s expansion accounting exactly: the
    /// matching terms of every word position are counted cumulatively, but
    /// the counter resets for each segment. A global union is not equivalent
    /// here — disjoint vocabularies in several segments may be larger than
    /// `max_expansions` together while every segment remains valid for
    /// Tantivy's BM25 phrase scorer.
    fn phrase_exceeds_max_expansions<A>(
        &self,
        regexes: &[Arc<A>],
        text_field: Field,
        max_expansions: u32,
    ) -> Result<bool>
    where
        A: Automaton,
        A::State: Clone,
    {
        let cancellation = crate::lexical_cancellation::LexicalFlag::current();
        let searcher = self.index_reader.searcher();
        for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(text_field)?;
            let mut segment_terms = 0usize;
            for regex in regexes {
                let mut stream = inverted.terms().search(regex.as_ref()).into_stream()?;
                while stream.advance() {
                    cancellation.check()?;
                    segment_terms += 1;
                    if segment_terms > max_expansions as usize {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Multi-word query for the paragraph/section scopes (and every partial
    /// match mode): every distinct word is materialized into its own
    /// `TermSetQuery` (same budgets and cache as the single-word path —
    /// each word executes exactly like it would alone), then combined:
    ///
    /// - **Paragraph** (וגם `WordDistance` בהתאמה חלקית) — boolean בין
    ///   מסמכים: כשכל המילים נדרשות — AND; פחות מזה — Should עם מינימום
    ///   נדרש (מסמך עם יותר מילים מקבל score גבוה יותר).
    /// - **Section** — a two-pass plan (see the `section_scope` module):
    ///   keep the sections whose per-word `sectionId` sets cover at least
    ///   the required word count (intersection when all are required),
    ///   then serve the lines that carry any query word inside such a
    ///   section. The facet filter narrows the pre-pass so a facet-excluded
    ///   book cannot bloat the section sets (correctness never depends on
    ///   it — section ids are unique per book).
    ///
    /// הסף נגזר מ-`match_mode` על מספר המילים **הייחודיות**: מופעים כפולים
    /// של אותה מילת-מקור מתמזגים ל-clause אחד (איחוד הענפים של כולם — גם
    /// כשאפשרויות פר-מילה נותנות להם תבניות שונות), אחרת clause כפול היה
    /// מספק את הסף פעמיים. בהיעדר מילות מקור (ה-API המחרוזתי, חלופות ר"ת)
    /// התבנית עצמה היא מפתח המיזוג.
    ///
    /// `truncated` is the OR over the per-word collection truncations.
    #[allow(clippy::too_many_arguments)]
    fn scoped_words_query(
        &self,
        regex_terms: &[hebrew_query::WordPattern],
        source_words: &[String],
        text_field: Field,
        max_expansions: u32,
        scope: &SearchScope,
        facets_query: Option<&dyn Query>,
        match_mode: &WordMatch,
    ) -> Result<(Box<dyn Query>, bool)> {
        let mut by_word: HashMap<String, usize> = HashMap::with_capacity(regex_terms.len());
        let mut merged_branches: Vec<Vec<String>> = Vec::with_capacity(regex_terms.len());
        for (i, pattern) in regex_terms.iter().enumerate() {
            let key = source_words
                .get(i)
                .cloned()
                .unwrap_or_else(|| pattern.joined());
            match by_word.entry(key) {
                std::collections::hash_map::Entry::Occupied(e) => {
                    let branches = &mut merged_branches[*e.get()];
                    for branch in pattern.branches() {
                        if !branches.iter().any(|b| b == branch) {
                            branches.push(branch.clone());
                        }
                    }
                }
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(merged_branches.len());
                    merged_branches.push(pattern.branches().to_vec());
                }
            }
        }
        // Every distinct word materializes independently (same scans, same
        // cache) — fan them out over the pool like the phrase path does.
        use rayon::prelude::*;
        let materialized: Vec<(Box<dyn Query>, bool)> = merged_branches
            .par_iter()
            .map(|branches| self.single_regex_term_query(branches, &[], text_field, max_expansions))
            .collect::<Result<_>>()?;
        let mut truncated = false;
        let mut word_queries: Vec<Box<dyn Query>> = Vec::with_capacity(materialized.len());
        for (word_query, word_truncated) in materialized {
            truncated |= word_truncated;
            word_queries.push(word_query);
        }
        let min_required = match_mode.min_words(word_queries.len());
        let all_required = min_required >= word_queries.len();

        // סף של מילה אחת שקול ל-OR שטוח גם בטווח הסעיף: שורה שנושאת מילה
        // כלשהי שייכת ממילא לסעיף שמכיל אותה — ה-pre-pass מיותר.
        if !matches!(scope, SearchScope::SameSection) || min_required <= 1 {
            let query: Box<dyn Query> = if all_required {
                let clauses: Vec<(Occur, Box<dyn Query>)> =
                    word_queries.into_iter().map(|q| (Occur::Must, q)).collect();
                Box::new(BooleanQuery::new(clauses))
            } else {
                let clauses: Vec<(Occur, Box<dyn Query>)> = word_queries
                    .into_iter()
                    .map(|q| (Occur::Should, q))
                    .collect();
                Box::new(BooleanQuery::with_minimum_required_clauses(
                    clauses,
                    min_required,
                ))
            };
            return Ok((query, truncated));
        }

        // Section scope — pass 1: the sections carrying enough of the words.
        let searcher = self.index_reader.searcher();
        let mut allowed: Option<HashSet<u64>> = None;
        let mut section_word_counts: HashMap<u64, usize> = HashMap::new();
        for word_query in &word_queries {
            let sections = match facets_query {
                Some(fq) => searcher.search(
                    &BooleanQuery::new(vec![
                        (Occur::Must, word_query.box_clone()),
                        (Occur::Must, fq.box_clone()),
                    ]),
                    &SectionIdsCollector,
                )?,
                None => searcher.search(word_query.as_ref(), &SectionIdsCollector)?,
            };
            if !all_required {
                truncated |= accumulate_section_counts(
                    &mut section_word_counts,
                    sections,
                    SECTION_COUNT_BUDGET,
                );
                continue;
            }
            allowed = Some(match allowed {
                None => sections,
                Some(prev) => prev.intersection(&sections).copied().collect(),
            });
            if allowed.as_ref().is_some_and(HashSet::is_empty) {
                // A word with no section in common — no section can ever
                // contain all the words.
                return Ok((Box::new(EmptyQuery), truncated));
            }
        }
        if !all_required {
            let reaching: HashSet<u64> = section_word_counts
                .into_iter()
                .filter(|(_, count)| *count >= min_required)
                .map(|(section, _)| section)
                .collect();
            if reaching.is_empty() {
                return Ok((Box::new(EmptyQuery), truncated));
            }
            allowed = Some(reaching);
        }

        // Pass 2: the lines that carry any query word, gated to the
        // intersected sections.
        let union = BooleanQuery::new(
            word_queries
                .into_iter()
                .map(|q| (Occur::Should, q))
                .collect::<Vec<_>>(),
        );
        Ok((
            Box::new(SectionFilteredQuery::new(
                Box::new(union),
                Arc::new(allowed.unwrap_or_default()),
            )),
            truncated,
        ))
    }

    /// Single regex term: materialize the matching index terms into a
    /// `TermSetQuery`, bounded by two budgets — `max_expansions` (term count,
    /// a memory guard on the materialized `Vec<Term>`) and
    /// [`SINGLE_WORD_POSTINGS_BUDGET`] (summed doc_freq, the real execution
    /// cost). Unlike `RegexPhraseQuery`, overflow here *degrades*: collection
    /// stops and the highest-priority automatons collected so far are served
    /// (never an error). A bare `RegexQuery` would enumerate the term
    /// dictionary without any bound, so a broad pattern (e.g. a 1-char word
    /// with prefix+suffix options) could scan a huge slice of the index
    /// unchecked.
    ///
    /// Each alternation branch is compiled as its own DFA: a combined
    /// `(?:b1|…|bN)` of wildcard-wrapped branches overlaps so heavily that it
    /// blew the upstream tantivy-fst 1 000-state cap (48 typo+partial
    /// branches — 806 chars; the vendored cap is 8 192) while every branch
    /// alone is tiny. `typo_tokens` add one Levenshtein-1 automaton each,
    /// scanned after all branches. Everything streams into one shared
    /// `HashSet` under the shared budgets, so the resulting `TermSetQuery`
    /// is exactly the union of what every automaton matches.
    fn single_regex_term_query(
        &self,
        branches: &[String],
        typo_tokens: &[String],
        text_field: Field,
        max_expansions: u32,
    ) -> Result<(Box<dyn Query>, bool)> {
        let entry = self.materialize_term_set(
            branches,
            typo_tokens,
            text_field,
            max_expansions,
            SINGLE_WORD_POSTINGS_BUDGET,
        )?;
        Ok((
            Box::new(TermSetQuery::new(entry.terms.iter().cloned())),
            entry.truncated,
        ))
    }

    /// [`Self::single_regex_term_query`] scored in relevance tiers: the same
    /// terms (the result set is unchanged), split by
    /// [`hebrew_query::match_tier`] against the typed `roots` into
    /// constant-score clauses whose scores sum.
    fn ranked_term_query(
        &self,
        branches: &[String],
        typo_tokens: &[String],
        text_field: Field,
        max_expansions: u32,
        roots: &[String],
    ) -> Result<(Box<dyn Query>, bool)> {
        let entry = self.materialize_term_set(
            branches,
            typo_tokens,
            text_field,
            max_expansions,
            SINGLE_WORD_POSTINGS_BUDGET,
        )?;
        let mut tiers: [Vec<Term>; 4] = Default::default();
        for term in entry.terms.iter() {
            let tier = term
                .value()
                .as_str()
                .map_or(3, |text| hebrew_query::match_tier(text, roots));
            tiers[usize::from(tier)].push(term.clone());
        }
        let clauses: Vec<(Occur, Box<dyn Query>)> = tiers
            .into_iter()
            .zip(ADVANCED_TIER_BOOSTS)
            .filter(|(terms, _)| !terms.is_empty())
            .map(|(terms, boost)| {
                (
                    Occur::Should,
                    Box::new(ConstScoreQuery::new(
                        Box::new(TermSetQuery::new(terms)),
                        boost,
                    )) as Box<dyn Query>,
                )
            })
            .collect();
        let query: Box<dyn Query> = if clauses.is_empty() {
            Box::new(EmptyQuery)
        } else {
            Box::new(BooleanQuery::new(clauses))
        };
        Ok((query, entry.truncated))
    }

    /// The materialization behind [`Self::single_regex_term_query`] and the
    /// phrase degrade path: streams every branch's (and typo automaton's)
    /// dictionary matches into one term set under the two collection budgets,
    /// served from / stored into the LRU term cache. Overflow degrades —
    /// collection stops at the budget and the highest-priority prefix is
    /// returned with `truncated: true`, never an error.
    fn materialize_term_set(
        &self,
        branches: &[String],
        typo_tokens: &[String],
        text_field: Field,
        max_expansions: u32,
        postings_budget: u64,
    ) -> Result<CachedTermSet> {
        let memory_epoch = self.memory_epoch.load(Ordering::Acquire);
        let searcher = self.index_reader.searcher();
        // The materialization below (per-branch DFA compile + one FST scan
        // per branch per segment) is the expensive part of a search, and one
        // user search repeats it verbatim across stream/count/count-by-book/
        // facet/pagination calls — serve those from the cache. The searcher
        // generation in the key invalidates entries on reader reload.
        let cache_key = TermCacheKey {
            generation: searcher.generation().generation_id(),
            field: text_field,
            branches: branches.to_vec(),
            typo_tokens: typo_tokens.to_vec(),
            max_expansions,
            postings_budget,
        };
        if let Some(entry) = self.term_cache.lock().unwrap().get(&cache_key) {
            return Ok(entry.clone());
        }

        let work = self.term_work.enter(cache_key.clone())?;
        if let Some(entry) = self.term_cache.lock().unwrap().get(&cache_key) {
            return Ok(entry.clone());
        }
        #[cfg(test)]
        search_concurrency_tests::after_materialize_gate(&cache_key);
        crate::lexical_cancellation::check_current()?;
        let automata = term_automaton::compile_branches(branches).map_err(|e| {
            // Surface the failure loudly: the historical failure mode here
            // was a compile error silently becoming "0 results" in the UI.
            error!(
                "regex branches failed to compile ({} branches): {e}. First branch prefix: {}",
                branches.len(),
                branches
                    .first()
                    .map(|b| b.chars().take(80).collect::<String>())
                    .unwrap_or_default(),
            );
            anyhow::anyhow!("invalid regex branches ({} branches): {e}", branches.len())
        })?;
        let inverted_indexes = searcher
            .segment_readers()
            .iter()
            .map(|reader| reader.inverted_index(text_field))
            .collect::<tantivy::Result<Vec<_>>>()?;
        // Same builder configuration as the fuzzy path (distance 1,
        // transposition counts as one edit): the whole edit-distance-1
        // neighborhood of each typo token, ranked after every branch.
        let typo_automata = typo_tokens
            .iter()
            .map(|token| self.fuzzy_automaton(token, 1))
            .collect::<Result<Vec<_>>>()?;
        // Everything is gathered and ranked before a budget applies, so a
        // budget drops the lowest-ranked forms — not whatever the dictionary
        // order happened to reach last.
        let (ranked, capped) = term_automaton::collect_ranked(
            &inverted_indexes,
            &automata,
            &typo_automata,
            term_automaton::COLLECT_CAP.max(max_expansions as usize),
            // Rayon workers never wait for another owner. Keep their duplicate
            // scan inline so it cannot recursively yield to more contenders;
            // owners still fan out over segments even within outer word tasks.
            if work.is_some() {
                term_automaton::ScanMode::Parallel
            } else {
                term_automaton::ScanMode::Inline
            },
        )?;
        let (kept, truncated) = term_automaton::within_budgets(
            ranked,
            capped,
            max_expansions as usize,
            postings_budget,
        );
        if truncated {
            warn!(
                "term collection truncated at {} terms (caps: {max_expansions} terms,                  {postings_budget} postings); serving the highest-ranked terms",
                kept.len(),
            );
        }
        let terms: Arc<Vec<Term>> = Arc::new(
            kept.iter()
                .map(|t| Term::from_field_text(text_field, &t.term))
                .collect(),
        );
        let entry = CachedTermSet { terms, truncated };
        crate::lexical_cancellation::check_current()?;
        let bytes = |key: &TermCacheKey, entry: &CachedTermSet| {
            [&key.branches, &key.typo_tokens]
                .into_iter()
                .map(|terms| {
                    terms.capacity() * std::mem::size_of::<String>()
                        + terms.iter().map(String::capacity).sum::<usize>()
                })
                .sum::<usize>()
                + entry
                    .terms
                    .iter()
                    .map(|term| {
                        std::mem::size_of::<Term>() + term.value().as_str().map_or(0, str::len)
                    })
                    .sum::<usize>()
        };
        let mut cache = self.term_cache.lock().unwrap();
        if self.memory_epoch.load(Ordering::Acquire) == memory_epoch
            && bytes(&cache_key, &entry) <= 16 << 20
        {
            cache.put(cache_key, entry.clone());
            while cache
                .iter()
                .map(|(key, entry)| bytes(key, entry))
                .sum::<usize>()
                > 16 << 20
            {
                cache.pop_lru();
            }
        }
        Ok(entry)
    }

    /// Tokenizes `text` with the same `"hebrew"` analyzer the `text` field is
    /// indexed with — so exact/fuzzy terms line up with the (normalized) index
    /// term dictionary, including geresh/gershayim kept inside tokens (ז"ל,
    /// תוס'). No pre-normalization: the tokenizer already strips attached
    /// marks and folds presentation forms, and a `strip_nikud` pass here would
    /// also delete maqaf/sof-pasuq, gluing `"אשר־שמע"` into one bogus term.
    fn index_token_texts(&self, text: &str) -> Result<Vec<String>> {
        self.index_token_texts_with("hebrew_query", text)
    }

    /// [`Self::index_token_texts`] with an explicit analyzer —
    /// `"hebrew_vocalized"` tokenizes a vocalized query exactly like the
    /// `textVocalized` field (marks kept inside tokens).
    fn index_token_texts_with(&self, analyzer_name: &str, text: &str) -> Result<Vec<String>> {
        let mut analyzer = self
            .index
            .tokenizers()
            .get(analyzer_name)
            .with_context(|| format!("{analyzer_name} tokenizer not registered"))?;
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while let Some(token) = stream.next() {
            out.push(token.text.clone());
        }
        Ok(out)
    }

    /// The stored/indexed text field a search reads: `textVocalized` when a
    /// vocalized flag is on, the plain `text` field otherwise.
    fn search_text_field(&self, voc: &VocalizedFlags) -> Result<Field> {
        if voc.any() {
            Ok(self.schema.get_field("textVocalized")?)
        } else {
            Ok(self.schema.get_field("text")?)
        }
    }

    /// Facet filter sub-query over the `topics` facet field.
    ///
    /// הנתיבים מחולקים לקבוצות לפי המקטע הראשון: כל שורש ממדי
    /// ([`FACET_DIMENSION_ROOTS`] — `author`/`era`/`base`) הוא קבוצה
    /// משלו, וכל השאר (עץ הקטגוריות/ספרים) קבוצה אחת. בתוך קבוצה —
    /// `TermSetQuery` (OR, בהתאמת-קידומת של facet); בין קבוצות — AND.
    /// כך "תקופת ראשונים AND המדף הנבחר" מתנהג נכון, בעוד קריאה עם
    /// נתיבי קטגוריות בלבד שקולה בדיוק להתנהגות הקודמת (קבוצה אחת).
    fn facet_filter_query(&self, facets: &[String]) -> Result<Box<dyn Query>> {
        let topics_f = self.schema.get_field("topics")?;
        // הסדר דטרמיניסטי: קטגוריות תחילה ואז הממדים לפי סדר ההגדרה —
        // אינדקס 0 = קטגוריות, i+1 = הממד ה-i.
        let mut groups: Vec<Vec<Term>> = vec![Vec::new(); FACET_DIMENSION_ROOTS.len() + 1];
        for f in facets {
            let facet = Facet::from_text(f)?;
            let root = f.trim_start_matches('/').split('/').next().unwrap_or("");
            let group = FACET_DIMENSION_ROOTS
                .iter()
                .position(|d| *d == root)
                .map_or(0, |i| i + 1);
            groups[group].push(Term::from_facet(topics_f, &facet));
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = groups
            .into_iter()
            .filter(|terms| !terms.is_empty())
            .map(|terms| {
                (
                    Occur::Must,
                    Box::new(TermSetQuery::new(terms)) as Box<dyn Query>,
                )
            })
            .collect();
        Ok(match clauses.len() {
            // אין facets — פילטר ריק לא אמור להיקרא, אבל אם כן: הכל עובר.
            0 => Box::new(AllQuery),
            1 => clauses.pop().expect("one clause").1,
            _ => Box::new(BooleanQuery::new(clauses)),
        })
    }

    /// Exact mode: a `TermQuery` (one token) or `PhraseQuery` (several), filtered
    /// by facets. No regex — fastest path. With a vocalized flag on, the query
    /// runs against the `textVocalized` dictionary instead: each token becomes
    /// a required-marks regex ([`hebrew_query::vocalized_token_pattern`]), a
    /// single word materializes via [`Self::single_regex_term_query`] (which
    /// may truncate — the returned flag), a phrase goes through
    /// [`Self::phrase_query_with_degrade`].
    fn build_exact_query(
        &self,
        query_str: &str,
        facets: &[String],
        voc: &VocalizedFlags,
    ) -> Result<(Box<dyn Query>, bool)> {
        self.build_exact_query_with(query_str, facets, voc, true)
    }

    /// [`Self::build_exact_query`]; `cross_line: false` keeps a phrase inside one line.
    fn build_exact_query_with(
        &self,
        query_str: &str,
        facets: &[String],
        voc: &VocalizedFlags,
        cross_line: bool,
    ) -> Result<(Box<dyn Query>, bool)> {
        if voc.any() {
            return self.build_exact_query_vocalized(query_str, facets, voc);
        }
        let facets_query = (!facets.is_empty())
            .then(|| self.facet_filter_query(facets))
            .transpose()?;
        let text_f = self.schema.get_field("text")?;
        let token_texts = self.index_token_texts(query_str)?;
        let searcher = self.index_reader.searcher();
        let main_query: Box<dyn Query> = match token_texts.len() {
            0 => Box::new(EmptyQuery),
            1 => quote_expansion::term_query(
                &searcher,
                text_f,
                &token_texts[0],
                IndexRecordOption::Basic,
            )?,
            _ if cross_line => {
                let words: Vec<cross_line::CrossLineWord> = token_texts
                    .iter()
                    .map(|t| {
                        quote_expansion::spellings(&searcher, text_f, t)
                            .map(cross_line::CrossLineWord::Terms)
                    })
                    .collect::<tantivy::Result<_>>()?;
                let gaps = vec![0; words.len() - 1];
                self.with_cross_line(
                    quote_expansion::phrase_query(&searcher, text_f, &token_texts, 0)?,
                    text_f,
                    &words,
                    &gaps,
                    facets_query.as_deref(),
                )?
            }
            _ => quote_expansion::phrase_query(&searcher, text_f, &token_texts, 0)?,
        };
        if let Some(facets_query) = facets_query {
            Ok((
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, main_query),
                    (Occur::Must, facets_query),
                ])),
                false,
            ))
        } else {
            Ok((main_query, false))
        }
    }

    /// Smart queries add alternatives at each word position, retaining the typed spelling.
    fn build_smart_exact_query(
        &self,
        query: &str,
        facets: &[String],
        voc: &VocalizedFlags,
        cross_line: bool,
    ) -> Result<(Box<dyn Query>, bool)> {
        if !smart_query::has_alternatives(query) {
            return self.build_exact_query_with(query, facets, voc, cross_line);
        }
        if voc.any() {
            let patterns = self
                .index_token_texts_with("hebrew_vocalized_query", query)?
                .iter()
                .map(|token| smart_query::vocalized_pattern(token, voc))
                .collect::<Vec<_>>();
            return self.build_vocalized_exact_patterns(&patterns, facets);
        }
        let alternatives = self
            .index_token_texts(query)?
            .iter()
            .map(|token| smart_query::alternatives(token))
            .collect::<Vec<_>>();
        let searcher = self.index_reader.searcher();
        let field = self.schema.get_field("text")?;
        let filter = (!facets.is_empty())
            .then(|| self.facet_filter_query(facets))
            .transpose()?;
        let query = match alternatives.as_slice() {
            [] => Box::new(EmptyQuery) as Box<dyn Query>,
            [word] => quote_expansion::alternatives_term_query(
                &searcher,
                field,
                word,
                IndexRecordOption::Basic,
            )?,
            _ => {
                let phrase =
                    quote_expansion::alternatives_phrase_query(&searcher, field, &alternatives, 0)?;
                if cross_line {
                    let words = alternatives
                        .iter()
                        .map(|words| {
                            Ok(cross_line::CrossLineWord::Terms(
                                quote_expansion::spelling_terms(&searcher, field, words)?
                                    .into_iter()
                                    .filter_map(|term| term.value().as_str().map(str::to_string))
                                    .collect(),
                            ))
                        })
                        .collect::<tantivy::Result<Vec<_>>>()?;
                    self.with_cross_line(
                        phrase,
                        field,
                        &words,
                        &vec![0; words.len() - 1],
                        filter.as_deref(),
                    )?
                } else {
                    phrase
                }
            }
        };
        Ok((
            match filter {
                None => query,
                Some(filter) => Box::new(BooleanQuery::new(vec![
                    (Occur::Must, query),
                    (Occur::Must, filter),
                ])),
            },
            false,
        ))
    }

    /// `inline` or the same phrase continuing from the end of a line onto the next
    /// line (see [`cross_line`]). Only the plain `text` field records line edges.
    fn with_cross_line(
        &self,
        inline: Box<dyn Query>,
        text_field: Field,
        words: &[cross_line::CrossLineWord],
        gaps: &[u32],
        facets_query: Option<&dyn Query>,
    ) -> Result<Box<dyn Query>> {
        if text_field != self.schema.get_field("text")? {
            return Ok(inline);
        }
        Ok(
            match cross_line::CrossLineQuery::new(&self.schema, text_field, words, gaps)? {
                Some(cross) => Box::new(BooleanQuery::new(vec![
                    (Occur::Should, inline),
                    (
                        Occur::Should,
                        Box::new(cross.with_filter(facets_query.map(|query| query.box_clone()))),
                    ),
                ])),
                None => inline,
            },
        )
    }

    /// The vocalized arm of [`Self::build_exact_query`].
    fn build_exact_query_vocalized(
        &self,
        query_str: &str,
        facets: &[String],
        voc: &VocalizedFlags,
    ) -> Result<(Box<dyn Query>, bool)> {
        let tokens = self.index_token_texts_with("hebrew_vocalized_query", query_str)?;
        let patterns: Vec<String> = tokens
            .iter()
            .map(|t| hebrew_query::vocalized_token_pattern(t, voc))
            .collect();
        self.build_vocalized_exact_patterns(&patterns, facets)
    }

    fn build_vocalized_exact_patterns(
        &self,
        patterns: &[String],
        facets: &[String],
    ) -> Result<(Box<dyn Query>, bool)> {
        let voc_field = self.schema.get_field("textVocalized")?;
        let (main_query, truncated): (Box<dyn Query>, bool) = match patterns.len() {
            0 => (Box::new(EmptyQuery), false),
            1 => self.single_regex_term_query(
                patterns,
                &[],
                voc_field,
                VOC_EXACT_SINGLE_MAX_EXPANSIONS,
            )?,
            _ => {
                // Free-mark runs make every vocalized token an expansion, so
                // a phrase of common words can outgrow the cumulative
                // `RegexPhraseQuery` ceiling; the shared degrade path keeps
                // the historical query when the counts fit and falls back to
                // the term-list phrase instead of erroring when they don't.
                let word_patterns: Vec<hebrew_query::WordPattern> = patterns
                    .iter()
                    .map(|p| hebrew_query::WordPattern::parse(p))
                    .collect();
                let proximity = Proximity::adjacent(word_patterns.len());
                self.phrase_query_with_degrade(
                    &word_patterns,
                    &proximity,
                    voc_field,
                    VOC_PHRASE_MAX_EXPANSIONS,
                    false,
                    None,
                )?
            }
        };
        if facets.is_empty() {
            Ok((main_query, truncated))
        } else {
            Ok((
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, main_query),
                    (Occur::Must, self.facet_filter_query(facets)?),
                ])),
                truncated,
            ))
        }
    }

    /// Expands vocalized typo/fuzzy tokens: scans the PLAIN dictionary with a
    /// Levenshtein automaton over each mark-free base (edit distance over
    /// marked terms would count every mark as an edit), and returns one
    /// free-mark branch per *existing* variant, re-projected onto the
    /// vocalized dictionary. The base itself is skipped — the caller already
    /// carries it as the required-marks exact branch, and a free duplicate
    /// would silently erase the typed-marks constraint.
    fn vocalized_variant_branches(
        &self,
        searcher: &Searcher,
        bases: &[String],
        distance: u8,
        seen: &mut HashSet<String>,
    ) -> Result<Vec<String>> {
        if bases.is_empty() || distance == 0 {
            return Ok(Vec::new());
        }
        let plain_field = self.schema.get_field("text")?;
        let mut branches = Vec::new();
        for base in bases {
            let automaton = self.fuzzy_automaton(base, distance)?;
            for variant in self.automaton_terms_in_field(
                searcher,
                plain_field,
                &automaton,
                VOC_VARIANTS_PER_TOKEN,
            )? {
                if &variant == base || !seen.insert(variant.clone()) {
                    continue;
                }
                branches.push(hebrew_query::vocalized_free_pattern(&variant));
                if branches.len() >= VOC_VARIANTS_PER_TOKEN {
                    return Ok(branches);
                }
            }
        }
        Ok(branches)
    }

    /// The vocalized arm of the approximate (`fuzzy`) mode. Per token, the
    /// branch list runs highest-priority-first (the collection contract of
    /// [`Self::single_regex_term_query`]): the exact form with its typed
    /// marks REQUIRED, then the quote-free spelling, then the lexicon's
    /// morphological relatives, then existing edit-distance variants — the
    /// last three mark-free (their letters differ from what was typed, so
    /// the typed marks have no positions to attach to). Each token
    /// materializes into a `TermSetQuery` over `textVocalized`; multi-word
    /// queries AND the tokens like the plain fuzzy path (documents holding
    /// all words anywhere — no phrase constraint). No relevance tiers: the
    /// vocalized paths order by catalogue, and an unranked recall query pays
    /// nothing for scoring it never uses.
    fn build_fuzzy_query_vocalized(
        &self,
        query_str: &str,
        facets: &[String],
        max_distance: u8,
        voc: &VocalizedFlags,
    ) -> Result<(Box<dyn Query>, bool)> {
        anyhow::ensure!(
            max_distance <= 2,
            "fuzzy distance is limited to 2, got {max_distance}"
        );
        let tokens = self.index_token_texts_with("hebrew_vocalized_query", query_str)?;
        if tokens.is_empty() {
            return Ok((Box::new(EmptyQuery), false));
        }
        let voc_field = self.schema.get_field("textVocalized")?;
        let searcher = self.index_reader.searcher();
        let mut truncated = false;
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(tokens.len() + 1);
        for token in &tokens {
            let base = hebrew_query::strip_attached_marks(token);
            let mut branches: Vec<String> = vec![hebrew_query::vocalized_token_pattern(token, voc)];
            let mut seen: HashSet<String> = HashSet::from([base.clone()]);
            if let Some(clean) = Self::quoteless_variant(&base) {
                if seen.insert(clean.clone()) {
                    branches.push(hebrew_query::vocalized_free_pattern(&clean));
                }
            }
            if max_distance > 0 {
                if let Some(dict) = self.magic_dict.as_ref() {
                    for form in dict.recall_forms(&base, MAX_LEXICAL_FORMS) {
                        if seen.insert(form.clone()) {
                            branches.push(hebrew_query::vocalized_free_pattern(&form));
                        }
                    }
                }
                branches.extend(self.vocalized_variant_branches(
                    &searcher,
                    std::slice::from_ref(&base),
                    max_distance,
                    &mut seen,
                )?);
            }
            let (token_query, token_truncated) =
                self.single_regex_term_query(&branches, &[], voc_field, VOC_FUZZY_MAX_EXPANSIONS)?;
            truncated |= token_truncated;
            clauses.push((Occur::Must, token_query));
        }
        if !facets.is_empty() {
            clauses.push((Occur::Must, self.facet_filter_query(facets)?));
        }
        let query: Box<dyn Query> = if clauses.len() == 1 {
            clauses.pop().expect("one clause").1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };
        Ok((query, truncated))
    }

    /// The quote-free spelling of a token that carries gershayim/geresh
    /// (`רמב"ם` → `רמבם`). Clean-typography editions store that term, so the
    /// fuzzy builders add it as an exact-tier alternative: the bridge works
    /// even at distance 0, and clean-edition hits rank as exact matches
    /// instead of edit-distance tail matches. `None` when the token has no
    /// quotes (the common case) or nothing remains without them.
    fn quoteless_variant(token: &str) -> Option<String> {
        if !token.contains(['"', '\'']) {
            return None;
        }
        let clean: String = token.chars().filter(|c| !matches!(c, '"' | '\'')).collect();
        (!clean.is_empty()).then_some(clean)
    }

    /// The two summed `Should` clauses that lift an exact-token hit to the top
    /// relevance tier: a `ConstScoreQuery` floor (immune to BM25 `idf` collapse
    /// on near-ubiquitous terms) plus a small BM25 `TermQuery` add-on for
    /// intra-exact ordering. Only used on the ranked (`Relevance`) fuzzy path.
    fn exact_rank_clauses(
        searcher: &Searcher,
        text_f: Field,
        token: &str,
    ) -> Result<Vec<(Occur, Box<dyn Query>)>> {
        let exact = |option| quote_expansion::term_query(searcher, text_f, token, option);
        Ok(vec![
            (
                Occur::Should,
                Box::new(ConstScoreQuery::new(
                    exact(IndexRecordOption::Basic)?,
                    FUZZY_BOOST_EXACT,
                )) as Box<dyn Query>,
            ),
            (
                Occur::Should,
                Box::new(BoostQuery::new(
                    exact(IndexRecordOption::WithFreqs)?,
                    FUZZY_BOOST_EXACT_REL,
                )),
            ),
        ])
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
        let searcher = self.index_reader.searcher();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = term_texts
            .iter()
            .map(|t| -> Result<(Occur, Box<dyn Query>)> {
                let fuzzy = AutomatonQuery::new(
                    text_f,
                    format!("{t}~{max_distance}"),
                    self.fuzzy_automaton(t, max_distance)?,
                );
                // Bare recall is one fuzzy automaton per token. distance 0 is
                // exact already, and unranked paths (count/catalogue) need no
                // scoring, so both stay the bare query — except that a
                // quote-bearing token also matches its quote-free spelling
                // (clean-typography editions), even at distance 0. On the
                // ranked path above distance 0 we add the exact tier so an
                // exact hit outranks a bare edit-distance neighbour; the exact
                // term is a subset of the fuzzy match, so recall is unchanged.
                let token_query: Box<dyn Query> = if !rank || max_distance == 0 {
                    match Self::quoteless_variant(t) {
                        Some(clean) => Box::new(BooleanQuery::new(vec![
                            (Occur::Should, Box::new(fuzzy) as Box<dyn Query>),
                            (
                                Occur::Should,
                                quote_expansion::term_query(
                                    &searcher,
                                    text_f,
                                    &clean,
                                    IndexRecordOption::Basic,
                                )?,
                            ),
                        ])),
                        None => Box::new(fuzzy),
                    }
                } else {
                    let mut should = Self::exact_rank_clauses(&searcher, text_f, t)?;
                    if let Some(clean) = Self::quoteless_variant(t) {
                        should.extend(Self::exact_rank_clauses(&searcher, text_f, &clean)?);
                    }
                    should.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(Box::new(fuzzy), FUZZY_BOOST_FUZZY)),
                    ));
                    Box::new(BooleanQuery::new(should))
                };
                Ok((Occur::Must, token_query))
            })
            .collect::<Result<_>>()?;
        if rank {
            clauses.extend(Self::exact_phrase_boost(&searcher, text_f, term_texts)?);
        }
        if !facets.is_empty() {
            clauses.push((Occur::Must, self.facet_filter_query(facets)?));
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    /// The ranking-only `Should` that lifts hits where the typed words stand
    /// adjacent (within [`LEXICAL_FUZZY_PHRASE_SLOP`]) above scattered ones.
    fn exact_phrase_boost(
        searcher: &Searcher,
        text_f: Field,
        term_texts: &[String],
    ) -> Result<Option<(Occur, Box<dyn Query>)>> {
        let Some(phrase) = Self::exact_phrase(searcher, text_f, term_texts)? else {
            return Ok(None);
        };
        Ok(Some((
            Occur::Should,
            Box::new(ConstScoreQuery::new(phrase, FUZZY_BOOST_PHRASE)),
        )))
    }

    /// The typed words, exact, within [`LEXICAL_FUZZY_PHRASE_SLOP`]; `None`
    /// for a single word.
    fn exact_phrase(
        searcher: &Searcher,
        text_f: Field,
        words: &[String],
    ) -> Result<Option<Box<dyn Query>>> {
        if words.len() < 2 {
            return Ok(None);
        }
        Ok(Some(quote_expansion::phrase_query(
            searcher,
            text_f,
            words,
            LEXICAL_FUZZY_PHRASE_SLOP,
        )?))
    }

    /// Whether one quotation contains every searchable word of the query. Independent
    /// quotations remain independent phrases, even when no words stand outside them.
    fn whole_query_quoted(&self, query: &str) -> Result<bool> {
        let phrases = quoted_phrases(query);
        if phrases.len() != 1 {
            return Ok(false);
        }
        let words = self.index_token_texts(query)?;
        Ok(!words.is_empty() && words == self.index_token_texts(&phrases[0])?)
    }

    /// Smart search alone treats quoted words as literal. The regular approximate API's
    /// morphology-aware quoted phrases retain their existing matching rules.
    fn build_smart_fuzzy_query(
        &self,
        query: &str,
        facets: &[String],
        max_distance: u8,
        voc: &VocalizedFlags,
        rank: bool,
    ) -> Result<(Box<dyn Query>, bool)> {
        let (fuzzy, mut truncated) = if smart_query::has_alternatives(query) {
            self.build_smart_fuzzy_alternatives(query, facets, max_distance, voc, rank)?
        } else if voc.any() {
            self.build_fuzzy_query_vocalized(query, facets, max_distance, voc)?
        } else {
            let words = self.index_token_texts(query)?;
            (
                self.build_fuzzy_search_query(&words, &[], facets, max_distance, rank)?,
                false,
            )
        };
        let phrases = quoted_phrases(query);
        if phrases.is_empty() {
            return Ok((fuzzy, truncated));
        }
        let mut clauses = vec![(Occur::Must, fuzzy)];
        for phrase in phrases {
            // A quote's own words must occur literally and consecutively. This includes
            // single-word quotes and works without lexical.db. Preserve typed vocalization.
            let (literal, literal_truncated) =
                self.build_smart_exact_query(&phrase, &[], voc, false)?;
            truncated |= literal_truncated;
            clauses.push((Occur::Must, literal));
        }
        Ok((Box::new(BooleanQuery::new(clauses)), truncated))
    }

    fn build_smart_fuzzy_alternatives(
        &self,
        query: &str,
        facets: &[String],
        max_distance: u8,
        voc: &VocalizedFlags,
        rank: bool,
    ) -> Result<(Box<dyn Query>, bool)> {
        anyhow::ensure!(
            max_distance <= 2,
            "fuzzy distance is limited to 2, got {max_distance}"
        );
        let analyzer = if voc.any() {
            "hebrew_vocalized_query"
        } else {
            "hebrew_query"
        };
        let words = self
            .index_token_texts_with(analyzer, query)?
            .iter()
            .map(|word| smart_query::alternatives(word))
            .collect::<Vec<_>>();
        if words.is_empty() {
            return Ok((Box::new(EmptyQuery), false));
        }
        let mut clauses = Vec::new();
        let mut truncated = false;
        for alternatives in &words {
            let mut choices = Vec::new();
            for word in alternatives {
                let (query, capped) = if voc.any() {
                    self.build_fuzzy_query_vocalized(word, &[], max_distance, voc)?
                } else {
                    (
                        self.build_fuzzy_search_query(
                            std::slice::from_ref(word),
                            &[],
                            &[],
                            max_distance,
                            rank,
                        )?,
                        false,
                    )
                };
                choices.push((Occur::Should, query));
                truncated |= capped;
            }
            clauses.push((
                Occur::Must,
                Box::new(BooleanQuery::new(choices)) as Box<dyn Query>,
            ));
        }
        if rank && !voc.any() && words.len() >= 2 {
            let searcher = self.index_reader.searcher();
            let field = self.schema.get_field("text")?;
            let exact = quote_expansion::alternatives_phrase_query(
                &searcher,
                field,
                &words,
                LEXICAL_FUZZY_PHRASE_SLOP,
            )?;
            clauses.push((
                Occur::Should,
                Box::new(ConstScoreQuery::new(exact, FUZZY_BOOST_PHRASE)),
            ));
            if LEXICAL_FUZZY_EXPANDED_PHRASE_BOOST {
                if let Some(dict) = &self.magic_dict {
                    let terms = words
                        .iter()
                        .map(|alternatives| {
                            let forms = self
                                .lexical_fuzzy_phrase_terms(dict, alternatives, max_distance)?
                                .into_iter()
                                .flatten()
                                .collect::<Vec<_>>();
                            Ok(quote_expansion::spelling_terms(&searcher, field, &forms)?)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let expanded = TermListPhraseQuery::new(
                        field,
                        terms,
                        Proximity::Ordered(vec![LEXICAL_FUZZY_PHRASE_SLOP; words.len() - 1]),
                    );
                    clauses.push((
                        Occur::Should,
                        Box::new(ConstScoreQuery::new(
                            Box::new(expanded),
                            FUZZY_BOOST_LEXICAL,
                        )),
                    ));
                }
            }
        }
        if !facets.is_empty() {
            clauses.push((Occur::Must, self.facet_filter_query(facets)?));
        }
        Ok((Box::new(BooleanQuery::new(clauses)), truncated))
    }

    fn count_smart_fuzzy(
        &self,
        query: &str,
        facets: &[String],
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<CountResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (query, truncated) =
            self.build_smart_fuzzy_query(query, facets, max_distance, &voc, false)?;
        Ok(CountResult {
            count: self.run_count(query)?,
            truncated,
        })
    }

    fn search_and_count_smart_fuzzy(
        &self,
        query: &str,
        facets: &[String],
        limit: u32,
        offset: u32,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
        grouping: Option<ResultGrouping>,
    ) -> Result<SearchPageResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (search_query, truncated) =
            self.build_smart_fuzzy_query(query, facets, max_distance, &voc, true)?;
        self.run_search_and_count(
            search_query,
            |searcher| self.smart_fuzzy_highlight_plan(searcher, query, max_distance, &voc),
            self.search_text_field(&voc)?,
            limit,
            offset,
            &ResultsOrder::Relevance,
            &HighlightConfig::default(),
            truncated,
            grouping.as_ref(),
        )
    }

    /// Token groups of the query's quoted phrases of two or more words. A `"`
    /// between two Hebrew letters is gershayim (`רמב"ם`); any other delimits.
    fn quoted_phrase_token_groups(&self, query: &str) -> Result<Vec<Vec<String>>> {
        let chars: Vec<char> = query
            .chars()
            .map(|c| match c {
                '\u{05F4}' | '\u{201C}' | '\u{201D}' | '\u{201E}' => '"',
                c => c,
            })
            .collect();
        let is_letter = |i: usize| {
            chars
                .get(i)
                .is_some_and(|c| ('\u{05D0}'..='\u{05EA}').contains(c))
        };
        let mut groups = Vec::new();
        let mut open: Option<usize> = None;
        for (i, &c) in chars.iter().enumerate() {
            if c != '"' || (i > 0 && is_letter(i - 1) && is_letter(i + 1)) {
                continue;
            }
            match open.take() {
                None => open = Some(i + 1),
                Some(start) => {
                    let segment: String = chars[start..i].iter().collect();
                    let tokens = self.index_token_texts(&segment)?;
                    if tokens.len() >= 2 {
                        groups.push(tokens);
                    }
                }
            }
        }
        Ok(groups)
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
        let phrases = self.quoted_phrase_token_groups(query)?;
        self.build_fuzzy_search_query(&token_texts, &phrases, facets, max_distance, false)
    }

    /// Approximate (`fuzzy`) recall query. Routes through the lexical builder
    /// when a `MagicDictionary` is loaded, otherwise the plain fuzzy builder.
    /// This is the single decision point so every fuzzy entry point
    /// (`search_*`/`count_*`) shares identical matching logic. `rank` toggles
    /// the relevance-scoring layer: `true` only for `ResultsOrder::Relevance`
    /// searches, `false` for counts and catalogue ordering (which ignore score)
    /// so they build the bare recall query and pay nothing for unused ranking.
    ///
    /// `phrases` are the query's quoted word groups ([`Self::quoted_phrase_token_groups`]);
    /// only the lexical builder requires their adjacency.
    fn build_fuzzy_search_query(
        &self,
        term_texts: &[String],
        phrases: &[Vec<String>],
        facets: &[String],
        max_distance: u8,
        rank: bool,
    ) -> Result<Box<dyn Query>> {
        if self.magic_dict.is_some() {
            self.build_lexical_fuzzy_query(term_texts, phrases, facets, max_distance, rank)
        } else {
            self.build_fuzzy_query_from_terms(term_texts, facets, max_distance, rank)
        }
    }

    /// Lexical fuzzy mode: per token, `(FuzzyTermQuery OR TermSetQuery[lexical
    /// forms])` is required (`MUST`); the inner `SHOULD` group keeps both
    /// edit-distance matches and morphological relatives. Distance 0 drops the
    /// edit-distance automaton: the exact token, its quote-free spelling and
    /// its dictionary forms. Every word is required anywhere in the line;
    /// adjacency is required only for `phrases` (quoted groups) and otherwise
    /// only ranks. Facets filter as usual. Only the fuzzy path calls it.
    fn build_lexical_fuzzy_query(
        &self,
        term_texts: &[String],
        phrases: &[Vec<String>],
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
        let searcher = self.index_reader.searcher();

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(term_texts.len() + 3);
        for phrase in phrases.iter().filter(|p| p.len() >= 2) {
            clauses.push((
                Occur::Must,
                self.lexical_fuzzy_phrase_query(dict, phrase, max_distance)?,
            ));
            if rank {
                clauses.extend(
                    Self::exact_phrase(&searcher, text_f, phrase)?.map(|q| (Occur::Should, q)),
                );
            }
        }
        for token in term_texts {
            // A quoted word's recall and rank are its phrase's: per-word tiers
            // would score every phrase candidate again (~20% of the query).
            if phrases.iter().any(|p| p.len() >= 2 && p.contains(token)) {
                continue;
            }
            // Wrap the fuzzy automaton in the fuzzy-tier boost only when ranking;
            // an unranked recall query (count/catalogue) carries no boost so it
            // stays the bare fuzzy automaton it always was.
            let fuzzy: Option<Box<dyn Query>> = if max_distance > 0 {
                let fuzzy_q = AutomatonQuery::new(
                    text_f,
                    format!("{token}~{max_distance}"),
                    self.fuzzy_automaton(token, max_distance)?,
                );
                Some(if rank {
                    Box::new(BoostQuery::new(Box::new(fuzzy_q), FUZZY_BOOST_FUZZY))
                        as Box<dyn Query>
                } else {
                    Box::new(fuzzy_q)
                })
            } else {
                None
            };
            let clean = Self::quoteless_variant(token);
            let mut forms = dict.recall_forms(token, MAX_LEXICAL_FORMS);
            forms.retain(|f| f != token && Some(f.as_str()) != clean.as_deref());

            // Unranked: the original recall shape — `fuzzy OR termset`, or just
            // `fuzzy` when the dictionary has no extra forms (the exact term
            // stands in for `fuzzy` at distance 0). Ranked: prepend the exact
            // tier (a subset of the fuzzy match, so recall is unchanged) and
            // boost the lexical tier. `BooleanQuery` sums `Should` scores, so
            // exact-floor + BM25 > lexical > fuzzy.
            // A quote-bearing token also carries its quote-free spelling in
            // the exact tier — clean-typography editions match at distance 0
            // and rank as exact, not as edit-distance tail.
            let mut should: Vec<(Occur, Box<dyn Query>)> = if rank {
                Self::exact_rank_clauses(&searcher, text_f, token)?
            } else if fuzzy.is_none() {
                vec![(
                    Occur::Should,
                    quote_expansion::term_query(
                        &searcher,
                        text_f,
                        token,
                        IndexRecordOption::Basic,
                    )?,
                )]
            } else {
                Vec::with_capacity(3)
            };
            if let Some(clean) = &clean {
                if rank {
                    should.extend(Self::exact_rank_clauses(&searcher, text_f, clean)?);
                } else {
                    should.push((
                        Occur::Should,
                        quote_expansion::term_query(
                            &searcher,
                            text_f,
                            clean,
                            IndexRecordOption::Basic,
                        )?,
                    ));
                }
            }
            should.extend(fuzzy.map(|fuzzy| (Occur::Should, fuzzy)));
            if !forms.is_empty() {
                let set_terms = quote_expansion::spelling_terms(&searcher, text_f, &forms)?;
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
        if rank {
            clauses.extend(Self::exact_phrase_boost(&searcher, text_f, term_texts)?);
            if LEXICAL_FUZZY_EXPANDED_PHRASE_BOOST && term_texts.len() >= 2 {
                let expanded = self.lexical_fuzzy_phrase_query(dict, term_texts, max_distance)?;
                clauses.push((
                    Occur::Should,
                    Box::new(ConstScoreQuery::new(expanded, FUZZY_BOOST_LEXICAL)),
                ));
            }
        }
        if !facets.is_empty() {
            clauses.push((Occur::Must, self.facet_filter_query(facets)?));
        }
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    /// Adjacency (within [`LEXICAL_FUZZY_PHRASE_SLOP`]) of each word's exact,
    /// dictionary and edit-distance forms, in order. Positions are verified
    /// lazily per candidate; an eager regex phrase union cost 0.5-1 s.
    fn lexical_fuzzy_phrase_query(
        &self,
        dict: &MagicDictionary,
        term_texts: &[String],
        max_distance: u8,
    ) -> Result<Box<dyn Query>> {
        let text_f = self.schema.get_field("text")?;
        let searcher = self.index_reader.searcher();
        let position_terms = self
            .lexical_fuzzy_phrase_terms(dict, term_texts, max_distance)?
            .into_iter()
            .map(|terms| quote_expansion::spelling_terms(&searcher, text_f, &terms))
            .collect::<tantivy::Result<_>>()?;
        let gaps = vec![LEXICAL_FUZZY_PHRASE_SLOP; term_texts.len().saturating_sub(1)];
        Ok(Box::new(TermListPhraseQuery::new(
            text_f,
            position_terms,
            Proximity::Ordered(gaps),
        )))
    }

    fn lexical_fuzzy_phrase_terms(
        &self,
        dict: &MagicDictionary,
        term_texts: &[String],
        max_distance: u8,
    ) -> Result<Vec<Vec<String>>> {
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
                // The quote-free spelling rides along ahead of the budgeted
                // expansions, like in the single-token path.
                if let Some(clean) = Self::quoteless_variant(token) {
                    Self::push_limited_unique(
                        &mut terms,
                        &mut seen,
                        clean,
                        MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN,
                    );
                }
                for form in dict.recall_forms(token, MAX_LEXICAL_FORMS) {
                    Self::push_limited_unique(
                        &mut terms,
                        &mut seen,
                        form,
                        MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN,
                    );
                }

                let remaining = MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN.saturating_sub(terms.len());
                if remaining > 0 && max_distance > 0 {
                    let automaton = self.fuzzy_automaton(token, max_distance)?;
                    for fuzzy_term in self.automaton_terms(&searcher, &automaton, remaining)? {
                        Self::push_limited_unique(
                            &mut terms,
                            &mut seen,
                            fuzzy_term,
                            MAX_LEXICAL_PHRASE_TERMS_PER_TOKEN,
                        );
                    }
                }

                Ok(terms)
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

    /// Advanced mode: ports the Dart morphological query builder to produce regex
    /// terms + slop + max_expansions, then reuses `build_query`. Also returns the
    /// regex patterns so callers can materialize concrete terms for highlighting.
    /// `any_order` applies as [`hebrew_query::resolve_proximity`] says; acronym
    /// expansions stay ordered.
    #[allow(clippy::too_many_arguments)]
    fn build_advanced_query(
        &self,
        query: &str,
        distance: u32,
        custom_spacing: &HashMap<String, String>,
        any_order: bool,
        alternative_words: &HashMap<u32, Vec<String>>,
        search_options: &HashMap<String, HashMap<String, bool>>,
        facets: Vec<String>,
        voc: &VocalizedFlags,
        scope: &SearchScope,
        match_mode: &WordMatch,
        cross_line: bool,
    ) -> Result<AdvancedQueryBuild> {
        self.build_advanced_query_ranked(
            query,
            distance,
            custom_spacing,
            any_order,
            alternative_words,
            search_options,
            facets,
            voc,
            scope,
            match_mode,
            cross_line,
            false,
        )
    }

    /// [`Self::build_advanced_query`]; `rank` (a relevance-ordered search)
    /// scores a single word's matches in tiers — see [`ADVANCED_TIER_BOOSTS`].
    #[allow(clippy::too_many_arguments)]
    fn build_advanced_query_ranked(
        &self,
        query: &str,
        distance: u32,
        custom_spacing: &HashMap<String, String>,
        any_order: bool,
        alternative_words: &HashMap<u32, Vec<String>>,
        search_options: &HashMap<String, HashMap<String, bool>>,
        facets: Vec<String>,
        voc: &VocalizedFlags,
        scope: &SearchScope,
        match_mode: &WordMatch,
        cross_line: bool,
        rank: bool,
    ) -> Result<AdvancedQueryBuild> {
        // The vocalized mode is requested either by the global API flags or
        // by a per-word "ניקוד"/"טעמים" option; the per-word requirement
        // derivation happens inside `prepare_advanced_query_vocalized`, which
        // receives the GLOBAL flags only (folding options into them would
        // bind every word's typed marks).
        let voc_mode = voc.or(hebrew_query::options_vocalized_flags(search_options));
        // "תרגום ארמי": מילה שסומנה לה האפשרות מקבלת את תרגומיה מהמילון
        // כמילים-חלופיות — ומשם הן זורמות בכל המסלולים הקיימים (ענפי
        // תבנית, המסלול המנוקד, איסוף טרמים להדגשה).
        let translated =
            self.translation_alternatives(query, alternative_words, search_options, &voc_mode);
        let alternative_words = translated.as_ref().unwrap_or(alternative_words);
        let mut prepared = if voc_mode.any() {
            hebrew_query::prepare_advanced_query_vocalized(
                query,
                distance,
                custom_spacing,
                alternative_words,
                search_options,
                voc,
            )
        } else {
            hebrew_query::prepare_advanced_query(
                query,
                distance,
                custom_spacing,
                alternative_words,
                search_options,
            )
        };
        let text_field = self.search_text_field(&voc_mode)?;
        // Vocalized typo tokens cannot ride the in-collection Levenshtein
        // scan (it would run on the vocalized dictionary, counting every mark
        // as an edit): expand them against the PLAIN dictionary here and
        // append the variants as lowest-priority free-mark branches.
        if voc_mode.any() && !prepared.typo_tokens.is_empty() {
            let searcher = self.index_reader.searcher();
            let mut seen: HashSet<String> = prepared.typo_tokens.iter().cloned().collect();
            let extra =
                self.vocalized_variant_branches(&searcher, &prepared.typo_tokens, 1, &mut seen)?;
            prepared.typo_tokens = Vec::new();
            if let Some(first) = prepared.regex_terms.pop() {
                prepared.regex_terms.push(first.with_extra_branches(extra));
            }
        }
        // `gaps` already folds `custom_spacing` in (per-pair values, else
        // `distance` for every pair) — the phrase filter's gap allowances
        // must use it, not the raw `distance`, or a spacing-permitted match
        // would be rejected and fall back to the broad term highlight.
        let proximity = hebrew_query::resolve_proximity(
            std::mem::take(&mut prepared.gaps),
            distance,
            custom_spacing,
            any_order,
        );
        let source_words = std::mem::take(&mut prepared.words);
        // The highlight builders want one pattern string per word; the query
        // builder gets the structured patterns so a single word compiles per
        // branch instead of as one state-limited DFA.
        let regex_terms: Vec<String> = prepared
            .regex_terms
            .iter()
            .map(hebrew_query::WordPattern::joined)
            .collect();
        // "ראשי תיבות": תת-שאילתות פענוח ר"ת שיש ל-OR עם השאילתה הראשית.
        // נבנות עכשיו — לפני ש-`query` (מחרוזת) מוצללת ע"י תוצאת בניית
        // השאילתה — ובאותם facets/scope כדי שה-OR יישאר מפולטר נכון.
        let acronym_alts = self.acronym_alternatives(
            query,
            search_options,
            &voc_mode,
            facets.clone(),
            prepared.max_expansions,
            text_field,
            scope,
            cross_line,
        )?;
        // Tiers compare mark-free text, so the vocalized field stays unranked.
        let ranking_roots: Option<Vec<String>> = (rank && !voc_mode.any())
            .then(|| {
                let word = source_words.first()?;
                let ignore_quotes =
                    hebrew_query::word_flags_at(&source_words, 0, search_options).ignore_quotes;
                let roots = std::iter::once(word.clone())
                    .chain(
                        alternative_words
                            .get(&0)
                            .into_iter()
                            .flatten()
                            .map(|a| hebrew_query::normalize_for_index(a)),
                    )
                    .map(|r| {
                        if ignore_quotes {
                            hebrew_query::strip_quote_chars(&r)
                        } else {
                            r
                        }
                    })
                    .collect();
                Some(roots)
            })
            .flatten();
        let (main_query, main_truncated) = self.build_query_from_patterns(
            prepared.regex_terms,
            &source_words,
            &prepared.typo_tokens,
            facets,
            &proximity,
            prepared.max_expansions,
            text_field,
            scope,
            match_mode,
            cross_line,
            ranking_roots.as_deref(),
        )?;
        let (query, truncated, acronym_patterns) = if acronym_alts.is_empty() {
            (main_query, main_truncated, Vec::new())
        } else {
            let mut truncated = main_truncated;
            let mut clauses: Vec<(Occur, Box<dyn Query>)> =
                Vec::with_capacity(acronym_alts.len() + 1);
            let mut alt_patterns = Vec::with_capacity(acronym_alts.len());
            clauses.push((Occur::Should, main_query));
            for (alt_query, alt_truncated, patterns) in acronym_alts {
                clauses.push((Occur::Should, alt_query));
                truncated |= alt_truncated;
                alt_patterns.push(patterns);
            }
            (
                Box::new(BooleanQuery::new(clauses)) as Box<dyn Query>,
                truncated,
                alt_patterns,
            )
        };
        Ok((query, regex_terms, proximity, truncated, acronym_patterns))
    }

    /// בונה תת-שאילתות פענוח ראשי-תיבות (דו-כיווני) שיש ל-OR עם השאילתה
    /// הראשית, כשאפשרות "ראשי תיבות" ([`hebrew_query::OPT_ACRONYM`]) דלוקה
    /// על מילה כלשהי. מחזיר וקטור ריק כשאין מילון, אין אפשרות מסומנת, או
    /// אין התאמה.
    ///
    /// הפענוח **רב-מילי**, ולכן אינו יכול לרכוב על ערוץ `alternative_words`
    /// (החד-מילתי) כמו התרגום — כל חלופה נבנית כשאילתה שלמה ומצטרפת כ-OR.
    /// הכיסוי מוגבל ל**שאילתה שהיא יחידה סמנטית אחת**: ר"ת בודד (כיוון
    /// ר"ת→פענוח) או ביטוי שכולו פענוח ידוע (כיוון פענוח→ר"ת). ר"ת המשובץ
    /// בתוך שאילתה ארוכה יותר, ומצב מנוקד, אינם נתמכים בשלב זה.
    ///
    /// כל פריט מוחזר כ-(שאילתה, truncated, תבניות-ליטרל פר-מילה) — התבניות
    /// מוזנות לבוני ההדגשה כדי שמסמך שנמצא דרך החלופה ייצבע.
    #[allow(clippy::too_many_arguments)]
    fn acronym_alternatives(
        &self,
        query: &str,
        search_options: &HashMap<String, HashMap<String, bool>>,
        voc: &VocalizedFlags,
        facets: Vec<String>,
        max_expansions: u32,
        text_field: Field,
        scope: &SearchScope,
        cross_line: bool,
    ) -> Result<Vec<AcronymAlternative>> {
        let Some(dict) = self.acronym_dict.as_ref() else {
            return Ok(Vec::new());
        };
        if search_options.is_empty() {
            return Ok(Vec::new());
        }
        // פענוח בשילוב חיפוש מנוקד אינו נתמך עדיין: החלופות ליטרלים
        // נטולי-סימנים ולא יתאימו למילון הטרמים המנוקד.
        if voc.any() {
            return Ok(Vec::new());
        }
        // אותה נורמליזציה/טוקניזציה שממנה נגזרים מפתחות האפשרויות ומפתחות
        // המילון — כדי שהכול יתלכד.
        let words = hebrew_query::split_query_words(&hebrew_query::normalize_for_index(query));
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let enabled = words.iter().enumerate().any(|(i, word)| {
            search_options
                .get(&format!("{word}_{i}"))
                .and_then(|opts| opts.get(hebrew_query::OPT_ACRONYM))
                .copied()
                .unwrap_or(false)
        });
        if !enabled {
            return Ok(Vec::new());
        }

        // אוסף החלופות כרשימות-מילים בצורת טרם-אינדקס.
        let mut alternatives: Vec<Vec<String>> = Vec::new();
        // כיוון א' (ר"ת→פענוח): רק כשהשאילתה כולה ר"ת בודד.
        if words.len() == 1 {
            alternatives.extend(dict.expand(&words[0], MAX_ACRONYM_EXPANSIONS));
        }
        // כיוון ב' (פענוח→ר"ת): כשכל השאילתה היא פענוח ידוע.
        for acronym in dict.acronyms_for(&words, MAX_ACRONYM_EXPANSIONS) {
            alternatives.push(vec![acronym]);
        }
        if alternatives.is_empty() {
            return Ok(Vec::new());
        }

        let mut out = Vec::with_capacity(alternatives.len());
        for alt in alternatives {
            let literal_patterns: Vec<String> =
                alt.iter().map(|w| hebrew_query::escape_regex(w)).collect();
            let patterns: Vec<hebrew_query::WordPattern> = literal_patterns
                .iter()
                .map(|p| hebrew_query::WordPattern::Literal(p.clone()))
                .collect();
            // פענוח הוא ביטוי קנוני — מילים צמודות (slop 0, ללא GapVerified),
            // וכל מילותיו חובה גם כשהשאילתה הראשית בהתאמה חלקית.
            let proximity = Proximity::adjacent(patterns.len());
            let (alt_query, truncated) = self.build_query_from_patterns(
                patterns,
                &[],
                &[],
                facets.clone(),
                &proximity,
                max_expansions,
                text_field,
                scope,
                &WordMatch::All,
                cross_line,
                None,
            )?;
            out.push((alt_query, truncated, literal_patterns));
        }
        Ok(out)
    }

    /// בונה מפת מילים-חלופיות מורחבת בתרגומי המילון עבור מילים שסומנה
    /// להן אפשרות "תרגום ארמי". מחזיר `None` כשאין מה להרחיב (אין מילון,
    /// אין אפשרות מסומנת, או אין תרגומים) — והשאילתה ממשיכה עם המפה
    /// המקורית ללא העתקה.
    fn translation_alternatives(
        &self,
        query: &str,
        alternative_words: &HashMap<u32, Vec<String>>,
        search_options: &HashMap<String, HashMap<String, bool>>,
        voc: &VocalizedFlags,
    ) -> Option<HashMap<u32, Vec<String>>> {
        let dict = self.translation_dict.as_ref()?;
        if search_options.is_empty() {
            return None;
        }
        // אותה נורמליזציה וטוקניזציה שממנה נגזרים מפתחות האפשרויות
        // ("{word}_{index}") בהכנת השאילתה — כדי שהמפתחות יתלכדו.
        let normalized = if voc.any() {
            hebrew_query::normalize_for_index_vocalized(query)
        } else {
            hebrew_query::normalize_for_index(query)
        };
        let words = hebrew_query::split_query_words(&normalized);
        let mut augmented: Option<HashMap<u32, Vec<String>>> = None;
        for (i, word) in words.iter().enumerate() {
            let enabled = search_options
                .get(&format!("{word}_{i}"))
                .and_then(|opts| opts.get(hebrew_query::OPT_TRANSLATION))
                .copied()
                .unwrap_or(false);
            if !enabled {
                continue;
            }
            // המילון ממופתח בצורת טרם נטולת-סימנים; במצב מנוקד המילה עוד
            // נושאת את סימניה.
            let base = hebrew_query::strip_attached_marks(word);
            let expansions = dict.expansions(&base, MAX_TRANSLATION_EXPANSIONS);
            if expansions.is_empty() {
                continue;
            }
            augmented
                .get_or_insert_with(|| alternative_words.clone())
                .entry(i as u32)
                .or_default()
                .extend(expansions);
        }
        augmented
    }

    // ── Shared query executors (take a prebuilt query) ───────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn run_search<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        text_field: Field,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
        grouping: Option<&ResultGrouping>,
    ) -> Result<Vec<SearchResult>>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        let query = crate::lexical_cancellation::LexicalFlag::current().wrap(query);
        crate::lexical_cancellation::check_current()?;
        let searcher = self.index_reader.searcher();
        if let Some(grouping) = grouping {
            let page = self.collect_grouped(&searcher, &*query, grouping, limit, offset, order)?;
            return Self::build_grouped_results(
                &self.schema,
                &searcher,
                query.as_ref(),
                make_highlight,
                text_field,
                hl,
                &page,
            );
        }
        let addresses = self
            .ranked_page(&searcher, &*query, limit, offset, order, PageCounts::None)?
            .addresses;
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
        crate::lexical_cancellation::check_current()?;
        let plan = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = plan.query.as_deref().unwrap_or(query.as_ref());
        Self::build_results(
            &self.schema,
            &searcher,
            hl_q,
            text_field,
            addresses,
            hl,
            plan.phrase.as_ref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_search_and_count<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        text_field: Field,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
        truncated: bool,
        grouping: Option<&ResultGrouping>,
    ) -> Result<SearchPageResult>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        let query = crate::lexical_cancellation::LexicalFlag::current().wrap(query);
        crate::lexical_cancellation::check_current()?;
        let searcher = self.index_reader.searcher();
        if let Some(grouping) = grouping {
            let page = self.collect_grouped(&searcher, &*query, grouping, limit, offset, order)?;
            let results = Self::build_grouped_results(
                &self.schema,
                &searcher,
                query.as_ref(),
                make_highlight,
                text_field,
                hl,
                &page,
            )?;
            return Ok(SearchPageResult {
                total_count: page.raw_total,
                results,
                truncated: truncated || page.truncated,
                group_count: Some(page.group_count),
            });
        }
        if limit == 0 {
            return Ok(SearchPageResult {
                total_count: self.count_on(&searcher, &*query)?,
                results: Vec::new(),
                truncated,
                group_count: None,
            });
        }
        // One index pass for both count and top-docs.
        let RankedPage {
            addresses,
            total: total_count,
            ..
        } = self.ranked_page(&searcher, &*query, limit, offset, order, PageCounts::Total)?;
        // total_count is the full hit count regardless of this page; only the
        // snippet highlighting (and its dictionary scan) is page-dependent, so
        // skip it when this page is empty (e.g. offset past the last hit).
        if addresses.is_empty() {
            return Ok(SearchPageResult {
                total_count,
                results: Vec::new(),
                truncated,
                group_count: None,
            });
        }
        crate::lexical_cancellation::check_current()?;
        let plan = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = plan.query.as_deref().unwrap_or(query.as_ref());
        let results = Self::build_results(
            &self.schema,
            &searcher,
            hl_q,
            text_field,
            addresses,
            hl,
            plan.phrase.as_ref(),
        )?;
        Ok(SearchPageResult {
            total_count,
            results,
            truncated,
            group_count: None,
        })
    }

    fn run_count(&self, query: Box<dyn Query>) -> Result<u32> {
        self.count_on(&self.index_reader.searcher(), &*query)
    }

    fn run_count_by_book(&self, query: Box<dyn Query>) -> Result<HashMap<String, u32>> {
        let searcher = self.index_reader.searcher();
        let key = PageKey::new(
            page_cache::snapshot_of(&searcher),
            self.order_epoch(),
            &*query,
        )
        .map(|key| key.with_epoch(self.page_cache.epoch()));
        if let Some(counts) = key
            .as_ref()
            .and_then(|key| self.page_cache.book_counts(key))
        {
            return Ok((*counts).clone());
        }
        let counts = searcher.search(&*query, &BookCountCollector)?;
        if let Some(key) = key {
            self.page_cache
                .store_book_counts(key, Arc::new(counts.clone()));
        }
        Ok(counts)
    }

    /// The hit count of `query`, from the page cache when a search counted it.
    fn count_on(&self, searcher: &Searcher, query: &dyn Query) -> Result<u32> {
        let key = PageKey::new(page_cache::snapshot_of(searcher), self.order_epoch(), query)
            .map(|key| key.with_epoch(self.page_cache.epoch()));
        if let Some(total) = key.as_ref().and_then(|key| self.page_cache.total(key)) {
            return Ok(total);
        }
        let total = searcher.search(query, &Count)? as u32;
        if let Some(key) = key {
            self.page_cache.store_total(key, total);
        }
        Ok(total)
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

    /// Obtain once after opening the engine. Its synchronous `cancel` bypasses
    /// the engine lock and worker queue, even during indexing or a busy search.
    pub fn lexical_cancellation_handle(&self) -> LexicalCancellationHandle {
        LexicalCancellationHandle {
            registry: Arc::clone(&self.lexical_cancellations),
        }
    }

    /// Stops every lexical call tagged with `request_id` (the streams, counts and
    /// per-book counts that take `cancel_request_id`), running or still to come.
    /// A cancelled count fails with [`LexicalSearchCancelled::MESSAGE`]; a
    /// cancelled stream closes without another event. Await the returned future
    /// before starting a call that must observe the cancel.
    ///
    /// Prefer [`Self::lexical_cancellation_handle`] for responsive cancellation:
    /// this compatibility entry point waits for the engine lock and worker queue.
    pub fn cancel_lexical_search(&self, request_id: u32) {
        self.lexical_cancellations.cancel(request_id);
    }

    /// Routes a stream-search failure into the stream itself, where the Dart
    /// side receives it as an `onError` event.
    ///
    /// Returning `Err` from a `StreamSink`-taking function does NOT reach the
    /// app: the generated Dart wrapper fires the call `unawaited` and returns
    /// `sink.stream` immediately, so the error becomes an unhandled async
    /// error while dropping the Rust sink just closes the stream — the user
    /// sees 0 results and no failure. This was the silent-failure half of the
    /// state-limit bug, and relaxing budgets makes `max_expansions` overflow
    /// (which must stay an error) more reachable, so it has to be visible.
    #[cfg(feature = "flutter-bridge")]
    fn surface_stream_error<T: crate::frb_generated::SseEncode>(
        sink: &StreamSink<T>,
        result: Result<()>,
    ) -> Result<()> {
        if let Err(err) = result {
            if LexicalSearchCancelled::is(&err) {
                debug!("stream search cancelled");
                return Ok(());
            }
            error!("stream search failed: {err:#}");
            let _ = sink.add_error(err);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "flutter-bridge")]
    fn run_search_stream<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        text_field: Field,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
        chunk_size: u32,
        grouping: Option<&ResultGrouping>,
        request: &LexicalRequest<'_>,
        sink: &StreamSink<Vec<SearchResult>>,
    ) -> Result<()>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        let searcher = self.index_reader.searcher();
        let query = request.wrap(query);
        let chunk_size = (chunk_size.max(1)) as usize;
        if let Some(grouping) = grouping {
            let page = self.collect_grouped(&searcher, &*query, grouping, limit, offset, order)?;
            request.check()?;
            if page.reps.is_empty() {
                return Ok(());
            }
            let plan = Self::resolve_highlight(&searcher, make_highlight);
            let hl_q = plan.query.as_deref().unwrap_or(query.as_ref());
            let generator = Self::make_snippet_generator(&searcher, hl_q, text_field, hl)?;
            for reps in result_chunks(&page.reps, chunk_size) {
                request.check()?;
                let chunk_page = GroupedPage {
                    reps: reps.to_vec(),
                    group_count: page.group_count,
                    raw_total: page.raw_total,
                    truncated: page.truncated,
                };
                let results = Self::build_grouped_results_with_generator(
                    &self.schema,
                    &searcher,
                    &generator,
                    text_field,
                    hl,
                    &chunk_page,
                    plan.phrase.as_ref(),
                )?;
                request.check()?;
                if sink.add(results).is_err() {
                    break;
                }
            }
            return Ok(());
        }
        let addresses = self
            .ranked_page(&searcher, &*query, limit, offset, order, PageCounts::None)?
            .addresses;
        if addresses.is_empty() {
            return Ok(());
        }
        crate::lexical_cancellation::check_current()?;
        let plan = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = plan.query.as_deref().unwrap_or(query.as_ref());
        let phrase = plan.phrase.as_ref();
        // One generator for the whole stream: creating it resolves term
        // doc-frequencies, which is too expensive to repeat per chunk.
        let snippet_generator = Self::make_snippet_generator(&searcher, hl_q, text_field, hl)?;
        for chunk in result_chunks(&addresses, chunk_size) {
            let results = Self::build_results_with_generator(
                &self.schema,
                &searcher,
                &snippet_generator,
                text_field,
                chunk.to_vec(),
                hl,
                phrase,
            )?;
            // If the Dart side cancelled the stream, stop early.
            request.check()?;
            if sink.add(results).is_err() {
                break;
            }
        }
        Ok(())
    }

    /// Combined-stream executor: ONE `searcher.search` pass evaluates the
    /// query with a tuple collector — ranked page + total count + per-book
    /// counts — then streams snippet chunks like [`Self::run_search_stream`].
    /// The counts go out as the first event so the UI can show totals and the
    /// facet tree before the first snippet chunk is even built.
    #[allow(clippy::too_many_arguments)]
    #[cfg(any(feature = "flutter-bridge", test))]
    fn run_search_stream_with_counts<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        text_field: Field,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
        chunk_size: u32,
        truncated: bool,
        grouping: Option<&ResultGrouping>,
        request: &LexicalRequest<'_>,
        emit: &mut dyn FnMut(SearchStreamUpdate) -> bool,
    ) -> Result<()>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        let mut sink = CallbackSink(emit);
        let searcher = self.index_reader.searcher();
        let query = request.wrap(query);
        let chunk_size = (chunk_size.max(1)) as usize;

        if let Some(grouping) = grouping {
            let key = self.group_cache_key(&searcher, &*query, grouping, order);
            let cached = key.as_ref().and_then(|key| {
                Some((
                    self.grouped_cache.get(key)?,
                    self.page_cache.book_counts(&key.0)?,
                ))
            });
            let (page, book_counts) = match cached {
                Some((page, counts)) => (page.slice(limit, offset), (*counts).clone()),
                None => {
                    let sort = self.group_sort(order)?;
                    let collectors = (GroupCollector::new(grouping, sort), BookCountCollector);
                    let ((), book_counts) = searcher.search(&*query, &collectors)?;
                    request.check()?;
                    let full = Arc::new(Self::finalize_grouped(
                        collectors.0.take_hits(),
                        u32::MAX,
                        0,
                    ));
                    if let Some(key) = key {
                        self.page_cache
                            .store_book_counts(key.0, Arc::new(book_counts.clone()));
                        self.grouped_cache.put(key, Arc::clone(&full));
                    }
                    (full.slice(limit, offset), book_counts)
                }
            };
            request.check()?;
            if sink
                .add(SearchStreamUpdate {
                    total_count: Some(page.raw_total),
                    book_counts: Some(book_counts),
                    results: Vec::new(),
                    truncated: truncated || page.truncated,
                    group_count: Some(page.group_count),
                })
                .is_err()
            {
                return Ok(());
            }
            request.check()?;
            if page.reps.is_empty() {
                return Ok(());
            }
            let plan = Self::resolve_highlight(&searcher, make_highlight);
            let hl_q = plan.query.as_deref().unwrap_or(query.as_ref());
            let generator = Self::make_snippet_generator(&searcher, hl_q, text_field, hl)?;
            for reps in result_chunks(&page.reps, chunk_size) {
                request.check()?;
                let chunk_page = GroupedPage {
                    reps: reps.to_vec(),
                    group_count: page.group_count,
                    raw_total: page.raw_total,
                    truncated: page.truncated,
                };
                let results = Self::build_grouped_results_with_generator(
                    &self.schema,
                    &searcher,
                    &generator,
                    text_field,
                    hl,
                    &chunk_page,
                    plan.phrase.as_ref(),
                )?;
                request.check()?;
                if sink
                    .add(SearchStreamUpdate {
                        total_count: None,
                        book_counts: None,
                        results,
                        truncated: false,
                        group_count: None,
                    })
                    .is_err()
                {
                    break;
                }
            }
            return Ok(());
        }

        let RankedPage {
            addresses,
            total: total_count,
            book_counts,
        } = self.ranked_page(
            &searcher,
            &*query,
            limit,
            offset,
            order,
            PageCounts::TotalAndBooks,
        )?;

        // Counts first — the page addresses may be empty (offset past the
        // end) while the totals are still meaningful.
        request.check()?;
        if sink
            .add(SearchStreamUpdate {
                total_count: Some(total_count),
                book_counts,
                results: Vec::new(),
                truncated,
                group_count: None,
            })
            .is_err()
        {
            return Ok(());
        }
        if addresses.is_empty() {
            return Ok(());
        }

        crate::lexical_cancellation::check_current()?;
        let plan = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = plan.query.as_deref().unwrap_or(query.as_ref());
        let phrase = plan.phrase.as_ref();
        // One generator for the whole stream: creating it resolves term
        // doc-frequencies, which is too expensive to repeat per chunk.
        let snippet_generator = Self::make_snippet_generator(&searcher, hl_q, text_field, hl)?;
        for chunk in result_chunks(&addresses, chunk_size) {
            let results = Self::build_results_with_generator(
                &self.schema,
                &searcher,
                &snippet_generator,
                text_field,
                chunk.to_vec(),
                hl,
                phrase,
            )?;
            // If the Dart side cancelled the stream, stop early.
            request.check()?;
            if sink
                .add(SearchStreamUpdate {
                    total_count: None,
                    book_counts: None,
                    results,
                    truncated: false,
                    group_count: None,
                })
                .is_err()
            {
                break;
            }
        }
        Ok(())
    }

    /// Invokes a highlight-query builder against the search's `searcher`,
    /// degrading a build failure to an empty plan (no highlight query, no
    /// phrase filter) instead of failing the whole search. An empty plan makes
    /// the caller fall back to the main query, which already exposes its terms
    /// when it is a Term/Phrase/TermSet query.
    fn resolve_highlight<F>(searcher: &Searcher, make_highlight: F) -> HighlightPlan
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        make_highlight(searcher).unwrap_or_else(|_| HighlightPlan::none())
    }

    /// The hits `offset..offset + limit` of `query` in `order`, with the counts
    /// `counts` asks for, in one index pass. Ranks a few pages ahead into the
    /// page cache, and serves from it when an earlier call ranked this far.
    fn ranked_page(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        counts: PageCounts,
    ) -> Result<RankedPage> {
        let slot = match order {
            ResultsOrder::Catalogue => 0,
            ResultsOrder::Generation => 1,
            ResultsOrder::Relevance => 2,
        };
        let (epoch, sort) = self.order_sort(order)?;
        let key = PageKey::new(page_cache::snapshot_of(searcher), epoch, query)
            .map(|key| key.with_epoch(self.page_cache.epoch()));
        if let Some(page) = key
            .as_ref()
            .and_then(|key| self.page_cache.page(key, slot, offset, limit))
        {
            if (counts == PageCounts::None || page.total.is_some())
                && (counts != PageCounts::TotalAndBooks || page.book_counts.is_some())
            {
                return Ok(RankedPage {
                    addresses: page.addresses,
                    total: page.total.unwrap_or(0),
                    book_counts: page.book_counts.map(|counts| (*counts).clone()),
                });
            }
        }
        let window = key.and(page_cache::ranked_window(offset, limit));
        let (from, take) = match window {
            Some(window) => (0, window),
            None => (offset as usize, limit as usize),
        };
        let top = TopDocs::with_limit(take).and_offset(from);
        let (mut addresses, total, book_counts) = match sort {
            Some(sort) => {
                collect_ranked(searcher, query, top.order_by((sort, Order::Asc)), counts)?
            }
            None => collect_ranked(searcher, query, top.order_by_score(), counts)?,
        };
        if let (Some(key), Some(window)) = (key, window) {
            if let Some(total) = total {
                self.page_cache.store_total(key, total);
            }
            if let Some(counts) = &book_counts {
                self.page_cache
                    .store_book_counts(key, Arc::new(counts.clone()));
            }
            let start = (offset as usize).min(addresses.len());
            let end = (offset as usize + limit as usize).min(addresses.len());
            let page = addresses[start..end].to_vec();
            self.page_cache.store_ranked(key, slot, addresses, window);
            addresses = page;
        }
        Ok(RankedPage {
            addresses,
            total: total.unwrap_or(0),
            book_counts,
        })
    }

    fn group_sort(&self, order: &ResultsOrder) -> Result<GroupSort> {
        Ok(match self.order_sort(order)?.1 {
            Some(sort) => GroupSort::Book(sort.tables, sort.kind),
            None => GroupSort::ScoreDesc(self.order_tables()?.1),
        })
    }

    /// The epoch of the order mapping `order` ranks by, and the sort it ranks with (`None`:
    /// by score).
    fn order_sort(&self, order: &ResultsOrder) -> Result<(u64, Option<BookOrderSort>)> {
        let kind = match order {
            ResultsOrder::Catalogue => OrderKind::Catalogue,
            ResultsOrder::Generation => OrderKind::Generation,
            ResultsOrder::Relevance => return Ok((self.order_epoch(), None)),
        };
        let (epoch, tables) = self.order_tables()?;
        Ok((epoch, Some(BookOrderSort { tables, kind })))
    }

    /// מריץ את שאילתת החיפוש עם [`GroupCollector`] וגוזר את עמוד הקבוצות
    /// המבוקש (`limit`/`offset` בקבוצות). CPU: מעבר אינדקס מלא אחד — אותה
    /// מחלקת עלות כמו ספירת-כל/ספירה-פר-ספר שהמסך ממילא מריץ. זיכרון:
    /// O(קבוצות), קצוץ ב-[`GROUP_COLLECTOR_MAX_GROUPS`] — בניגוד לספירות,
    /// שמחזיקות מונה-לספר בלבד.
    fn collect_grouped(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        grouping: &ResultGrouping,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
    ) -> Result<GroupedPage> {
        let key = self.group_cache_key(searcher, query, grouping, order);
        if let Some(page) = key.as_ref().and_then(|key| self.grouped_cache.get(key)) {
            return Ok(page.slice(limit, offset));
        }
        let collector = GroupCollector::new(grouping, self.group_sort(order)?);
        searcher.search(query, &collector)?;
        crate::lexical_cancellation::check_current()?;
        let page = Arc::new(Self::finalize_grouped(collector.take_hits(), u32::MAX, 0));
        if let Some(key) = key {
            self.grouped_cache.put(key, Arc::clone(&page));
        }
        Ok(page.slice(limit, offset))
    }

    fn group_cache_key(
        &self,
        searcher: &Searcher,
        query: &dyn Query,
        grouping: &ResultGrouping,
        order: &ResultsOrder,
    ) -> Option<GroupCacheKey> {
        self.refresh_book_order();
        let key = PageKey::new(page_cache::snapshot_of(searcher), self.order_epoch(), query)
            .map(|key| key.with_epoch(self.page_cache.epoch()))?;
        let grouping = match grouping {
            ResultGrouping::SameSection => 0,
            ResultGrouping::IdenticalText => 1,
        };
        let order = match order {
            ResultsOrder::Catalogue => 0,
            ResultsOrder::Generation => 1,
            ResultsOrder::Relevance => 2,
        };
        Some((
            key,
            grouping,
            order,
            self.grouped_cache.1.load(Ordering::Acquire),
        ))
    }

    /// ממיין את הקבוצות לפי הנציג הטוב ביותר וגוזר את העמוד המבוקש.
    fn finalize_grouped(hits: GroupedHits, limit: u32, offset: u32) -> GroupedPage {
        let group_count = hits.groups.len() as u32;
        let mut groups: Vec<GroupAcc> = hits.groups.into_values().collect();
        groups.sort_by_key(|g| (g.best.0, g.best.1));
        let reps: Vec<GroupedRep> = groups
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|g| GroupedRep {
                id: g.best.3,
                address: g.best.2,
                count: g.count,
                siblings: g.siblings.iter().map(|s| s.2).collect(),
            })
            .collect();
        GroupedPage {
            reps,
            group_count,
            raw_total: hits.raw_total,
            truncated: hits.truncated,
        }
    }

    /// בונה את תוצאות העמוד המקובץ: snippet מלא לנציגים (כמו במסלול
    /// השטוח), ולכל נציג — מונה הקבוצה וחברותיה (שליפת doc-store קלה,
    /// בלי snippet). התאמת נציג→קבוצה נעשית לפי `id` (build_results עשוי
    /// לדלג על מסמך שאינו ניתן לקריאה).
    #[allow(clippy::too_many_arguments)]
    fn build_grouped_results<F>(
        schema: &Schema,
        searcher: &Searcher,
        query: &dyn Query,
        make_highlight: F,
        text_field: Field,
        hl: &HighlightConfig,
        page: &GroupedPage,
    ) -> Result<Vec<SearchResult>>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        if page.reps.is_empty() {
            return Ok(Vec::new());
        }
        crate::lexical_cancellation::check_current()?;
        let plan = Self::resolve_highlight(searcher, make_highlight);
        let hl_q: &dyn Query = plan.query.as_deref().unwrap_or(query);
        let generator = Self::make_snippet_generator(searcher, hl_q, text_field, hl)?;
        Self::build_grouped_results_with_generator(
            schema,
            searcher,
            &generator,
            text_field,
            hl,
            page,
            plan.phrase.as_ref(),
        )
    }

    fn build_grouped_results_with_generator(
        schema: &Schema,
        searcher: &Searcher,
        generator: &SnippetGenerator,
        text_field: Field,
        hl: &HighlightConfig,
        page: &GroupedPage,
        phrase: Option<&PhraseHighlight>,
    ) -> Result<Vec<SearchResult>> {
        let addresses = page.reps.iter().map(|r| r.address).collect();
        let mut results = Self::build_results_with_generator(
            schema, searcher, generator, text_field, addresses, hl, phrase,
        )?;
        let rep_by_id: HashMap<u64, &GroupedRep> = page.reps.iter().map(|r| (r.id, r)).collect();
        let fields = SiblingFields::resolve(schema)?;
        let mut loader = DocumentLoader::new(searcher);
        for result in &mut results {
            crate::lexical_cancellation::check_current()?;
            let Some(rep) = rep_by_id.get(&result.id) else {
                continue;
            };
            result.merged_count = rep.count;
            result.merged = rep
                .siblings
                .iter()
                .filter_map(|addr| {
                    Self::fetch_merged_sibling(&fields, &mut loader, *addr).transpose()
                })
                .collect::<Result<Vec<_>>>()?;
        }
        Ok(results)
    }

    /// שליפת doc-store קלה לחברת קבוצה — מיקום בלבד, בלי snippet.
    /// מסמך שאינו ניתן לקריאה נשמט בשקט מהרשימה (המונה כבר ספר אותו).
    fn fetch_merged_sibling(
        fields: &SiblingFields,
        loader: &mut DocumentLoader<'_>,
        address: DocAddress,
    ) -> Result<Option<MergedSibling>> {
        let doc = match loader.load(address) {
            Ok(d) => d,
            Err(e) => {
                log::error!(
                    "dropping merged sibling: doc store read failed at segment {} doc {}: {e}",
                    address.segment_ord,
                    address.doc_id
                );
                return Ok(None);
            }
        };
        Ok(Some(MergedSibling {
            title: doc
                .get_first(fields.title)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            reference: doc
                .get_first(fields.reference)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            id: doc
                .get_first(fields.id)
                .and_then(|v| v.as_u64())
                .unwrap_or_default(),
            segment: doc
                .get_first(fields.segment)
                .and_then(|v| v.as_u64())
                .unwrap_or_default(),
            is_pdf: doc
                .get_first(fields.is_pdf)
                .and_then(|v| v.as_bool())
                .unwrap_or_default(),
            file_path: doc
                .get_first(fields.file_path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
        }))
    }

    /// Builds display-highlight patterns from the index terms this query
    /// actually matches — the same automaton scan the search and the snippet
    /// highlighter run — so a document found via any variant (typo,
    /// morphological affix, partial word) highlights exactly that variant in
    /// an opened book. Full parity with search by construction: the automatons
    /// are the very `regex_terms` branches `prepare_advanced_query` builds for
    /// the search query.
    ///
    /// A word whose branches match nothing in this index (or fail to compile)
    /// falls back to the query-shape pattern of [`generate_highlight_pattern`],
    /// so the result is never worse than the pure-string one. Runs FST scans
    /// against the term dictionary — async, unlike the sync pure-string
    /// fallback; the app fetches it once per search-parameter change and
    /// caches the compiled `RegExp`s.
    pub fn generate_index_highlight_pattern(
        &self,
        query: String,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        any_order: Option<bool>,
        cancel_request_id: Option<u32>,
    ) -> Result<Option<HighlightPattern>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let searcher = self.index_reader.searcher();
            let key = format!(
                "advanced {query:?} {distance} {:?} {:?} {:?} {any_order:?}",
                custom_spacing.iter().collect::<BTreeMap<_, _>>(),
                alternative_words.iter().collect::<BTreeMap<_, _>>(),
                search_options
                    .iter()
                    .map(|(word, options)| (word, options.iter().collect::<BTreeMap<_, _>>()))
                    .collect::<BTreeMap<_, _>>(),
            );
            let pattern =
                self.highlight_cache
                    .pattern(searcher.generation().generation_id(), key, || {
                        self.compute_index_highlight_pattern(
                            &searcher,
                            &query,
                            distance,
                            &custom_spacing,
                            &alternative_words,
                            &search_options,
                            any_order,
                        )
                    })?;
            Ok(pattern.map(|pattern| pattern.to_api()))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn compute_index_highlight_pattern(
        &self,
        _searcher: &Searcher,
        query: &str,
        distance: u32,
        custom_spacing: &HashMap<String, String>,
        alternative_words: &HashMap<u32, Vec<String>>,
        search_options: &HashMap<String, HashMap<String, bool>>,
        any_order: Option<bool>,
    ) -> Result<Option<DisplayPattern>> {
        let advanced = hebrew_query::prepare_advanced_query(
            query,
            distance,
            custom_spacing,
            alternative_words,
            search_options,
        );

        // Per word: the very term set the search serves — same automata, same
        // ranking, same budgets (the phrase degrade path's for a multi-word
        // query, which keep a superset of the scoped paths' ranked prefix) —
        // so every form a result was found by is painted. A word whose
        // branches fail to compile paints the query shape instead.
        let text_field = self.schema.get_field("text")?;
        let single = advanced.regex_terms.len() == 1;
        let (max_terms, postings_budget) = if single {
            (advanced.max_expansions, SINGLE_WORD_POSTINGS_BUDGET)
        } else {
            (
                PHRASE_POSITION_MAX_EXPANSIONS.max(advanced.max_expansions),
                PHRASE_POSITION_POSTINGS_BUDGET,
            )
        };
        let mut per_word_terms: Vec<Vec<String>> = Vec::with_capacity(advanced.regex_terms.len());
        for word_pattern in &advanced.regex_terms {
            let typo_tokens: &[String] = if single { &advanced.typo_tokens } else { &[] };
            let terms = match self.materialize_term_set(
                word_pattern.branches(),
                typo_tokens,
                text_field,
                max_terms,
                postings_budget,
            ) {
                Ok(entry) => entry
                    .terms
                    .iter()
                    .filter_map(|t| t.value().as_str().map(str::to_string))
                    .collect(),
                Err(e) => {
                    log::error!("highlight terms unavailable: {e}");
                    Vec::new()
                }
            };
            per_word_terms.push(terms);
        }

        Ok(display_highlight::build_display_highlight_from_terms(
            query,
            distance,
            custom_spacing,
            alternative_words,
            search_options,
            &per_word_terms,
            any_order.unwrap_or(false),
        )
        .map(DisplayPattern::from))
    }

    /// Fuzzy-mode counterpart of [`Self::generate_index_highlight_pattern`]:
    /// paints the index terms within `max_distance` edits of each query token,
    /// plus the dictionary morphological forms when a magic dictionary is
    /// loaded — mirroring [`Self::build_fuzzy_highlight`]'s term collection,
    /// so an opened book highlights exactly what the fuzzy search matched.
    pub fn generate_index_fuzzy_highlight_pattern(
        &self,
        query: String,
        max_distance: u8,
        cancel_request_id: Option<u32>,
    ) -> Result<Option<HighlightPattern>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            anyhow::ensure!(
                max_distance <= 2,
                "fuzzy highlight distance is limited to 2, got {max_distance}"
            );
            let searcher = self.index_reader.searcher();
            let pattern = self.highlight_cache.pattern(
                searcher.generation().generation_id(),
                format!("fuzzy {query:?} {max_distance}"),
                || self.compute_index_fuzzy_highlight_pattern(&searcher, &query, max_distance),
            )?;
            Ok(pattern.map(|pattern| pattern.to_api()))
        })
    }

    /// Display matching for smart lexical results: literal quotations stay grouped,
    /// while outside words include their original, canonical and dictionary forms.
    pub fn generate_index_smart_highlight_pattern(
        &self,
        query: String,
        lexical_mode: SemanticLexicalMode,
        max_distance: u8,
        cancel_request_id: Option<u32>,
    ) -> Result<Option<HighlightPattern>> {
        let request = self.lexical_cancellations.begin(cancel_request_id);
        request.scope(|| {
            let searcher = self.index_reader.searcher();
            let exact = matches!(lexical_mode, SemanticLexicalMode::Exact)
                || self.whole_query_quoted(&query)?;
            if !exact {
                anyhow::ensure!(
                    max_distance <= 2,
                    "fuzzy highlight distance is limited to 2, got {max_distance}"
                );
            }
            let pattern = self.highlight_cache.pattern(
                searcher.generation().generation_id(),
                format!("smart {query:?} {exact} {max_distance}"),
                || {
                    self.compute_index_smart_highlight_pattern(
                        &searcher,
                        &query,
                        exact,
                        max_distance,
                    )
                },
            )?;
            Ok(pattern.map(|pattern| pattern.to_api()))
        })
    }

    fn compute_index_smart_highlight_pattern(
        &self,
        searcher: &Searcher,
        query: &str,
        exact: bool,
        max_distance: u8,
    ) -> Result<Option<DisplayPattern>> {
        smart_query::display_pattern(query, exact, |alternatives| {
            Ok(self
                .lexical_phrase_per_word_terms(searcher, alternatives, max_distance)?
                .into_iter()
                .flatten()
                .collect::<HashSet<_>>()
                .into_iter()
                .collect())
        })
    }

    fn compute_index_fuzzy_highlight_pattern(
        &self,
        searcher: &Searcher,
        query: &str,
        max_distance: u8,
    ) -> Result<Option<DisplayPattern>> {
        let tokens = self.index_token_texts(query)?;
        // `build_display_highlight_from_terms` walks `split_query_words`;
        // the analyzer tokens are aligned with it by design, but if they ever
        // diverge, feeding misaligned terms would paint the wrong word — fall
        // back to the query-shape pattern instead.
        let words = hebrew_query::split_query_words(&hebrew_query::normalize_for_index(query));
        let per_word_terms: Vec<Vec<String>> = if tokens.len() != words.len() {
            Vec::new()
        } else {
            let mut collected = Vec::with_capacity(tokens.len());
            for token in &tokens {
                let mut matched = if max_distance == 0 {
                    HashSet::new()
                } else {
                    self.automaton_highlight_terms(
                        searcher,
                        &[self.fuzzy_automaton(token, max_distance)?],
                    )?
                };
                matched.insert(token.clone());
                matched.extend(Self::quoteless_variant(token));
                if let Some(dict) = self.magic_dict.as_ref() {
                    for form in dict.highlight_forms(token, MAX_LEXICAL_FORMS) {
                        matched.insert(form);
                    }
                }
                collected.push(matched.into_iter().collect());
            }
            collected
        };

        Ok(display_highlight::build_display_highlight_from_terms(
            query,
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &per_word_terms,
            false,
        )
        .map(DisplayPattern::from))
    }

    /// One index-term set per query word (in phrase order), materialized like
    /// the search's own term sets ([`Self::materialize_term_set`]: same
    /// automata, same ranking), so the sets contain exactly the morphological
    /// variants (prefixes, suffixes, alternatives) that genuinely matched.
    /// [`MAX_HIGHLIGHT_TERMS`] is split evenly between the words; a word's
    /// highest-ranked forms fill its share first.
    fn phrase_per_word_terms(
        &self,
        searcher: &Searcher,
        regex_terms: &[String],
        field: Field,
    ) -> Result<Vec<HashSet<String>>> {
        let cap = (MAX_HIGHLIGHT_TERMS / regex_terms.len().max(1)).max(1);
        let generation = searcher.generation().generation_id();
        regex_terms
            .iter()
            .map(|pattern| {
                let terms =
                    self.highlight_cache
                        .term_set(generation, field, pattern, cap, || {
                            let entry = self
                                .materialize_term_set(
                                    hebrew_query::WordPattern::parse(pattern).branches(),
                                    &[],
                                    field,
                                    cap as u32,
                                    u64::MAX,
                                )
                                .map_err(|e| {
                                    anyhow::anyhow!("invalid highlight regex {pattern:?}: {e}")
                                })?;
                            Ok(entry
                                .terms
                                .iter()
                                .filter_map(|t| t.value().as_str().map(str::to_string))
                                .collect())
                        })?;
                Ok(terms.iter().cloned().collect())
            })
            .collect()
    }

    /// Flattens per-word term sets into the `TermSetQuery` that drives
    /// `SnippetGenerator`'s fragment selection and highlighting — `RegexPhraseQuery`
    /// exposes no static terms of its own. `None` when nothing matched (the
    /// caller then falls back to the main query).
    fn terms_query_from_word_sets(
        &self,
        per_word_terms: &[HashSet<String>],
        text_f: Field,
    ) -> Result<Option<Box<dyn Query>>> {
        let words: Vec<&String> = per_word_terms.iter().flatten().collect();
        let searcher = self.index_reader.searcher();
        let terms = quote_expansion::spelling_terms(&searcher, text_f, &words)?;
        Ok((!terms.is_empty()).then(|| Box::new(TermSetQuery::new(terms)) as Box<dyn Query>))
    }

    /// Highlight plan for an advanced (regex) search.
    ///
    /// A single regex term runs as a `TermSetQuery` (see
    /// [`Self::single_regex_term_query`]) whose materialized terms ARE the terms
    /// that matched, so the main query already exposes them to `SnippetGenerator`
    /// — no highlight query, no phrase filter. The multi-term `RegexPhraseQuery`
    /// exposes no static terms, so it needs a separately-materialized flat
    /// highlight query AND — being a phrase — the per-word filter that keeps only
    /// in-order, per-pair-within-allowance occurrences painted (parity with the
    /// search's gap verification).
    ///
    /// `gaps` is the query's resolved per-pair allowance vector, which already
    /// folds `custom_spacing` in (else the global `distance` for every pair) —
    /// passing the raw `distance` would reject a spacing-permitted match and
    /// fall back to the broad term highlight.
    /// Scope-aware entry: the word-distance scope keeps the phrase filter
    /// (order + per-pair allowance) of [`Self::advanced_highlight_plan`];
    /// the paragraph/section scopes impose no order or distance inside a
    /// line, so every occurrence of every word variant is a true match to
    /// paint — flat term-union highlighting, no phrase filter.
    /// `acronym_alts` — חלופות פענוח ר"ת (תבניות-ליטרל פר-מילה): מילות כל
    /// חלופה מתממשות ומצטרפות לאיחוד ההדגשה השטוח. במסלול המרחק נשמר
    /// גם מסנן ביטוי נפרד לכל חלופה, עם המרווחים הקנוניים שלה.
    fn advanced_highlight_plan_for_scope(
        &self,
        searcher: &Searcher,
        regex_terms: &[String],
        proximity: &Proximity,
        voc: &VocalizedFlags,
        scope: &SearchScope,
        match_mode: &WordMatch,
        acronym_alts: &[Vec<String>],
    ) -> Result<HighlightPlan> {
        // בהתאמה חלקית אין דרישת סדר/מרחק — מסנן הביטוי של מסלול המרחק
        // היה מוחק הדגשות לגיטימיות של מילים בודדות או בסדר הפוך.
        let partial = !matches!(match_mode, WordMatch::All);
        match scope {
            SearchScope::WordDistance if !partial => {
                self.advanced_highlight_plan(searcher, regex_terms, proximity, voc, acronym_alts)
            }
            _ => {
                if regex_terms.len() < 2 && acronym_alts.is_empty() {
                    return Ok(HighlightPlan::none());
                }
                let field = self.search_text_field(voc)?;
                let mut word_sets = self.phrase_per_word_terms(searcher, regex_terms, field)?;
                for alt in acronym_alts {
                    word_sets.extend(self.phrase_per_word_terms(searcher, alt, field)?);
                }
                let query = self.terms_query_from_word_sets(&word_sets, field)?;
                Ok(HighlightPlan {
                    query,
                    phrase: None,
                })
            }
        }
    }

    fn advanced_highlight_plan(
        &self,
        searcher: &Searcher,
        regex_terms: &[String],
        proximity: &Proximity,
        voc: &VocalizedFlags,
        acronym_alts: &[Vec<String>],
    ) -> Result<HighlightPlan> {
        if regex_terms.len() < 2 && acronym_alts.is_empty() {
            return Ok(HighlightPlan::none());
        }
        let field = self.search_text_field(voc)?;
        let analyzer = if voc.any() {
            VOCALIZED_DISPLAY_ANALYZER
        } else {
            DISPLAY_ANALYZER
        };
        let per_word_terms = self.phrase_per_word_terms(searcher, regex_terms, field)?;
        let mut all_word_sets = per_word_terms.clone();
        let mut alternatives = Vec::with_capacity(acronym_alts.len());
        for alt in acronym_alts {
            let alt_terms = self.phrase_per_word_terms(searcher, alt, field)?;
            all_word_sets.extend(alt_terms.iter().cloned());
            alternatives.push(PhraseHighlight {
                proximity: Proximity::adjacent(alt_terms.len()),
                per_word_terms: alt_terms,
                analyzer,
                cross_line: !voc.any(),
                alternatives: Vec::new(),
                allow_term_fallback: true,
            });
        }
        let query = self.terms_query_from_word_sets(&all_word_sets, field)?;
        // A single-word main query still needs the expansion's phrase plan to
        // detect and render a match that continues on the next line.
        let phrase = Some(PhraseHighlight {
            per_word_terms,
            proximity: proximity.clone(),
            analyzer,
            // As in the query: an any-order match never continues across lines.
            cross_line: !voc.any() && !proximity.is_any_order(),
            alternatives,
            allow_term_fallback: true,
        });
        Ok(HighlightPlan { query, phrase })
    }

    /// Highlight plan for an exact (`Term`/`PhraseQuery`) search. A single term
    /// needs nothing — the `TermQuery` highlights itself. A multi-word
    /// `PhraseQuery` already exposes its terms to `SnippetGenerator`, so it needs
    /// no separate highlight query, but it is a strict-adjacency phrase, so it
    /// gets an all-zero gaps filter that drops every non-adjacent occurrence.
    ///
    /// The vocalized arm mirrors the advanced plan instead: each token's
    /// required-marks pattern is materialized against the vocalized
    /// dictionary (the `RegexPhraseQuery` exposes no static terms), and the
    /// phrase filter re-tokenizes fragments with the vocalized analyzer.
    fn exact_highlight_plan(
        &self,
        searcher: &Searcher,
        query_str: &str,
        voc: &VocalizedFlags,
    ) -> Result<HighlightPlan> {
        if voc.any() {
            let tokens = self.index_token_texts_with("hebrew_vocalized_query", query_str)?;
            if tokens.len() < 2 {
                return Ok(HighlightPlan::none());
            }
            let patterns: Vec<String> = tokens
                .iter()
                .map(|t| hebrew_query::vocalized_token_pattern(t, voc))
                .collect();
            let field = self.schema.get_field("textVocalized")?;
            let per_word_terms = self.phrase_per_word_terms(searcher, &patterns, field)?;
            let query = self.terms_query_from_word_sets(&per_word_terms, field)?;
            return Ok(HighlightPlan {
                query,
                phrase: Some(PhraseHighlight {
                    proximity: Proximity::adjacent(per_word_terms.len()),
                    per_word_terms,
                    analyzer: VOCALIZED_DISPLAY_ANALYZER,
                    cross_line: false,
                    alternatives: Vec::new(),
                    allow_term_fallback: true,
                }),
            });
        }
        let tokens = self.index_token_texts(query_str)?;
        if tokens.len() < 2 {
            return Ok(HighlightPlan::none());
        }
        let per_word_terms: Vec<HashSet<String>> =
            tokens.into_iter().map(|t| HashSet::from([t])).collect();
        Ok(HighlightPlan {
            query: None,
            phrase: Some(PhraseHighlight {
                proximity: Proximity::adjacent(per_word_terms.len()),
                per_word_terms,
                analyzer: DISPLAY_ANALYZER,
                cross_line: true,
                alternatives: Vec::new(),
                allow_term_fallback: true,
            }),
        })
    }

    fn smart_exact_highlight_plan(
        &self,
        searcher: &Searcher,
        query: &str,
        voc: &VocalizedFlags,
    ) -> Result<HighlightPlan> {
        if !smart_query::has_alternatives(query) {
            return self.exact_highlight_plan(searcher, query, voc);
        }
        let field = self.search_text_field(voc)?;
        let (sets, analyzer) = if voc.any() {
            let patterns = self
                .index_token_texts_with("hebrew_vocalized_query", query)?
                .iter()
                .map(|token| smart_query::vocalized_pattern(token, voc))
                .collect::<Vec<_>>();
            (
                self.phrase_per_word_terms(searcher, &patterns, field)?,
                VOCALIZED_DISPLAY_ANALYZER,
            )
        } else {
            (
                self.index_token_texts(query)?
                    .iter()
                    .map(|token| smart_query::alternatives(token).into_iter().collect())
                    .collect::<Vec<HashSet<String>>>(),
                DISPLAY_ANALYZER,
            )
        };
        let query = self.terms_query_from_word_sets(&sets, field)?;
        Ok(HighlightPlan {
            query,
            phrase: (sets.len() >= 2).then(|| PhraseHighlight {
                proximity: Proximity::adjacent(sets.len()),
                per_word_terms: sets,
                analyzer,
                cross_line: !voc.any(),
                alternatives: Vec::new(),
                allow_term_fallback: true,
            }),
        })
    }

    /// Quotations paint complete literal occurrences; only outside words can
    /// contribute edit-distance or dictionary forms. Single-word alternatives
    /// keep those outside words independent of the quotations' positions.
    fn smart_fuzzy_highlight_plan(
        &self,
        searcher: &Searcher,
        query: &str,
        max_distance: u8,
        voc: &VocalizedFlags,
    ) -> Result<HighlightPlan> {
        let expand_aliases = smart_query::has_alternatives(query);
        let alternatives = |token: &str, expand: bool| {
            if expand {
                smart_query::alternatives(token)
            } else {
                vec![token.to_string()]
            }
        };
        let phrases = quoted_phrases(query);
        if phrases.is_empty() {
            return if voc.any() {
                Ok(HighlightPlan::none())
            } else {
                self.fuzzy_highlight_plan(
                    searcher,
                    &self
                        .index_token_texts(query)?
                        .iter()
                        .flat_map(|token| alternatives(token, expand_aliases))
                        .collect::<Vec<_>>(),
                    &[],
                    max_distance,
                )
            };
        }
        let (query_analyzer, analyzer) = if voc.any() {
            ("hebrew_vocalized_query", VOCALIZED_DISPLAY_ANALYZER)
        } else {
            ("hebrew_query", DISPLAY_ANALYZER)
        };
        let field = self.search_text_field(voc)?;
        let mut outside = self.index_token_texts_with(query_analyzer, query)?;
        let mut variants = Vec::new();
        let mut all_word_sets = Vec::new();
        for phrase in phrases {
            let expand_phrase_aliases = smart_query::has_alternatives(&phrase);
            let tokens = self.index_token_texts_with(query_analyzer, &phrase)?;
            // Remove occurrences, not distinct words: the same word may also
            // appear outside a quotation and legitimately allow expansion there.
            for token in &tokens {
                if let Some(i) = outside.iter().position(|word| word == token) {
                    outside.remove(i);
                }
            }
            let per_word_terms = if voc.any() {
                let patterns = tokens
                    .iter()
                    .map(|token| {
                        if expand_phrase_aliases {
                            smart_query::vocalized_pattern(token, voc)
                        } else {
                            hebrew_query::vocalized_token_pattern(token, voc)
                        }
                    })
                    .collect::<Vec<_>>();
                self.phrase_per_word_terms(searcher, &patterns, field)?
            } else {
                tokens
                    .into_iter()
                    .map(|token| {
                        alternatives(&token, expand_phrase_aliases)
                            .into_iter()
                            .collect()
                    })
                    .collect()
            };
            if per_word_terms.is_empty() {
                continue;
            }
            all_word_sets.extend(per_word_terms.iter().cloned());
            variants.push(PhraseHighlight {
                proximity: Proximity::adjacent(per_word_terms.len()),
                per_word_terms,
                analyzer,
                cross_line: false,
                alternatives: Vec::new(),
                allow_term_fallback: false,
            });
        }
        let outside = outside
            .iter()
            .flat_map(|token| alternatives(token, expand_aliases))
            .collect::<Vec<_>>();
        let outside_sets = if voc.any() {
            let mut patterns = Vec::new();
            for token in &outside {
                let base = hebrew_query::strip_attached_marks(token);
                let mut branches = vec![hebrew_query::vocalized_token_pattern(token, voc)];
                let mut seen = HashSet::from([base.clone()]);
                if let Some(clean) = Self::quoteless_variant(&base) {
                    if seen.insert(clean.clone()) {
                        branches.push(hebrew_query::vocalized_free_pattern(&clean));
                    }
                }
                if max_distance > 0 {
                    if let Some(dict) = self.magic_dict.as_ref() {
                        for form in dict.highlight_forms(&base, MAX_LEXICAL_FORMS) {
                            if seen.insert(form.clone()) {
                                branches.push(hebrew_query::vocalized_free_pattern(&form));
                            }
                        }
                    }
                    branches.extend(self.vocalized_variant_branches(
                        searcher,
                        std::slice::from_ref(&base),
                        max_distance,
                        &mut seen,
                    )?);
                }
                patterns.push(
                    branches
                        .into_iter()
                        .map(|branch| format!("({branch})"))
                        .collect::<Vec<_>>()
                        .join("|"),
                );
            }
            self.phrase_per_word_terms(searcher, &patterns, field)?
        } else {
            self.lexical_phrase_per_word_terms(searcher, &outside, max_distance)?
        };
        for terms in outside_sets {
            all_word_sets.push(terms.clone());
            variants.push(PhraseHighlight {
                per_word_terms: vec![terms],
                proximity: Proximity::Ordered(Vec::new()),
                analyzer,
                cross_line: false,
                alternatives: Vec::new(),
                allow_term_fallback: false,
            });
        }
        let query = self.terms_query_from_word_sets(&all_word_sets, field)?;
        let phrase = if variants.is_empty() {
            None
        } else {
            let mut first = variants.remove(0);
            first.alternatives = variants;
            Some(first)
        };
        Ok(HighlightPlan { query, phrase })
    }

    /// Highlight plan for the approximate (`fuzzy`) search. Always builds the
    /// flat highlight query (fuzzy/lexical automatons expose no static terms).
    /// Multi-word fuzzy is a per-word AND, where every occurrence of every word
    /// is a real hit and must stay highlighted, so it carries no phrase filter
    /// — unless the whole query is one quoted phrase the lexical path requires.
    fn fuzzy_highlight_plan(
        &self,
        searcher: &Searcher,
        term_texts: &[String],
        phrases: &[Vec<String>],
        max_distance: u8,
    ) -> Result<HighlightPlan> {
        let query = self
            .build_fuzzy_highlight(searcher, term_texts, max_distance)
            .ok();
        let whole_query_quoted = term_texts.len() >= 2 && phrases.iter().any(|p| p == term_texts);
        let phrase = if whole_query_quoted && self.magic_dict.is_some() {
            let per_word_terms =
                self.lexical_phrase_per_word_terms(searcher, term_texts, max_distance)?;
            let gaps = vec![LEXICAL_FUZZY_PHRASE_SLOP; per_word_terms.len().saturating_sub(1)];
            Some(PhraseHighlight {
                per_word_terms,
                proximity: Proximity::Ordered(gaps),
                analyzer: DISPLAY_ANALYZER,
                cross_line: false,
                alternatives: Vec::new(),
                allow_term_fallback: true,
            })
        } else {
            None
        };
        Ok(HighlightPlan { query, phrase })
    }

    /// Per-word term sets for the lexical fuzzy *phrase* path: each word's
    /// edit-distance matches plus its blacklist-filtered dictionary forms and
    /// quote-free spelling — mirroring
    /// [`Self::generate_index_fuzzy_highlight_pattern`] so the results snippet
    /// and an opened book paint the same fuzzy variants.
    fn lexical_phrase_per_word_terms(
        &self,
        searcher: &Searcher,
        tokens: &[String],
        max_distance: u8,
    ) -> Result<Vec<HashSet<String>>> {
        tokens
            .iter()
            .map(|token| {
                let mut matched = if max_distance == 0 {
                    HashSet::new()
                } else {
                    self.automaton_highlight_terms(
                        searcher,
                        &[self.fuzzy_automaton(token, max_distance)?],
                    )?
                };
                matched.insert(token.clone());
                if let Some(clean) = Self::quoteless_variant(token) {
                    matched.insert(clean);
                }
                if let Some(dict) = self.magic_dict.as_ref() {
                    for form in dict.highlight_forms(token, MAX_LEXICAL_FORMS) {
                        matched.insert(form);
                    }
                }
                Ok(matched)
            })
            .collect()
    }

    /// Fuzzy-mode counterpart of [`Self::phrase_per_word_terms`]'s scan:
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
        let automatons = term_texts
            .iter()
            .map(|t| self.fuzzy_automaton(t, max_distance))
            .collect::<Result<Vec<_>>>()?;
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
        if self.magic_dict.is_some() {
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
        let (fuzzy_terms, complete) = if max_distance == 0 {
            (HashSet::new(), true)
        } else {
            let automatons = term_texts
                .iter()
                .map(|t| self.fuzzy_automaton(t, max_distance))
                .collect::<Result<Vec<_>>>()?;
            self.automaton_highlight_terms_complete(searcher, &automatons)?
        };

        // ...then add the literal tokens and the (blacklist-filtered) lexical
        // forms per token. The exact token can otherwise be omitted when a broad
        // fuzzy automaton exhausts its highlight-term budget first.
        let mut matched = HashSet::new();
        for token in term_texts {
            matched.insert(token.clone());
            matched.extend(Self::quoteless_variant(token));
            for form in dict.highlight_forms(token, MAX_LEXICAL_FORMS) {
                matched.insert(form);
            }
        }

        // A complete quote-transparent walk already holds every quoted spelling of
        // its terms; only a capped one needs them looked up.
        let mut words: Vec<&String> = matched.iter().collect();
        if !complete {
            words.extend(&fuzzy_terms);
        }
        let mut terms = quote_expansion::spelling_terms(searcher, text_f, &words)?;
        if complete {
            terms.extend(fuzzy_terms.iter().map(|t| Term::from_field_text(text_f, t)));
        }
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

    /// The automaton the search's `FuzzyTermQuery` (transposition = one edit) matches
    /// `token` with, so collected terms are exactly the ones the query can match.
    fn fuzzy_automaton(&self, token: &str, distance: u8) -> Result<DfaWrapper> {
        let field = self.schema.get_field("text")?;
        Ok(FuzzyTermQuery::new(Term::from_field_text(field, token), distance, true).automaton()?)
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
        Ok(self
            .automaton_highlight_terms_complete(searcher, automatons)?
            .0)
    }

    /// [`Self::automaton_highlight_terms`], and whether no automaton reached its
    /// share of the budget (every term it matches is in the set).
    fn automaton_highlight_terms_complete<A>(
        &self,
        searcher: &Searcher,
        automatons: &[A],
    ) -> Result<(HashSet<String>, bool)>
    where
        A: Automaton,
        A::State: Clone,
    {
        let mut matched: HashSet<String> = HashSet::new();
        let mut complete = true;
        // Split the term budget evenly between automatons: a global cap would
        // let one broad first word exhaust it and leave the remaining query
        // words with no highlighting at all.
        let per_automaton_cap = (MAX_HIGHLIGHT_TERMS / automatons.len().max(1)).max(1);
        for automaton in automatons {
            let terms = self.automaton_terms(searcher, automaton, per_automaton_cap)?;
            complete &= terms.len() < per_automaton_cap;
            matched.extend(terms);
        }
        Ok((matched, complete))
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
        self.automaton_terms_in_field(searcher, text_f, automaton, cap)
    }

    /// [`Self::automaton_terms`] against an explicit field's dictionary.
    fn automaton_terms_in_field<A>(
        &self,
        searcher: &Searcher,
        text_f: Field,
        automaton: &A,
        cap: usize,
    ) -> Result<Vec<String>>
    where
        A: Automaton,
        A::State: Clone,
    {
        let cancellation = crate::lexical_cancellation::LexicalFlag::current();
        let mut matched = Vec::new();
        let mut seen = HashSet::new();
        'segments: for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(text_f)?;
            let mut stream = inverted
                .terms()
                .search(QuoteTransparent::new(automaton))
                .into_stream()?;
            while stream.advance() {
                cancellation.check()?;
                if let Ok(term) = std::str::from_utf8(stream.key()) {
                    // contains-before-insert: a term already seen in an
                    // earlier segment costs no allocation at all.
                    if !seen.contains(term) {
                        seen.insert(term.to_string());
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

    /// Creates a `SnippetGenerator` for `text_field` (the plain `text` field,
    /// or `textVocalized` on the vocalized paths), configured from `hl`.
    /// Creation resolves the doc-frequencies of every query term, so reuse one
    /// generator across chunks instead of recreating it per call.
    fn make_snippet_generator(
        searcher: &Searcher,
        query: &dyn Query,
        text_field: Field,
        hl: &HighlightConfig,
    ) -> Result<SnippetGenerator> {
        let cancellation = crate::lexical_cancellation::LexicalFlag::current();
        // `SnippetGenerator::create` with the display analyzer instead of the field's:
        // a term reached through its quote-free spelling paints the quoted word.
        let mut terms = std::collections::BTreeSet::new();
        query.query_terms(&mut |term, _| {
            if term.field() == text_field {
                terms.insert(term.clone());
            }
        });
        let mut terms_text = std::collections::BTreeMap::new();
        for term in terms {
            cancellation.check()?;
            let Some(text) = term.value().as_str().map(str::to_string) else {
                continue;
            };
            let doc_freq = searcher.doc_freq(&term)?;
            if doc_freq > 0 {
                terms_text.insert(text, 1.0 / (1.0 + doc_freq as Score));
            }
        }
        let vocalized = text_field == searcher.schema().get_field("textVocalized")?;
        let analyzer = if vocalized {
            VOCALIZED_DISPLAY_ANALYZER
        } else {
            DISPLAY_ANALYZER
        };
        let tokenizer = searcher
            .index()
            .tokenizers()
            .get(analyzer)
            .with_context(|| format!("{analyzer} is not registered"))?;
        Ok(SnippetGenerator::new(
            terms_text,
            tokenizer,
            text_field,
            hl.max_chars as usize,
        ))
    }

    /// Re-derives a snippet's highlight markup so only complete phrase
    /// occurrences under the phrase's [`Proximity`] stay painted (see
    /// [`PhraseHighlight`]).
    ///
    /// Tokenizes the fragment with the same `text`-field analyzer the index and
    /// `SnippetGenerator` use, tags each token with the query words it can fill,
    /// then keeps the byte ranges of tokens forming an occurrence. Ordered: an
    /// occurrence `w0 … w1 … w_{k-1}` where each adjacent pair `w-1, w` is at
    /// most `gaps[w-1]` intermediate tokens apart — the leftmost complete,
    /// non-overlapping matches, retaining a candidate only when its entire
    /// phrase suffix can be completed. Any order: every word at its own token
    /// within one window — the earliest-ending, non-overlapping windows.
    ///
    /// Returns `None` when the fragment holds no complete occurrence, so the
    /// caller falls back to the plain term highlight instead of painting
    /// nothing (never less context than before).
    ///
    /// `window`: `fragment` is the whole line, cut to that many bytes around its
    /// first occurrence — tantivy picks the fragment by term density, which can
    /// cut the phrase off when its fuzzy variants cluster elsewhere.
    #[cfg(test)]
    fn phrase_filtered_snippet_html(
        searcher: &Searcher,
        fragment: &str,
        phrase: &PhraseHighlight,
        hl: &HighlightConfig,
        window: Option<usize>,
    ) -> Option<String> {
        Self::phrase_filtered_snippet(searcher, fragment, phrase, hl, window).map(|(_, html)| html)
    }

    /// [`Self::phrase_filtered_snippet_html`] with the byte offset in `fragment` where
    /// the painted text starts.
    fn phrase_filtered_snippet(
        searcher: &Searcher,
        fragment: &str,
        phrase: &PhraseHighlight,
        hl: &HighlightConfig,
        window: Option<usize>,
    ) -> Option<(usize, String)> {
        let whole = fragment;
        phrase
            .variants()
            .enumerate()
            .find_map(|(selected, variant)| {
                let word_count = variant.per_word_terms.len();
                let ranges = Self::phrase_occurrences(searcher, fragment, variant)?;
                let (fragment, mut ranges) = match window {
                    Some(budget) => {
                        crop_around_first_occurrence(fragment, &ranges, word_count, budget)?
                    }
                    None => (fragment, ranges),
                };
                // A snippet can contain both the acronym and its expansion, or
                // multiple expansions. Paint every complete variant in that window.
                for (i, other) in phrase.variants().enumerate() {
                    if i != selected {
                        ranges.extend(
                            Self::phrase_occurrences(searcher, fragment, other)
                                .into_iter()
                                .flatten(),
                        );
                    }
                }
                let offset = fragment.as_ptr() as usize - whole.as_ptr() as usize;
                Some((offset, Self::paint_ranges(fragment, ranges, hl)))
            })
    }

    /// The byte ranges of `fragment`'s complete phrase occurrences, one range per
    /// word, occurrence after occurrence; `None` when there is none.
    fn phrase_occurrences(
        searcher: &Searcher,
        fragment: &str,
        phrase: &PhraseHighlight,
    ) -> Option<Vec<(usize, usize)>> {
        let word_count = phrase.per_word_terms.len();
        if word_count == 0 {
            return None;
        }
        let mut analyzer = searcher.index().tokenizers().get(phrase.analyzer)?;

        // Index positions count words, not emitted tokens: quote-free twins
        // and paired readings share their position and cannot fill successive
        // query words. Keep their source ranges separate for painting.
        struct Candidate {
            order: usize,
            from: usize,
            to: usize,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut by_word = vec![Vec::new(); word_count];
        let mut stream = analyzer.token_stream(fragment);
        while let Some(token) = stream.next() {
            let ci = candidates.len();
            let mut matched = false;
            for (set, word_candidates) in phrase.per_word_terms.iter().zip(&mut by_word) {
                if set.contains(token.text.as_str()) {
                    word_candidates.push(ci);
                    matched = true;
                }
            }
            if matched {
                candidates.push(Candidate {
                    order: token.position,
                    from: token.offset_from,
                    to: token.offset_to,
                });
            }
        }

        let gaps = match &phrase.proximity {
            Proximity::Ordered(gaps) => gaps,
            &Proximity::AnyOrder { max_span } => {
                // By position, then token: a shared position paints its first token.
                let mut cells = Vec::new();
                for (w, word_candidates) in by_word.iter().enumerate() {
                    cells.extend(
                        word_candidates
                            .iter()
                            .map(|&ci| (candidates[ci].order, ci, w)),
                    );
                }
                cells.sort_unstable();
                let mut windows = WordWindows::default();
                windows.reset(word_count);
                let mut slots = Vec::new();
                for (order, ci, w) in cells {
                    let Ok(at) = u32::try_from(order) else {
                        break;
                    };
                    if windows.push(at, w).is_some() {
                        slots.push(ci);
                    }
                }
                let mut ranges = Vec::new();
                let mut assignment = Vec::new();
                let mut from = 0;
                while let Some(last) = windows.next_window(from, max_span, &mut assignment) {
                    let occurrence = ranges.len();
                    ranges.extend(assignment.iter().enumerate().map(|(w, &entry)| {
                        let candidate = &candidates[slots[windows.slot(entry, w)]];
                        (candidate.from, candidate.to)
                    }));
                    // Callers take an occurrence's first and last range as its edges.
                    ranges[occurrence..].sort_unstable();
                    from = last + 1;
                }
                return (!ranges.is_empty()).then_some(ranges);
            }
        };
        // Layer w stores only candidates able to complete words w..end and
        // the earliest viable continuation in layer w+1. A monotone cursor
        // finds that continuation in linear time per layer. Unlike greedily
        // accepting the first matching next word, this cannot get trapped by
        // a repeated word whose later suffix lies outside the next gap.
        // Time and space are O(word_count * candidate_count), with no
        // exponential backtracking when many words match the same pattern.
        let mut layers: Vec<Vec<(usize, usize)>> = vec![Vec::new(); word_count];
        layers[word_count - 1] = by_word[word_count - 1].iter().map(|&ci| (ci, 0)).collect();
        for w in (0..word_count - 1).rev() {
            let max_step = (gaps.get(w).copied().unwrap_or(0) as usize).saturating_add(1);
            let next = &layers[w + 1];
            let mut cursor = 0;
            let mut viable = Vec::new();
            for &ci in &by_word[w] {
                let order = candidates[ci].order;
                while cursor < next.len() && candidates[next[cursor].0].order <= order {
                    cursor += 1;
                }
                if cursor < next.len()
                    && candidates[next[cursor].0].order <= order.saturating_add(max_step)
                {
                    viable.push((ci, cursor));
                }
            }
            layers[w] = viable;
        }

        let mut ranges = Vec::new();
        let mut consumed_through = None;
        for (start, &(ci, _)) in layers[0].iter().enumerate() {
            if consumed_through.is_some_and(|order| candidates[ci].order <= order) {
                continue;
            }
            let mut entry = start;
            for layer in &layers {
                let (ci, next) = layer[entry];
                ranges.push((candidates[ci].from, candidates[ci].to));
                consumed_through = Some(candidates[ci].order);
                entry = next;
            }
        }
        (!ranges.is_empty()).then_some(ranges)
    }

    /// `fragment` as HTML with `ranges` painted.
    fn paint_ranges(
        fragment: &str,
        mut ranges: Vec<(usize, usize)>,
        hl: &HighlightConfig,
    ) -> String {
        // Same escaping as tantivy's `Snippet::to_html`. Ranges are built in
        // increasing, non-overlapping order; the guard is defensive.
        ranges.sort_by_key(|&(s, _)| s);
        let mut html = String::new();
        let mut start_from = 0usize;
        for (s, e) in ranges {
            if s < start_from {
                continue;
            }
            html.push_str(&htmlescape::encode_minimal(&fragment[start_from..s]));
            html.push_str(&hl.highlight_prefix);
            html.push_str(&htmlescape::encode_minimal(&fragment[s..e]));
            html.push_str(&hl.highlight_postfix);
            start_from = e;
        }
        html.push_str(&htmlescape::encode_minimal(&fragment[start_from..]));
        html
    }

    /// The snippet of a hit whose phrase continues onto the next line: the end of
    /// `line` and the start of `next`, joined at [`cross_line::SNIPPET_LINE_BREAK`] and
    /// painted. `None` when no phrase occurrence crosses the break.
    fn cross_line_snippet_html(
        searcher: &Searcher,
        line: &DisplayText,
        next: &DisplayText,
        phrase: &PhraseHighlight,
        hl: &HighlightConfig,
    ) -> Option<String> {
        // Same enumerator/whitespace cuts as joined_lines, retaining the offsets of
        // both source lines instead of searching their possibly repeated words.
        let left_end = line[..cross_line::content_range(line).end].trim_end().len();
        let right_start = cross_line::content_range(next).start;
        let right_start =
            right_start + next[right_start..].len() - next[right_start..].trim_start().len();
        let left = line.slice(0..left_end);
        let right = next.slice(right_start..next.len());
        let joined = DisplayText::join(&[&left, &right], cross_line::SNIPPET_LINE_BREAK);
        let line_break = left.len()..left.len() + cross_line::SNIPPET_LINE_BREAK.len();
        debug_assert_eq!(joined.text, cross_line::joined_lines(line, next).0);
        phrase
            .variants()
            .filter(|variant| variant.cross_line)
            .find_map(|variant| {
                let word_count = variant.per_word_terms.len();
                if word_count < 2 {
                    return None;
                }
                let ranges = Self::phrase_occurrences(searcher, &joined, variant)?;
                let crossing = ranges.chunks_exact(word_count).position(|occurrence| {
                    occurrence[0].0 < line_break.start
                        && occurrence[word_count - 1].1 > line_break.end
                })?;
                let from_crossing = &ranges[crossing * word_count..];
                let (fragment, ranges) = crop_around_first_occurrence(
                    &joined,
                    from_crossing,
                    word_count,
                    hl.max_chars as usize + cross_line::SNIPPET_LINE_BREAK.len(),
                )
                .unwrap_or((&joined, from_crossing.to_vec()));
                let offset = fragment.as_ptr() as usize - joined.as_ptr() as usize;
                Some(
                    joined
                        .annotate(
                            Self::paint_ranges(fragment, ranges, hl),
                            offset,
                            &hl.highlight_prefix,
                            &hl.highlight_postfix,
                        )
                        .replace(
                            cross_line::SNIPPET_LINE_BREAK,
                            cross_line::SNIPPET_LINE_BREAK_HTML,
                        ),
                )
            })
    }

    fn build_results(
        schema: &Schema,
        searcher: &Searcher,
        query: &dyn Query,
        text_field: Field,
        addresses: Vec<DocAddress>,
        hl: &HighlightConfig,
        phrase: Option<&PhraseHighlight>,
    ) -> Result<Vec<SearchResult>> {
        let snippet_generator = Self::make_snippet_generator(searcher, query, text_field, hl)?;
        Self::build_results_with_generator(
            schema,
            searcher,
            &snippet_generator,
            text_field,
            addresses,
            hl,
            phrase,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_results_with_generator(
        schema: &Schema,
        searcher: &Searcher,
        snippet_generator: &SnippetGenerator,
        text_field: Field,
        addresses: Vec<DocAddress>,
        hl: &HighlightConfig,
        phrase: Option<&PhraseHighlight>,
    ) -> Result<Vec<SearchResult>> {
        let cancellation = crate::lexical_cancellation::LexicalFlag::current();
        cancellation.check()?;
        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        // המסלול המנוקד מציג את העותק המנוקד; לשורה בלי עותק כזה — הטקסט הרגיל.
        let vocalized_field = text_field == schema.get_field("textVocalized")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;

        let mut documents = Vec::with_capacity(addresses.len());
        let mut loader = DocumentLoader::new(searcher);
        for doc_address in addresses {
            cancellation.check()?;
            match loader.load(doc_address) {
                Ok(d) => documents.push((doc_address, d)),
                Err(e) => {
                    // A hit the collectors counted but the doc store cannot
                    // materialize (e.g. a corrupt store block). Skipping it
                    // silently is what shows up in the UI as "3/4 results"
                    // with a load-more button that never delivers — leave a
                    // trace so the mismatch is diagnosable.
                    log::error!(
                        "dropping counted hit: doc store read failed at segment {} doc {}: {e}",
                        doc_address.segment_ord,
                        doc_address.doc_id
                    );
                }
            }
        }
        let rendering = if vocalized_field {
            HitRendering::Vocalized
        } else if crate::vocalized_snippet::enabled() {
            HitRendering::PlainAndVocalized
        } else {
            HitRendering::Plain
        };
        let texts = Self::resolve_hit_texts(schema, searcher, &documents, rendering)?;
        cancellation.check()?;
        let next_lines = match phrase {
            Some(pf) if pf.cross_line && !vocalized_field => {
                Self::cross_line_next_lines(schema, searcher, &documents, &texts, pf)?
            }
            _ => Vec::new(),
        };

        // A snippet costs as much as its line is long, and one line can hold a whole book.
        // This byte-budget heuristic coarsens only small pages while process-local
        // library-preparation/enqueue reservations are observed.
        let render_min_len = if texts.len() < 16
            && crate::indexing_diagnostics::has_library_preparation_reservations()
            && texts
                .iter()
                .flat_map(|hit| {
                    [
                        hit.plain.len(),
                        hit.vocalized.as_ref().map_or(0, String::len),
                    ]
                })
                .chain(next_lines.iter().flatten().map(|text| text.len()))
                .try_fold(16 * 1024usize, |remaining, bytes| {
                    remaining.checked_sub(bytes)
                })
                .is_some()
        {
            8
        } else {
            1
        };
        use rayon::prelude::*;
        let results = documents
            .par_iter()
            .zip(texts.into_par_iter())
            .enumerate()
            .with_min_len(render_min_len)
            .map(
                |(slot, ((_, retrieved_doc), hit_text))| -> Result<SearchResult> {
                    cancellation.check()?;
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

                    let text_status = hit_text.status;
                    let (text, shown_vocalized) = match rendering {
                        HitRendering::PlainAndVocalized => {
                            let vocalized = hit_text
                                .vocalized
                                .map(|text| hit_text.notes.clone().display(text, true));
                            (hit_text.notes.display(hit_text.plain, false), vocalized)
                        }
                        _ => (hit_text.display_notes(vocalized_field), None),
                    };
                    let cross_line_html = match (next_lines.get(slot), phrase) {
                        (Some(Some(next)), Some(pf)) => {
                            Self::cross_line_snippet_html(searcher, &text, next, pf, hl)
                        }
                        _ => None,
                    };
                    let continues_to_next_line = cross_line_html.is_some();
                    let result_text = match (text_status, cross_line_html) {
                        (TextStatus::Ok, Some(html)) => html,
                        (TextStatus::Ok, None) => {
                            match Self::snippet_html(searcher, snippet_generator, &text, hl, phrase)
                            {
                                Some((offset, html)) => {
                                    let html = text.annotate(
                                        html,
                                        offset,
                                        &hl.highlight_prefix,
                                        &hl.highlight_postfix,
                                    );
                                    shown_vocalized
                                        .as_ref()
                                        .and_then(|vocalized| {
                                            crate::vocalized_snippet::vocalize_snippet_html(
                                                &html,
                                                &text,
                                                offset,
                                                vocalized,
                                                &hl.highlight_prefix,
                                                &hl.highlight_postfix,
                                            )
                                        })
                                        .unwrap_or(html)
                                }
                                None => bounded_display_snippet(
                                    shown_vocalized.as_ref().unwrap_or(&text),
                                    hl.max_chars,
                                ),
                            }
                        }
                        // The line no longer matches what was indexed: painting the query's
                        // terms into it would claim a match the index never saw.
                        (TextStatus::Stale, _) => bounded_plain_snippet(&text, hl.max_chars),
                        (TextStatus::Unavailable, _) => String::new(),
                    };

                    cancellation.check()?;
                    Ok(SearchResult {
                        title,
                        reference,
                        text: result_text,
                        id,
                        segment,
                        is_pdf,
                        file_path,
                        merged_count: 1,
                        merged: Vec::new(),
                        text_status,
                        continues_to_next_line,
                    })
                },
            )
            .collect::<Result<Vec<_>>>()?;
        cancellation.check()?;
        Ok(results)
    }

    /// The next line's text for each hit whose phrase may continue onto it: a hit
    /// without an in-line occurrence, on a line whose phrases may continue onto the
    /// next one, when that line reads back as indexed. `None` for every other hit.
    fn cross_line_next_lines(
        schema: &Schema,
        searcher: &Searcher,
        documents: &[(DocAddress, TantivyDocument)],
        texts: &[HitText],
        phrase: &PhraseHighlight,
    ) -> Result<Vec<Option<DisplayText>>> {
        let id_field = schema.get_field("id")?;
        let file_path_field = schema.get_field("filePath")?;
        let mut out = vec![None; documents.len()];
        let mut next_documents = Vec::new();
        let mut loader = DocumentLoader::new(searcher);
        let mut slots = Vec::new();
        for (slot, ((address, document), text)) in documents.iter().zip(texts).enumerate() {
            if text.status != TextStatus::Ok
                || phrase.variants().any(|variant| {
                    Self::phrase_occurrences(searcher, &text.plain, variant).is_some()
                })
            {
                continue;
            }
            let ends_open = searcher
                .segment_reader(address.segment_ord)
                .fast_fields()
                .u64(cross_line::LINE_LAST_FIELD)?
                .first(address.doc_id)
                .is_some_and(|v| v > 0);
            let Some(id) = document.get_first(id_field).and_then(|v| v.as_u64()) else {
                continue;
            };
            if !ends_open {
                continue;
            }
            // By the book too: two books' lines can share an id (see `address_in_book`).
            let file_path = document
                .get_first(file_path_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let reader = searcher.segment_reader(address.segment_ord);
            let paths = reader.fast_fields().str("filePath")?;
            let book_of = |doc| paths.as_ref().and_then(|p| p.term_ords(doc).next());
            let beside = crate::id_lookup::neighbour(
                reader,
                &reader.fast_fields().u64("id")?,
                address.doc_id,
                id + 1,
            )
            .filter(|&doc| book_of(doc) == book_of(address.doc_id))
            .map(|doc| DocAddress::new(address.segment_ord, doc));
            let next = match beside {
                Some(next) => next,
                None => {
                    let next_query = BooleanQuery::new(vec![
                        (
                            Occur::Must,
                            Box::new(TermQuery::new(
                                Term::from_field_text(file_path_field, file_path),
                                IndexRecordOption::Basic,
                            )) as Box<dyn Query>,
                        ),
                        (Occur::Must, Box::new(IdSetQuery::new([id + 1]))),
                    ]);
                    let Some((_, next)) = searcher
                        .search(&next_query, &TopDocs::with_limit(1).order_by_score())?
                        .into_iter()
                        .next()
                    else {
                        continue;
                    };
                    next
                }
            };
            let starts_open = searcher
                .segment_reader(next.segment_ord)
                .fast_fields()
                .u64(cross_line::LINE_FIRST_FIELD)?
                .first(next.doc_id)
                .is_some_and(|v| v > 0);
            if starts_open {
                next_documents.push((next, loader.load(next)?));
                slots.push(slot);
            }
        }
        if next_documents.is_empty() {
            return Ok(out);
        }
        let next_texts =
            Self::resolve_hit_texts(schema, searcher, &next_documents, HitRendering::Plain)?;
        for (slot, text) in slots.into_iter().zip(next_texts) {
            if text.status == TextStatus::Ok {
                out[slot] = Some(text.display_notes(false));
            }
        }
        Ok(out)
    }

    /// The highlighted snippet of `text` and the byte offset in `text` where it starts,
    /// or `None` when the generator found nothing to show (the caller then shows the line as is).
    fn snippet_html(
        searcher: &Searcher,
        snippet_generator: &SnippetGenerator,
        text: &str,
        hl: &HighlightConfig,
        phrase: Option<&PhraseHighlight>,
    ) -> Option<(usize, String)> {
        let mut snippet = snippet_generator.snippet(text);
        snippet.set_snippet_prefix_postfix(&hl.highlight_prefix, &hl.highlight_postfix);
        // For multi-word phrase queries, tantivy's term-based highlighter
        // paints every occurrence of every query term; re-derive the
        // highlights so only in-order, within-gap phrase occurrences stay
        // painted. Falls back to the plain term highlight when the chosen
        // fragment holds no complete phrase occurrence (never paints less
        // context than before).
        let (lead, tail) = glued_punctuation(text, snippet.fragment_range());
        let fragment = [lead, snippet.fragment(), tail].concat();
        let fragment_start = snippet.fragment_range().start - lead.len();
        let term_html = || {
            let html = htmlescape::encode_minimal(lead)
                + &snippet.to_html()
                + &htmlescape::encode_minimal(tail);
            (fragment_start, html)
        };
        let (offset, snippet_html) = match phrase {
            Some(pf) => Self::phrase_filtered_snippet(searcher, &fragment, pf, hl, None)
                .map(|(offset, html)| (fragment_start + offset, html))
                .or_else(|| {
                    Self::phrase_filtered_snippet(
                        searcher,
                        text,
                        pf,
                        hl,
                        Some(hl.max_chars as usize),
                    )
                })
                // The caller's display fallback wraps the source before adding
                // its synthetic ellipsis, and can use the vocalized rendering.
                .or_else(|| pf.allow_term_fallback.then(term_html))?,
            None => term_html(),
        };
        (!snippet_html.is_empty()).then_some((offset, snippet_html))
    }

    /// The display text of each document, in order: the stored copy for
    /// [`TextStorage::InIndex`] documents, the library-database row for the rest.
    /// Every library row of the call is read in one transaction.
    ///
    /// `rendering`: which renderings of library rows to derive.
    fn resolve_hit_texts(
        schema: &Schema,
        searcher: &Searcher,
        documents: &[(DocAddress, TantivyDocument)],
        rendering: HitRendering,
    ) -> Result<Vec<HitText>> {
        let text_stored_f = schema.get_field(TEXT_STORED_FIELD)?;
        let vocalized_stored_f = schema.get_field(TEXT_VOCALIZED_STORED_FIELD)?;
        let note_field = schema.get_field(note_spans::FIELD).ok();
        let file_path_f = schema.get_field("filePath")?;
        let segment_f = schema.get_field("segment")?;

        struct Pending {
            slot: usize,
            source: crate::line_source::Source,
            key: crate::line_source::LineKey,
            line_check: Option<u32>,
            line_hash: u64,
        }
        let mut out = Vec::with_capacity(documents.len());
        let mut pending: Vec<Pending> = Vec::new();
        let mut check_columns = HashMap::new();
        for (slot, (address, document)) in documents.iter().enumerate() {
            if let Some(stored) = document.get_first(text_stored_f).and_then(|v| v.as_str()) {
                out.push(HitText {
                    plain: stored.to_string(),
                    vocalized: document
                        .get_first(vocalized_stored_f)
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    notes: NoteSpans::decode(
                        note_field
                            .and_then(|field| document.get_first(field))
                            .and_then(|value| value.as_str()),
                    ),
                    status: TextStatus::Ok,
                });
                continue;
            }
            out.push(HitText::unavailable());
            let file_path = document
                .get_first(file_path_f)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let Some((source, book_id)) = line_source_book(file_path) else {
                log::warn!("document without stored text has no line-source book: {file_path:?}");
                continue;
            };
            let ordinal = document
                .get_first(segment_f)
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let (check_column, hash_column) = match check_columns.entry(address.segment_ord) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let fast = searcher.segment_reader(address.segment_ord).fast_fields();
                    e.insert((fast.u64(LINE_CHECK_FIELD)?, fast.u64("lineHash")?))
                }
            };
            pending.push(Pending {
                slot,
                source,
                key: crate::line_source::LineKey { book_id, ordinal },
                line_check: check_column
                    .first(address.doc_id)
                    .and_then(|v| u32::try_from(v).ok()),
                line_hash: hash_column.first(address.doc_id).unwrap_or_default(),
            });
        }
        if pending.is_empty() {
            return Ok(out);
        }

        // One transaction per source, one after the other: never two mutexes at once.
        for source in crate::line_source::Source::all() {
            let pending: Vec<&Pending> = pending.iter().filter(|p| p.source == source).collect();
            if pending.is_empty() {
                continue;
            }
            let keys: Vec<_> = pending.iter().map(|p| p.key).collect();
            let checks: Vec<_> = pending.iter().map(|p| p.line_check).collect();
            let rows = match crate::line_source::with_store(source, |store| {
                store.fetch_window(&keys, &checks)
            }) {
                Ok(rows) => rows,
                Err(reason) => {
                    debug!(
                        "{source:?} text unavailable for {} hit(s): {reason:?}",
                        keys.len()
                    );
                    continue;
                }
            };
            // Normalizing a window's rows is most of its cost; a lone row is not worth the hop.
            use rayon::prelude::*;
            let hits: Vec<HitText> = pending
                .par_iter()
                .zip(rows.into_par_iter())
                .with_min_len(LIBRARY_ROWS_PER_TASK)
                .map(|(p, row)| {
                    HitText::from_library_row(row, p.line_check, p.line_hash, rendering)
                })
                .collect();
            for (p, hit) in pending.iter().zip(hits) {
                out[p.slot] = hit;
            }
        }
        Ok(out)
    }
}

/// Library rows normalized per parallel task in [`SearchEngine::resolve_hit_texts`].
const LIBRARY_ROWS_PER_TASK: usize = 8;

/// Which renderings [`SearchEngine::resolve_hit_texts`] derives from a library row.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HitRendering {
    Plain,
    /// The vocalized one where the row has marks (what `textVocalized` was indexed from).
    Vocalized,
    /// Both: the snippet is built on the plain one and shown on the vocalized one.
    PlainAndVocalized,
}

/// One hit's display text before snippeting — see [`SearchEngine::resolve_hit_texts`].
struct HitText {
    plain: String,
    /// The vocalized rendering, for documents that have one.
    vocalized: Option<String>,
    notes: NoteSpans,
    status: TextStatus,
}

impl HitText {
    fn unavailable() -> Self {
        HitText {
            plain: String::new(),
            vocalized: None,
            notes: NoteSpans::default(),
            status: TextStatus::Unavailable,
        }
    }

    /// The texts `add_text_book` indexed this row as, `Ok` when the row is the line the
    /// document was indexed from: its `lineCheck`, and its `lineHash` when that is not 0.
    fn from_library_row(
        row: crate::line_source::RowText,
        line_check: Option<u32>,
        line_hash: u64,
        rendering: HitRendering,
    ) -> Self {
        use crate::line_source::RowText;
        let raw = match row {
            RowText::Found(raw) => raw,
            RowText::Missing => {
                return HitText {
                    plain: String::new(),
                    vocalized: None,
                    notes: NoteSpans::default(),
                    status: TextStatus::Stale,
                }
            }
            RowText::Unreadable => return HitText::unavailable(),
        };
        let only_vocalized = rendering == HitRendering::Vocalized;
        let (plain, vocalized) = if rendering == HitRendering::PlainAndVocalized {
            hebrew_query::normalize_line_for_indexing(&raw)
        } else {
            let vocalized = (only_vocalized && hebrew_query::contains_attached_marks(&raw))
                .then(|| hebrew_query::normalize_vocalized_text_for_indexing(&raw))
                .filter(|v| !v.is_empty());
            let plain = if vocalized.is_none() || line_hash != 0 {
                hebrew_query::normalize_text_for_indexing(&raw)
            } else {
                String::new()
            };
            (plain, vocalized)
        };
        let status = if line_check == Some(crate::line_source::line_check(&raw))
            && (line_hash == 0 || line_dedup_hash(&plain) == line_hash)
        {
            TextStatus::Ok
        } else {
            TextStatus::Stale
        };
        // The source's lineCheck and normalized lineHash must agree first. A changed
        // library row is never decorated as if it were the indexed note layout.
        let notes = if status == TextStatus::Ok {
            NoteSpans::from_html(&raw, &plain, vocalized.as_deref())
        } else {
            NoteSpans::default()
        };
        // `display` shows the vocalized rendering whenever there is one.
        let plain = if vocalized.is_some() && only_vocalized {
            String::new()
        } else {
            plain
        };
        HitText {
            plain,
            vocalized,
            notes,
            status,
        }
    }

    fn display_notes(self, vocalized: bool) -> DisplayText {
        match self.vocalized {
            Some(text) if vocalized => self.notes.display(text, true),
            _ => self.notes.display(self.plain, false),
        }
    }
}

/// The text a document was indexed from, as [`indexed_texts`] reads it.
pub(crate) struct IndexedText {
    pub(crate) text: String,
    pub(crate) status: TextStatus,
}

/// The normalized text each document was indexed from — what its `text` field holds: the
/// stored copy, or its library row read back and checked as a result's is
/// ([`SearchEngine::resolve_hit_texts`]), every row of the call in one transaction. A
/// [`TextStatus::Stale`] line carries the row the library holds now (empty when it is
/// gone), and a [`TextStatus::Unavailable`] one no text. What the semantic paths compute a
/// line's chunk key from.
pub(crate) fn indexed_texts(
    searcher: &Searcher,
    documents: &[(DocAddress, TantivyDocument)],
) -> Result<Vec<IndexedText>> {
    Ok(SearchEngine::resolve_hit_texts(
        searcher.schema(),
        searcher,
        documents,
        HitRendering::Plain,
    )?
    .into_iter()
    .map(|hit| IndexedText {
        text: hit.plain,
        status: hit.status,
    })
    .collect())
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

/// Which counts a ranked page is collected with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PageCounts {
    None,
    Total,
    TotalAndBooks,
}

struct RankedPage {
    addresses: Vec<DocAddress>,
    /// 0 unless counted.
    total: u32,
    // Full counts are consumed by the Flutter stream and its engine tests.
    #[cfg_attr(not(any(feature = "flutter-bridge", test)), allow(dead_code))]
    book_counts: Option<HashMap<String, u32>>,
}

/// `top` with the counts `counts` asks for, in one pass.
#[allow(clippy::type_complexity)]
fn collect_ranked<C, K>(
    searcher: &Searcher,
    query: &dyn Query,
    top: C,
    counts: PageCounts,
) -> Result<(Vec<DocAddress>, Option<u32>, Option<HashMap<String, u32>>)>
where
    C: Collector<Fruit = Vec<(K, DocAddress)>>,
{
    let addresses = |hits: Vec<(K, DocAddress)>| hits.into_iter().map(|(_, a)| a).collect();
    Ok(match counts {
        PageCounts::None => (addresses(searcher.search(query, &top)?), None, None),
        PageCounts::Total => {
            let (hits, total) = searcher.search(query, &(top, Count))?;
            (addresses(hits), Some(total as u32), None)
        }
        PageCounts::TotalAndBooks => {
            let (hits, total, books) = searcher.search(query, &(top, Count, BookCountCollector))?;
            (addresses(hits), Some(total as u32), Some(books))
        }
    })
}

/// Cancels request ids on one engine without waiting for its lock or workers.
/// Keep this handle while searching; cancellation also covers calls not yet started.
#[frb(opaque)]
pub struct LexicalCancellationHandle {
    registry: Arc<LexicalCancellations>,
}

impl LexicalCancellationHandle {
    /// Stops lexical calls tagged with `request_id`; unrelated ids keep running.
    /// Cancelled counts fail; streams close without another event.
    #[frb(sync)]
    pub fn cancel(&self, request_id: u32) {
        self.registry.cancel(request_id);
    }
}

// ── User regex (`/…/`) ─────────────────────────────────────────────────────────
//
// The engine side of a regular expression the user types wrapped in slashes; the work is in
// `crate::user_regex`. The application calls [`parse_user_regex`] on every query that is
// wrapped in `/…/`: `Invalid` means "search the text as ordinary words", any other kind is
// searched with the `*_user_regex*` family below, each call taking a
// [`UserRegexCancellationToken`]. A cross-space pattern may read whole books, so its counts
// arrive with the stream's last event, and the count/facet calls after a finished stream are
// answered from its cached result.

/// How a `/…/` pattern is searched (see [`parse_user_regex`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserRegexKind {
    /// Not usable as a regex (not wrapped, syntax, too complex): search it as plain text.
    Invalid,
    /// No whitespace: matched against whole index terms (`/מש.ה/`, `/[א-ת]+ים/`).
    SingleWord,
    /// Words separated by a space or `\s+`: an adjacent phrase of term patterns, which may
    /// continue onto the next line (`/(משה|אהרן) רבנו/`).
    WordSequence,
    /// Anything else that spans a space, or has `^`/`$` at an edge: matched against each
    /// line's text (`/שמע .* ישראל/`, `/^ספר$/`).
    CrossSpace,
}

/// What [`parse_user_regex`] made of a query.
pub struct UserRegexPlan {
    pub kind: UserRegexKind,
    /// The pattern as searched: without `/…/`, edge `\b` and edge spaces, folded like the
    /// index text (no nikud, ASCII quotes, maqaf as a space); an edge `^`/`$` stays as
    /// `\A`/`\z`. Empty when `Invalid`.
    pub pattern: String,
    /// The pattern of each word, for `SingleWord` (one) and `WordSequence`; empty otherwise.
    pub words: Vec<String>,
    /// Why the query is `Invalid`.
    pub reason: Option<String>,
}

/// Classifies a query typed as `/pattern/`. Cheap and synchronous: parsing plus a
/// size-limited compile, never more than a few milliseconds whatever the pattern.
///
/// Rules: the outer `/…/` and `\b` at the pattern's edges are dropped (a term pattern
/// matches whole terms, a cross-space one starts and ends on a word boundary when its edge is
/// a letter). `^` or `$` at an edge makes the pattern cross-space, anchored to the line's
/// start or end, or past an enumerator at that edge (`(ג) `, `{פ}`, a spaced `[א] `).
/// `.` and negated classes never cross a space by themselves; only explicit whitespace (a
/// space, `\s`, `\W`, a class of spaces) makes a pattern cross-space — as do an inner anchor,
/// and a sequence word that names no letter (`.*`).
/// Matching ignores case and nikud; geresh/gershayim match their ASCII forms.
#[frb(sync)]
pub fn parse_user_regex(raw: String) -> UserRegexPlan {
    match crate::user_regex::parse(&raw) {
        Ok(regex) => UserRegexPlan {
            kind: regex.kind.api(),
            words: regex.parts.iter().map(|p| p.pattern.clone()).collect(),
            pattern: regex.pattern,
            reason: None,
        },
        Err(reason) => UserRegexPlan {
            kind: UserRegexKind::Invalid,
            pattern: String::new(),
            words: Vec::new(),
            reason: Some(reason),
        },
    }
}

/// Finds a `/…/` pattern in displayed book text (markup and nikud included); see
/// [`prepare_user_regex_matcher`].
#[frb(opaque)]
pub struct UserRegexMatcher {
    inner: crate::user_regex::DisplayMatcher,
}

impl UserRegexMatcher {
    /// Every match in `data` (one line of the book, HTML as shown), in UTF-16 units, with
    /// one word range per word of a `WordSequence`.
    #[frb(sync)]
    pub fn find_matches(&self, data: String) -> Vec<HighlightMatch> {
        convert_highlight_matches(self.inner.find(&data))
    }
}

/// The book-view matcher of a `/…/` query, compiled once; `None` when it is `Invalid`.
#[frb(sync)]
pub fn prepare_user_regex_matcher(raw: String) -> Option<UserRegexMatcher> {
    let regex = crate::user_regex::parse(&raw).ok()?;
    crate::user_regex::DisplayMatcher::new(&regex).map(|inner| UserRegexMatcher { inner })
}

/// Stops a `*_user_regex*` call. Opaque and borrowed, like [`SemanticCancellationToken`]:
/// Dart keeps the object and [`Self::cancel`] reaches the running search, which looks at it
/// while collecting terms, between verified batches and between the books it reads. One
/// token per search; a cancelled one stays cancelled.
#[frb(opaque)]
pub struct UserRegexCancellationToken {
    flag: Arc<std::sync::atomic::AtomicBool>,
}

impl UserRegexCancellationToken {
    #[frb(sync)]
    pub fn new() -> Self {
        Self {
            flag: Arc::default(),
        }
    }

    /// Returns at once; the search stops at its next look.
    #[frb(sync)]
    pub fn cancel(&self) {
        self.flag.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    #[frb(sync, getter)]
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for UserRegexCancellationToken {
    #[frb(ignore)]
    fn default() -> Self {
        Self::new()
    }
}

/// The match ranges of one result of a cross-space search.
pub struct UserRegexLineMatches {
    /// UTF-16 ranges in the result's whole line as [`SearchEngine::get_document_by_id`]
    /// returns its `text` (the snippet is already painted). Empty for term kinds: their
    /// snippets are painted, and [`UserRegexMatcher`] finds them in the book.
    pub ranges: Vec<HighlightRange>,
}

/// One event of [`SearchEngine::search_user_regex_stream`].
///
/// The last event has `done` set and carries the final `total_count`/`book_counts` and
/// `truncated` (and `cancelled` when the token stopped it). Term kinds also send their counts
/// first, before the results, as the other streams do; a cross-space search reports
/// `progress_done`/`progress_total` (candidate lines, or books read) and `found_so_far`
/// while it runs, and the page's results as soon as they are found.
pub struct UserRegexStreamUpdate {
    pub kind: UserRegexKind,
    pub results: Vec<SearchResult>,
    /// One entry per result, in order.
    pub match_ranges: Vec<UserRegexLineMatches>,
    pub total_count: Option<u32>,
    pub book_counts: Option<HashMap<String, u32>>,
    pub found_so_far: u32,
    pub progress_done: u32,
    pub progress_total: u32,
    /// Partial: a term budget ran out, more than `MAX_LINE_HITS` lines matched, or some lines
    /// could not be read (the library database suspended or busy).
    pub truncated: bool,
    pub done: bool,
    pub cancelled: bool,
}

/// A term pattern's query, whether a budget truncated it, and each word's terms.
type UserRegexTermQuery = (Box<dyn Query>, bool, Vec<Arc<Vec<String>>>);

/// Terms one word of a `SingleWord` pattern may expand to before it is truncated.
const USER_REGEX_WORD_MAX_TERMS: u32 = 10_000;

impl SearchEngine {
    /// Results of a `/…/` query in the order asked for (a cross-space pattern is always in
    /// catalogue order), `limit` from `offset`, in chunks of `chunk_size`, as
    /// [`UserRegexStreamUpdate`] events. Grouping is not offered. An invalid pattern is an
    /// error event; check [`parse_user_regex`] first.
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "flutter-bridge")]
    pub fn search_user_regex_stream(
        &self,
        raw: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        chunk_size: u32,
        cancellation: &UserRegexCancellationToken,
        sink: StreamSink<UserRegexStreamUpdate>,
    ) -> Result<()> {
        let mut emit = |update: UserRegexStreamUpdate| sink.add(update).is_ok();
        let result = self.user_regex_stream_into(
            &raw,
            &facets,
            limit,
            offset,
            &order,
            chunk_size,
            &cancellation.flag,
            &mut emit,
        );
        Self::surface_stream_error(&sink, result)
    }

    /// [`Self::search_user_regex_stream`] into `emit`, which returns `false` once nobody
    /// listens.
    #[allow(clippy::too_many_arguments)]
    #[cfg(any(feature = "flutter-bridge", test))]
    pub(crate) fn user_regex_stream_into(
        &self,
        raw: &str,
        facets: &[String],
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        chunk_size: u32,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
        emit: &mut dyn FnMut(UserRegexStreamUpdate) -> bool,
    ) -> Result<()> {
        let regex = Self::user_regex(raw)?;
        let searcher = self.index_reader.searcher();
        if regex.kind == crate::user_regex::RegexKind::Line {
            let ctx = self.user_regex_line_search(&searcher, facets, cancel)?;
            let hl = HighlightConfig::default();
            let page = crate::user_regex::Page {
                offset,
                limit,
                chunk_size,
                highlight_prefix: hl.highlight_prefix,
                highlight_postfix: hl.highlight_postfix,
                max_chars: hl.max_chars,
            };
            crate::user_regex::run_line_search(&ctx, &regex, Some(&page), emit)?;
            return Ok(());
        }
        self.user_regex_term_stream(
            &searcher, &regex, facets, limit, offset, order, chunk_size, cancel, emit,
        )
    }

    /// Lines a `/…/` query matches. A cancelled call is an error.
    pub fn count_user_regex(
        &self,
        raw: String,
        facets: Vec<String>,
        cancellation: &UserRegexCancellationToken,
    ) -> Result<CountResult> {
        let regex = Self::user_regex(&raw)?;
        let searcher = self.index_reader.searcher();
        if regex.kind == crate::user_regex::RegexKind::Line {
            let outcome = self.user_regex_line_outcome(&searcher, &regex, &facets, cancellation)?;
            return Ok(CountResult {
                count: outcome.count(),
                truncated: outcome.truncated,
            });
        }
        let (query, truncated, _) =
            self.user_regex_term_query(&searcher, &regex, &facets, &cancellation.flag)?;
        let count = Self::user_regex_checked(&cancellation.flag, searcher.search(&*query, &Count))?;
        Ok(CountResult {
            count: count as u32,
            truncated,
        })
    }

    /// Lines a `/…/` query matches per `filePath`. A cancelled call is an error.
    pub fn count_by_book_user_regex(
        &self,
        raw: String,
        facets: Vec<String>,
        cancellation: &UserRegexCancellationToken,
    ) -> Result<BookCountResult> {
        let regex = Self::user_regex(&raw)?;
        let searcher = self.index_reader.searcher();
        if regex.kind == crate::user_regex::RegexKind::Line {
            let outcome = self.user_regex_line_outcome(&searcher, &regex, &facets, cancellation)?;
            return Ok(BookCountResult {
                counts: outcome.book_counts.clone(),
                truncated: outcome.truncated,
            });
        }
        let (query, truncated, _) =
            self.user_regex_term_query(&searcher, &regex, &facets, &cancellation.flag)?;
        let counts = Self::user_regex_checked(
            &cancellation.flag,
            searcher.search(&*query, &BookCountCollector),
        )?;
        Ok(BookCountResult { counts, truncated })
    }

    /// Per-child facet counts under `facet_prefix` of a `/…/` query (see
    /// [`Self::get_facet_counts`] on the dimension roots). `facets` filters, as everywhere:
    /// one book's path is a search in that book. A cancelled call is an error.
    pub fn get_facet_counts_user_regex(
        &self,
        raw: String,
        facets: Vec<String>,
        facet_prefix: String,
        cancellation: &UserRegexCancellationToken,
    ) -> Result<FacetCountsResult> {
        let regex = Self::user_regex(&raw)?;
        let searcher = self.index_reader.searcher();
        let (query, truncated): (Box<dyn Query>, bool) = if regex.kind
            == crate::user_regex::RegexKind::Line
        {
            let outcome = self.user_regex_line_outcome(&searcher, &regex, &facets, cancellation)?;
            (
                Box::new(crate::user_regex::HitSetQuery::new(&searcher, &outcome)),
                outcome.truncated,
            )
        } else {
            let (query, truncated, _) =
                self.user_regex_term_query(&searcher, &regex, &facets, &cancellation.flag)?;
            (query, truncated)
        };
        let mut collector = FacetCollector::for_field("topics");
        collector.add_facet(&facet_prefix);
        let counts =
            Self::user_regex_checked(&cancellation.flag, searcher.search(&*query, &collector))?;
        Ok(FacetCountsResult {
            counts: counts
                .get(facet_prefix.as_str())
                .map(|(f, count)| FacetCount {
                    path: f.to_string(),
                    count,
                })
                .collect(),
            truncated,
        })
    }

    fn user_regex(raw: &str) -> Result<crate::user_regex::UserRegex> {
        crate::user_regex::parse(raw)
            .map_err(|reason| anyhow::anyhow!("invalid user regex: {reason}"))
    }

    fn user_regex_checked<T>(
        cancel: &std::sync::atomic::AtomicBool,
        result: tantivy::Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            !cancel.load(Ordering::Relaxed),
            crate::user_regex::CANCELLED
        );
        Ok(result?)
    }

    fn user_regex_line_search<'a>(
        &'a self,
        searcher: &'a Searcher,
        facets: &[String],
        cancel: &'a std::sync::atomic::AtomicBool,
    ) -> Result<crate::user_regex::LineSearch<'a>> {
        let (order_epoch, order) = self.order_tables()?;
        Ok(crate::user_regex::LineSearch {
            searcher,
            schema: &self.schema,
            scope: (!facets.is_empty())
                .then(|| self.facet_filter_query(facets))
                .transpose()?,
            facets: facets.to_vec(),
            cancel,
            max_hits: crate::user_regex::MAX_LINE_HITS,
            order,
            order_epoch,
        })
    }

    fn user_regex_line_outcome(
        &self,
        searcher: &Searcher,
        regex: &crate::user_regex::UserRegex,
        facets: &[String],
        cancellation: &UserRegexCancellationToken,
    ) -> Result<Arc<crate::user_regex::LineOutcome>> {
        let ctx = self.user_regex_line_search(searcher, facets, &cancellation.flag)?;
        let outcome = crate::user_regex::run_line_search(&ctx, regex, None, &mut |_| true)?;
        anyhow::ensure!(!cancellation.is_cancelled(), crate::user_regex::CANCELLED);
        outcome.ok_or_else(|| anyhow::anyhow!(crate::user_regex::CANCELLED))
    }

    /// The query of a term pattern: one word as its term set, a sequence as an adjacent
    /// phrase of term sets that may continue onto the next line. Also returns whether a
    /// budget truncated a term set, and the sets (for highlighting).
    fn user_regex_term_query(
        &self,
        searcher: &Searcher,
        regex: &crate::user_regex::UserRegex,
        facets: &[String],
        cancel: &Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<UserRegexTermQuery> {
        let text_field = self.schema.get_field("text")?;
        let single = regex.parts.len() == 1;
        let (max_terms, budget) = if single {
            (USER_REGEX_WORD_MAX_TERMS, SINGLE_WORD_POSTINGS_BUDGET)
        } else {
            (
                PHRASE_POSITION_MAX_EXPANSIONS,
                PHRASE_POSITION_POSTINGS_BUDGET,
            )
        };
        let mut truncated = false;
        let mut sets = Vec::with_capacity(regex.parts.len());
        for part in &regex.parts {
            let found = crate::user_regex::collect_part_terms(
                searcher, text_field, part, max_terms, budget, cancel,
            )?;
            truncated |= found.truncated;
            sets.push(found.terms);
        }
        let facets_query = (!facets.is_empty())
            .then(|| self.facet_filter_query(facets))
            .transpose()?;
        let terms_of = |set: &Vec<String>| -> Vec<Term> {
            set.iter()
                .map(|t| Term::from_field_text(text_field, t))
                .collect()
        };
        let main: Box<dyn Query> = if sets.iter().any(|s| s.is_empty()) {
            Box::new(EmptyQuery)
        } else if single {
            Box::new(TermSetQuery::new(terms_of(&sets[0])))
        } else {
            let n = sets.len();
            let inline: Box<dyn Query> = Box::new(TermListPhraseQuery::new(
                text_field,
                sets.iter().map(|s| terms_of(s)).collect(),
                Proximity::adjacent(n),
            ));
            let words: Vec<cross_line::CrossLineWord> = sets
                .iter()
                .map(|s| cross_line::CrossLineWord::Terms(s.as_ref().clone()))
                .collect();
            self.with_cross_line(
                inline,
                text_field,
                &words,
                &vec![0; n - 1],
                facets_query.as_deref(),
            )?
        };
        let query: Box<dyn Query> = match facets_query {
            Some(facets_query) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, main),
                (Occur::Must, facets_query),
            ])),
            None => main,
        };
        Ok((
            crate::lexical_cancellation::LexicalFlag::shared(cancel).wrap(query),
            truncated,
            sets,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(any(feature = "flutter-bridge", test))]
    fn user_regex_term_stream(
        &self,
        searcher: &Searcher,
        regex: &crate::user_regex::UserRegex,
        facets: &[String],
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        chunk_size: u32,
        cancel: &Arc<std::sync::atomic::AtomicBool>,
        emit: &mut dyn FnMut(UserRegexStreamUpdate) -> bool,
    ) -> Result<()> {
        use std::sync::atomic::Ordering;
        let kind = regex.kind.api();
        let event = |results: Vec<SearchResult>,
                     counts: Option<(u32, &HashMap<String, u32>)>,
                     truncated: bool,
                     done: bool,
                     cancelled: bool| UserRegexStreamUpdate {
            kind,
            match_ranges: results
                .iter()
                .map(|_| UserRegexLineMatches { ranges: Vec::new() })
                .collect(),
            results,
            total_count: counts.map(|(total, _)| total),
            book_counts: counts.map(|(_, by_book)| by_book.clone()),
            found_so_far: counts.map_or(0, |(total, _)| total),
            progress_done: 0,
            progress_total: 0,
            truncated,
            done,
            cancelled,
        };
        let built = self.user_regex_term_query(searcher, regex, facets, cancel);
        if cancel.load(Ordering::Relaxed) {
            emit(event(Vec::new(), None, false, true, true));
            return Ok(());
        }
        let (query, truncated, sets) = built?;
        let text_field = self.schema.get_field("text")?;
        let top = TopDocs::with_limit(limit.max(1) as usize).and_offset(offset as usize);
        let collected = match self.order_sort(order)?.1 {
            Some(sort) => {
                let top = top.order_by((sort, Order::Asc));
                searcher
                    .search(&*query, &(top, Count, BookCountCollector))
                    .map(|(docs, count, by_book)| {
                        (
                            docs.into_iter().map(|(_, a)| a).collect::<Vec<_>>(),
                            count,
                            by_book,
                        )
                    })
            }
            None => {
                let top = top.order_by_score();
                searcher
                    .search(&*query, &(top, Count, BookCountCollector))
                    .map(|(docs, count, by_book)| {
                        (docs.into_iter().map(|(_, a)| a).collect(), count, by_book)
                    })
            }
        };
        if cancel.load(Ordering::Relaxed) {
            emit(event(Vec::new(), None, false, true, true));
            return Ok(());
        }
        let (addresses, total, by_book) = collected?;
        let addresses = if limit == 0 { Vec::new() } else { addresses };
        let total = total as u32;
        if !emit(event(
            Vec::new(),
            Some((total, &by_book)),
            truncated,
            false,
            false,
        )) {
            return Ok(());
        }
        if !addresses.is_empty() && !cancel.load(Ordering::Relaxed) {
            let per_word: Vec<HashSet<String>> =
                sets.iter().map(|s| s.iter().cloned().collect()).collect();
            let plan = HighlightPlan {
                query: self.terms_query_from_word_sets(&per_word, text_field)?,
                phrase: (per_word.len() > 1).then(|| PhraseHighlight {
                    proximity: Proximity::adjacent(per_word.len()),
                    per_word_terms: per_word,
                    analyzer: "hebrew",
                    cross_line: true,
                    alternatives: Vec::new(),
                    allow_term_fallback: true,
                }),
            };
            let hl = HighlightConfig::default();
            let hl_query: &dyn Query = plan.query.as_deref().unwrap_or(query.as_ref());
            let generator = Self::make_snippet_generator(searcher, hl_query, text_field, &hl)?;
            for chunk in addresses.chunks(chunk_size.max(1) as usize) {
                if cancel.load(Ordering::Relaxed) {
                    emit(event(
                        Vec::new(),
                        Some((total, &by_book)),
                        truncated,
                        true,
                        true,
                    ));
                    return Ok(());
                }
                let results = Self::build_results_with_generator(
                    &self.schema,
                    searcher,
                    &generator,
                    text_field,
                    chunk.to_vec(),
                    &hl,
                    plan.phrase.as_ref(),
                )?;
                if cancel.load(Ordering::Relaxed) {
                    emit(event(
                        Vec::new(),
                        Some((total, &by_book)),
                        truncated,
                        true,
                        true,
                    ));
                    return Ok(());
                }
                if !emit(event(results, None, false, false, false)) {
                    return Ok(());
                }
            }
        }
        emit(event(
            Vec::new(),
            Some((total, &by_book)),
            truncated,
            true,
            cancel.load(Ordering::Relaxed),
        ));
        Ok(())
    }
}

// ── BookCountCollector ─────────────────────────────────────────────────────────

/// Counts matching documents grouped by `filePath` fast field.
/// Per-segment counts use term ordinals; strings are decoded only in harvest().
struct BookCountCollector;

struct BookCountSegmentCollector {
    str_col: Option<tantivy::columnar::StrColumn>,
    /// Indexed by term ordinal: a segment holds only thousands of paths.
    counts: Vec<u32>,
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
        let num_terms = str_col.as_ref().map_or(0, |col| col.num_terms());
        Ok(BookCountSegmentCollector {
            str_col,
            counts: vec![0; num_terms],
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
            if let Some(term_ord) = col.ords().first(doc_id) {
                self.counts[term_ord as usize] += 1;
            }
        }
    }

    fn harvest(self) -> tantivy::Result<HashMap<String, u32>> {
        let Some(col) = self.str_col else {
            return Ok(HashMap::new());
        };
        let hits: Vec<(u64, u32)> = (0u64..)
            .zip(self.counts)
            .filter(|&(_, count)| count > 0)
            .collect();
        let mut result = HashMap::with_capacity(hits.len());
        let mut counts = hits.iter().map(|&(_, count)| count);
        // One sequential pass over the dictionary: `ord_to_str` per ordinal
        // re-seeks the sstable and dominated the whole count.
        col.dictionary().sorted_ords_to_term_cb(
            hits.iter().map(|&(term_ord, _)| term_ord),
            |bytes| {
                let path = std::str::from_utf8(bytes).map_err(|_| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "Not valid utf-8")
                })?;
                result.insert(path.to_owned(), counts.next().unwrap_or(0));
                Ok(())
            },
        )?;
        Ok(result)
    }
}

// ── GroupCollector ─────────────────────────────────────────────────────────────

/// מפתח המיון של מסמך בקיבוץ, כ-u64 עולה: מפתח סדר הספרים (קטלוג/דורות), או
/// היפוך ביטי הציון (רלוונטיות — ציון BM25 אי-שלילי, כך שהיפוך הביטים הופך
/// "ציון גבוה" ל"ערך נמוך" והמיון נשאר עולה).
#[derive(Clone)]
enum GroupSort {
    Book(Arc<OrderTables>, OrderKind),
    /// Equal scores, which are common, go by the catalogue.
    ScoreDesc(Arc<OrderTables>),
}

/// `(sort_val, tie, address, id)`: `tie` is unique per document and breaks ties
/// deterministically; `id` names the representative.
type GroupEntry = (u64, u64, DocAddress, u64);

/// צבירת קבוצה אחת: מונה, הנציג הטוב ביותר, ועד [`MERGED_SIBLINGS_CAP`]
/// חברות נוספות ממוינות לפי אותו סדר.
struct GroupAcc {
    count: u32,
    best: GroupEntry,
    siblings: Vec<GroupEntry>,
}

impl GroupAcc {
    fn new(entry: GroupEntry) -> Self {
        Self {
            count: 1,
            best: entry,
            siblings: Vec::new(),
        }
    }

    fn push(&mut self, mut entry: GroupEntry) {
        self.count += 1;
        if (entry.0, entry.1) < (self.best.0, self.best.1) {
            std::mem::swap(&mut self.best, &mut entry);
        }
        Self::insert_capped(&mut self.siblings, entry);
    }

    fn insert_capped(siblings: &mut Vec<GroupEntry>, entry: GroupEntry) {
        let pos = siblings.partition_point(|s| (s.0, s.1) <= (entry.0, entry.1));
        if pos < MERGED_SIBLINGS_CAP {
            siblings.insert(pos, entry);
            siblings.truncate(MERGED_SIBLINGS_CAP);
        }
    }
}

/// מפת קבוצות חסומת-[`GROUP_COLLECTOR_MAX_GROUPS`] שמפנה בתקרה את הקבוצה
/// *הגרועה* (לפי הנציג הטוב ביותר) לטובת קבוצה טובה ממנה — כך שהקבוצות
/// הטובות ביותר שורדות בכל סדר סריקה/מיזוג, והעמוד המוחזר מדויק. השארית
/// לאחר תקרה: `group_count` הוא תחתית, ומונה של קבוצה שפונתה וחזרה מאבד
/// את חבריה המוקדמים — שניהם תחת אותו דגל `truncated`.
struct BoundedGroups {
    groups: HashMap<(u8, u64), GroupAcc>,
    /// אינדקס המיון: `(sort_val, id)` של הנציג הטוב + מפתח הקבוצה.
    by_best: std::collections::BTreeSet<(u64, u64, (u8, u64))>,
    truncated: bool,
}

impl BoundedGroups {
    fn new() -> Self {
        Self {
            groups: HashMap::new(),
            by_best: std::collections::BTreeSet::new(),
            truncated: false,
        }
    }

    /// מסמך בודד (מסלול האיסוף פר-סגמנט).
    fn add_entry(&mut self, key: (u8, u64), entry: GroupEntry) {
        if let Some(acc) = self.groups.get_mut(&key) {
            let old_best = (acc.best.0, acc.best.1, key);
            acc.push(entry);
            let new_best = (acc.best.0, acc.best.1, key);
            if new_best != old_best {
                self.by_best.remove(&old_best);
                self.by_best.insert(new_best);
            }
            return;
        }
        self.insert_group(key, GroupAcc::new(entry));
    }

    fn insert_group(&mut self, key: (u8, u64), acc: GroupAcc) {
        let best = (acc.best.0, acc.best.1, key);
        if self.groups.len() >= GROUP_COLLECTOR_MAX_GROUPS {
            self.truncated = true;
            let worst = *self
                .by_best
                .last()
                .expect("cap > 0, so a full map has a worst group");
            if best >= worst {
                return;
            }
            self.by_best.remove(&worst);
            self.groups.remove(&worst.2);
        }
        self.by_best.insert(best);
        self.groups.insert(key, acc);
    }
}

/// פרי החיפוש המקובץ: ספירת התוצאות הגולמית + מפת הקבוצות. מפתח קבוצה
/// הוא `(תג, ערך)` — תג 0 = `sectionId`, תג 1 = `lineHash`, תג 2 =
/// מסמך-יחיד (שורה ללא חתימת דה-דופ במצב `IdenticalText` לעולם אינה
/// מתאחדת, וה-id שלה משמש כמפתח).
///
/// `truncated` — [`GROUP_COLLECTOR_MAX_GROUPS`] נפגעה וקבוצות גרועות
/// פונו (ראו [`BoundedGroups`]); `raw_total` נשאר מדויק.
struct GroupedHits {
    raw_total: u32,
    groups: HashMap<(u8, u64), GroupAcc>,
    truncated: bool,
}

/// נציג קבוצה בעמוד סופי, אחרי מיון וחיתוך `limit`/`offset`.
#[derive(Clone)]
struct GroupedRep {
    id: u64,
    address: DocAddress,
    count: u32,
    siblings: Vec<DocAddress>,
}

/// עמוד קבוצות סופי — נציגים + ספירות. `truncated` כב-[`GroupedHits`]:
/// תקרת הקבוצות נפגעה, `group_count` הוא תחתית ולא הערך המלא.
#[derive(Clone)]
struct GroupedPage {
    reps: Vec<GroupedRep>,
    group_count: u32,
    raw_total: u32,
    truncated: bool,
}

#[cfg(any(feature = "flutter-bridge", test))]
struct CallbackSink<'a, T>(&'a mut dyn FnMut(T) -> bool);
#[cfg(any(feature = "flutter-bridge", test))]
impl<T> CallbackSink<'_, T> {
    fn add(&mut self, update: T) -> Result<(), ()> {
        if (self.0)(update) {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(any(feature = "flutter-bridge", test))]
fn result_chunks<T>(items: &[T], chunk_size: usize) -> impl Iterator<Item = &[T]> {
    let first_size = chunk_size.max(1);
    #[cfg(test)]
    let first_size = std::env::var("OTZARIA_PERF_FIRST_CHUNK")
        .ok()
        .and_then(|size| size.parse::<usize>().ok())
        .map_or(first_size, |size| size.clamp(1, chunk_size.max(1)));
    let (first, rest) = items.split_at(items.len().min(first_size));
    std::iter::once(first)
        .filter(|chunk| !chunk.is_empty())
        .chain(rest.chunks(chunk_size.max(1)))
}

const GROUP_CACHE_BYTES: usize = 8 << 20;

type GroupCacheKey = (PageKey, u8, u8, u64);

struct GroupCache(
    Mutex<LruCache<GroupCacheKey, Arc<GroupedPage>>>,
    std::sync::atomic::AtomicU64,
);

#[frb(ignore)]
impl Default for GroupCache {
    fn default() -> Self {
        Self(
            Mutex::new(LruCache::new(NonZeroUsize::new(4).unwrap())),
            std::sync::atomic::AtomicU64::default(),
        )
    }
}

impl GroupCache {
    fn get(&self, key: &GroupCacheKey) -> Option<Arc<GroupedPage>> {
        self.0.lock().unwrap().get(key).cloned()
    }

    fn put(&self, key: GroupCacheKey, page: Arc<GroupedPage>) {
        let bytes = |page: &GroupedPage| {
            page.reps.capacity() * std::mem::size_of::<GroupedRep>()
                + page
                    .reps
                    .iter()
                    .map(|rep| rep.siblings.capacity() * std::mem::size_of::<DocAddress>())
                    .sum::<usize>()
        };
        if bytes(&page) > GROUP_CACHE_BYTES {
            return;
        }
        let mut cache = self.0.lock().unwrap();
        if key.3 != self.1.load(Ordering::Acquire) {
            return;
        }
        cache.put(key, page);
        while cache.iter().map(|(_, page)| bytes(page)).sum::<usize>() > GROUP_CACHE_BYTES {
            cache.pop_lru();
        }
    }

    fn clear(&self) {
        let mut cache = self.0.lock().unwrap();
        self.1.fetch_add(1, Ordering::AcqRel);
        cache.clear();
    }
}

impl GroupedPage {
    fn slice(&self, limit: u32, offset: u32) -> Self {
        Self {
            reps: self
                .reps
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .cloned()
                .collect(),
            group_count: self.group_count,
            raw_total: self.raw_total,
            truncated: self.truncated,
        }
    }
}

/// שדות ה-doc-store שחברת קבוצה נושאת ([`MergedSibling`]) — נפתרים מהסכימה
/// פעם אחת לעמוד ולא פר-sibling.
struct SiblingFields {
    title: Field,
    reference: Field,
    id: Field,
    segment: Field,
    is_pdf: Field,
    file_path: Field,
}

impl SiblingFields {
    fn resolve(schema: &Schema) -> Result<Self> {
        Ok(Self {
            title: schema.get_field("title")?,
            reference: schema.get_field("reference")?,
            id: schema.get_field("id")?,
            segment: schema.get_field("segment")?,
            is_pdf: schema.get_field("isPdf")?,
            file_path: schema.get_field("filePath")?,
        })
    }
}

/// גודל ה-buffer הפר-סגמנטי שמרווח את הנעילה על הצבירה המשותפת —
/// נעילה אחת לכל ~4K מסמכים תואמים במקום נעילה פר-מסמך.
const GROUP_FLUSH_BUFFER: usize = 4_096;

/// הצבירה המשותפת של החיפוש המקובץ: מפה חסומה **אחת לכל החיפוש**.
/// צבירה פר-סגמנט (עם מיזוג ב-merge_fruits) הייתה מחזיקה עד
/// [`GROUP_COLLECTOR_MAX_GROUPS`] קבוצות לכל סגמנט בו-זמנית — עשרות
/// סגמנטים (אינדקס לא ממוזג) היו מצטברים למאות MB לפני המיזוג.
struct SharedGroupedAcc {
    raw_total: u32,
    groups: BoundedGroups,
}

/// אוסף את כל המסמכים התואמים לקבוצות (ראו [`ResultGrouping`]) במעבר
/// אחד, עם נציג-מיטבי וחברות קצוצות-תקרה לכל קבוצה. הזיכרון פרופורציונלי
/// למספר הקבוצות — וזה *אינו* חסום בתקציב ה-postings: המסלולים exact
/// הלא-מנוקד (TermQuery/PhraseQuery) ו-fuzzy (תקציבי האוטומטון חוסמים
/// מספר מונחים, לא מספר מסמכים תואמים) יכולים לעבור על מיליוני שורות.
/// לכן מספר הקבוצות קצוץ גלובלית ב-[`GROUP_COLLECTOR_MAX_GROUPS`] עם
/// degrade מדווח, לא צבירה בלתי-חסומה. התוצאה נשלפת ב-[`Self::take_hits`]
/// אחרי `searcher.search` (ה-Fruit ריק).
struct GroupCollector {
    mode: ResultGrouping,
    sort: GroupSort,
    shared: Arc<Mutex<SharedGroupedAcc>>,
}

impl GroupCollector {
    fn new(grouping: &ResultGrouping, sort: GroupSort) -> Self {
        Self {
            mode: *grouping,
            sort,
            shared: Arc::new(Mutex::new(SharedGroupedAcc {
                raw_total: 0,
                groups: BoundedGroups::new(),
            })),
        }
    }

    /// שולף את התוצאה שנצברה — לקריאה פעם אחת, אחרי `searcher.search`.
    fn take_hits(&self) -> GroupedHits {
        let mut acc = self.shared.lock().expect("group accumulator poisoned");
        GroupedHits {
            raw_total: acc.raw_total,
            truncated: acc.groups.truncated,
            groups: std::mem::take(&mut acc.groups.groups),
        }
    }
}

enum GroupKeyColumn {
    Section(SectionIdColumn),
    LineHash(tantivy::columnar::Column<u64>),
}

impl GroupKeyColumn {
    fn first(&self, doc: DocId) -> Option<u64> {
        match self {
            Self::Section(column) => column.first(doc),
            Self::LineHash(column) => column.first(doc),
        }
    }
}

struct GroupSegmentCollector {
    seg_ord: SegmentOrdinal,
    mode: ResultGrouping,
    sort: GroupSort,
    id_col: tantivy::columnar::Column<u64>,
    /// עמודת מפתח הסדר; `None` במיון לפי ציון.
    sort_col: Option<BookOrderColumn>,
    /// עמודת מפתח הקבוצה: `sectionId` או `lineHash` לפי המצב.
    key_col: GroupKeyColumn,
    shared: Arc<Mutex<SharedGroupedAcc>>,
    buffer: Vec<((u8, u64), GroupEntry)>,
}

impl GroupSegmentCollector {
    fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let mut acc = self.shared.lock().expect("group accumulator poisoned");
        acc.raw_total = acc.raw_total.saturating_add(self.buffer.len() as u32);
        for (key, entry) in self.buffer.drain(..) {
            acc.groups.add_entry(key, entry);
        }
    }
}

impl Collector for GroupCollector {
    type Fruit = ();
    type Child = GroupSegmentCollector;

    fn for_segment(
        &self,
        seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<GroupSegmentCollector> {
        let fast = reader.fast_fields();
        let id_col = fast.u64("id")?;
        let sort_col = match &self.sort {
            GroupSort::Book(tables, kind) => {
                Some(BookOrderColumn::open(reader, Arc::clone(tables), *kind)?)
            }
            GroupSort::ScoreDesc(_) => None,
        };
        let key_col = match self.mode {
            ResultGrouping::SameSection => GroupKeyColumn::Section(SectionIdColumn::open(reader)?),
            ResultGrouping::IdenticalText => GroupKeyColumn::LineHash(fast.u64("lineHash")?),
        };
        Ok(GroupSegmentCollector {
            seg_ord,
            mode: self.mode,
            sort: self.sort.clone(),
            id_col,
            sort_col,
            key_col,
            shared: Arc::clone(&self.shared),
            buffer: Vec::with_capacity(GROUP_FLUSH_BUFFER),
        })
    }

    fn requires_scoring(&self) -> bool {
        matches!(self.sort, GroupSort::ScoreDesc(_))
    }

    fn merge_fruits(&self, _per_segment: Vec<()>) -> tantivy::Result<()> {
        Ok(())
    }
}

impl SegmentCollector for GroupSegmentCollector {
    type Fruit = ();

    fn collect(&mut self, doc_id: DocId, score: Score) {
        let id = self.id_col.first(doc_id).unwrap_or_default();
        let (sort_val, tie) = match &self.sort {
            GroupSort::Book(..) => (
                self.sort_col
                    .as_ref()
                    .and_then(|c| c.first(doc_id))
                    .unwrap_or(u64::MAX),
                id,
            ),
            GroupSort::ScoreDesc(tables) => (
                u64::from(!score.to_bits()),
                tables.key(OrderKind::Catalogue, id),
            ),
        };
        let key = match self.mode {
            ResultGrouping::SameSection => (0u8, self.key_col.first(doc_id).unwrap_or(id)),
            ResultGrouping::IdenticalText => {
                let hash = self.key_col.first(doc_id).unwrap_or(0);
                if hash == 0 {
                    (2u8, id)
                } else {
                    (1u8, hash)
                }
            }
        };
        let entry: GroupEntry = (sort_val, tie, DocAddress::new(self.seg_ord, doc_id), id);
        self.buffer.push((key, entry));
        if self.buffer.len() >= GROUP_FLUSH_BUFFER {
            self.flush();
        }
    }

    fn harvest(mut self) {
        self.flush();
    }
}

// ── BookFingerprintCollector ───────────────────────────────────────────────────

/// Collects a u64 fast-field value (`hash_column`) per `filePath` fast field.
/// Per-segment work uses term ordinals; strings are decoded only in harvest().
/// Only the document [`stamp_book`] wrote holds a book's value: a book
/// whose documents hold none is 0, and two that disagree collapse to 0
/// ("unverifiable"), which wins over any value when merging segments.
struct BookFingerprintCollector {
    hash_column: &'static str,
}

struct BookFingerprintSegmentCollector {
    str_col: Option<tantivy::columnar::StrColumn>,
    hash_col: Option<tantivy::columnar::Column<u64>>,
    fingerprints: HashMap<u64, Option<u64>>,
}

/// One more of a book's fingerprint values, `None` for a document without one:
/// `Some(0)` once two values disagree.
pub(crate) fn combine_fingerprint(seen: Option<u64>, value: Option<u64>) -> Option<u64> {
    match (seen, value) {
        (seen, None) => seen,
        (None, value) => value,
        (Some(a), Some(b)) if a == b => Some(a),
        _ => Some(0),
    }
}

impl Collector for BookFingerprintCollector {
    type Fruit = HashMap<String, u64>;
    type Child = BookFingerprintSegmentCollector;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<BookFingerprintSegmentCollector> {
        let str_col = reader.fast_fields().str("filePath")?;
        // אינדקסים מלפני הוספת השדה: אין עמודה — כל הספרים "לא ניתנים לאימות".
        let hash_col = reader.fast_fields().u64(self.hash_column).ok();
        Ok(BookFingerprintSegmentCollector {
            str_col,
            hash_col,
            fingerprints: HashMap::new(),
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(
        &self,
        per_segment: Vec<tantivy::Result<HashMap<String, Option<u64>>>>,
    ) -> tantivy::Result<HashMap<String, u64>> {
        let mut merged: HashMap<String, Option<u64>> = HashMap::new();
        for seg_result in per_segment {
            for (path, hash) in seg_result? {
                let seen = merged.entry(path).or_default();
                *seen = combine_fingerprint(*seen, hash);
            }
        }
        Ok(merged
            .into_iter()
            .map(|(path, hash)| (path, hash.unwrap_or(0)))
            .collect())
    }
}

// ── SingleBookFingerprintCollector ────────────────────────────────────────────

/// The `textHash` of the documents a single-book query matched, read from those
/// that hold one. Values that disagree collapse to 0 ("unverifiable"), and 0 wins
/// over any value — same semantics as [`BookFingerprintCollector`], without
/// touching `filePath`: the query already restricts the documents to one book.
struct SingleBookFingerprintCollector;

struct SingleBookFingerprintSegmentCollector {
    hash_col: Option<tantivy::columnar::Column<u64>>,
    fingerprint: Option<u64>,
}

impl Collector for SingleBookFingerprintCollector {
    type Fruit = u64;
    type Child = SingleBookFingerprintSegmentCollector;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<SingleBookFingerprintSegmentCollector> {
        Ok(SingleBookFingerprintSegmentCollector {
            // אינדקסים מלפני הוספת השדה: אין עמודה — "לא ניתן לאימות".
            hash_col: reader.fast_fields().u64("textHash").ok(),
            fingerprint: None,
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, per_segment: Vec<Option<u64>>) -> tantivy::Result<u64> {
        Ok(per_segment
            .into_iter()
            .fold(None, combine_fingerprint)
            .unwrap_or(0))
    }
}

impl SegmentCollector for SingleBookFingerprintSegmentCollector {
    type Fruit = Option<u64>;

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        let hash = self.hash_col.as_ref().and_then(|col| col.first(doc_id));
        self.fingerprint = combine_fingerprint(self.fingerprint, hash);
    }

    fn harvest(self) -> Option<u64> {
        self.fingerprint
    }
}

impl SegmentCollector for BookFingerprintSegmentCollector {
    type Fruit = tantivy::Result<HashMap<String, Option<u64>>>;

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        let Some(str_col) = &self.str_col else {
            return;
        };
        let Some(term_ord) = str_col.term_ords(doc_id).next() else {
            return;
        };
        let hash = self.hash_col.as_ref().and_then(|col| col.first(doc_id));
        let seen = self.fingerprints.entry(term_ord).or_default();
        *seen = combine_fingerprint(*seen, hash);
    }

    fn harvest(self) -> tantivy::Result<HashMap<String, Option<u64>>> {
        let Some(col) = self.str_col else {
            return Ok(HashMap::new());
        };
        let mut result = HashMap::with_capacity(self.fingerprints.len());
        let mut buf = String::new();
        for (term_ord, hash) in self.fingerprints {
            buf.clear();
            if col.ord_to_str(term_ord, &mut buf)? {
                result.insert(buf.clone(), hash);
            }
        }
        Ok(result)
    }
}

/// Physical segment size, including stored text and postings. Optional
/// components (notably the delete file) are absent on healthy segments.
fn segment_disk_bytes(index_path: &Path, meta: &SegmentMeta) -> Result<u64> {
    let mut bytes = 0u64;
    for file in meta.list_files() {
        match fs::metadata(index_path.join(&file)) {
            Ok(metadata) => bytes = bytes.saturating_add(metadata.len()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("reading segment file {file:?}")),
        }
    }
    Ok(bytes)
}

fn sized_segment_metas(
    index_path: &Path,
    metas: Vec<SegmentMeta>,
) -> Result<Vec<(u64, SegmentMeta)>> {
    metas
        .into_iter()
        .map(|meta| Ok((segment_disk_bytes(index_path, &meta)?, meta)))
        .collect()
}

/// The normal writer must obey the same size rule as manual optimize; Tantivy's
/// default LogMergePolicy can otherwise rewrite eight existing segments during
/// the commit that adds one tiny book (its default minimum is eight segments).
#[derive(Debug)]
struct SizeAwareMergePolicy {
    index_path: PathBuf,
}

impl MergePolicy for SizeAwareMergePolicy {
    fn compute_merge_candidates(&self, metas: &[SegmentMeta]) -> Vec<MergeCandidate> {
        if metas.len() <= MAX_SEGMENTS_AFTER_OPTIMIZE {
            return Vec::new();
        }
        let sized = match sized_segment_metas(&self.index_path, metas.to_vec()) {
            Ok(sized) => sized,
            Err(err) => {
                warn!("background merge: cannot measure segment sizes: {err:#}");
                return Vec::new();
            }
        };
        select_smallest_level(&sized)
            .map(|ids| vec![MergeCandidate(ids)])
            .unwrap_or_default()
    }
}

/// Pick one merge at a time. Reclaim a delete-heavy segment on its own, then
/// combine the smallest healthy segments only if their byte sizes are close.
fn select_segments_to_compact(sized_metas: &[(u64, SegmentMeta)]) -> Option<Vec<SegmentId>> {
    if let Some((_, meta)) = sized_metas
        .iter()
        .filter(|(_, meta)| {
            meta.max_doc() > 0
                && f64::from(meta.num_deleted_docs()) / f64::from(meta.max_doc())
                    > OPTIMIZE_COMPACT_DELETE_RATIO
        })
        .min_by_key(|(bytes, _)| *bytes)
    {
        return Some(vec![meta.id()]);
    }

    select_level_to_compact(sized_metas)
}

fn sorted_by_size(sized_metas: &[(u64, SegmentMeta)]) -> Vec<&(u64, SegmentMeta)> {
    let mut by_size: Vec<_> = sized_metas.iter().collect();
    by_size.sort_by_key(|(bytes, _)| *bytes);
    by_size
}

/// The segments, sorted by size, split into levels: a level begins at the
/// smallest segment not yet placed and holds everything within
/// MAX_COMPACT_SIZE_RATIO of it. Segments in one level are interchangeable as
/// far as write amplification goes; segments in different levels are not.
fn size_levels(by_size: &[&(u64, SegmentMeta)]) -> Vec<Range<usize>> {
    let mut levels = Vec::new();
    let mut start = 0;
    while start < by_size.len() {
        let ceiling = by_size[start].0.saturating_mul(MAX_COMPACT_SIZE_RATIO);
        let mut end = start + 1;
        while end < by_size.len() && by_size[end].0 <= ceiling {
            end += 1;
        }
        levels.push(start..end);
        start = end;
    }
    levels
}

/// Merging k segments retires k - 1 of them, so take no more of a level than
/// the distance to the target allows. When a level holds nearly the whole
/// index this still merges nearly the whole index into one dominant segment —
/// that is the cheapest way to reach the target in bytes written, and the
/// alternative is rewriting the same data over several rounds of the loop.
fn take_from_level(
    total: usize,
    by_size: &[&(u64, SegmentMeta)],
    level: Range<usize>,
) -> Vec<SegmentId> {
    let room = total - MAX_SEGMENTS_AFTER_OPTIMIZE + 1;
    let take = level.len().min(room);
    by_size[level.start..level.start + take]
        .iter()
        .map(|(_, meta)| meta.id())
        .collect()
}

/// What the writer merges on its own, in a background thread, while someone is
/// indexing: the smallest level only, and only when it has something to
/// combine. Nothing above that level is ever rewritten unasked — a user who
/// adds a book to a library of large segments would otherwise pay gigabytes of
/// rewriting for it, which is the property #21 added.
fn select_smallest_level(sized_metas: &[(u64, SegmentMeta)]) -> Option<Vec<SegmentId>> {
    if sized_metas.len() <= MAX_SEGMENTS_AFTER_OPTIMIZE {
        return None;
    }
    let by_size = sorted_by_size(sized_metas);
    let smallest = size_levels(&by_size).into_iter().next()?;
    (smallest.len() >= 2).then(|| take_from_level(sized_metas.len(), &by_size, smallest))
}

/// What `optimize` compacts when it is asked to, which is more than the
/// background policy above: the smallest level when it has two members, and
/// otherwise the first level over it that alone holds more segments than the
/// whole index is meant to end with. Measuring the ratio against the smallest
/// segment in the index instead — which is what this did — let one short final
/// flush of a from-scratch build veto the merging of every other segment with
/// its own kind, and left the index at 188 segments against a target of 8.
///
/// A level of eight or fewer above a singleton is still left alone, so an
/// index can settle above the target. That is deliberate: the alternative is
/// rewriting healthy large segments to retire one or two, and `optimize`
/// documents its target as soft for exactly that reason.
fn select_level_to_compact(sized_metas: &[(u64, SegmentMeta)]) -> Option<Vec<SegmentId>> {
    if sized_metas.len() <= MAX_SEGMENTS_AFTER_OPTIMIZE {
        return None;
    }
    let by_size = sorted_by_size(sized_metas);
    for (rank, level) in size_levels(&by_size).into_iter().enumerate() {
        let worth_rewriting = if rank == 0 {
            level.len() >= 2
        } else {
            level.len() > MAX_SEGMENTS_AFTER_OPTIMIZE
        };
        if worth_rewriting {
            return Some(take_from_level(sized_metas.len(), &by_size, level));
        }
    }
    None
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../search_quality_tests.rs"]
mod quality_tests;

#[cfg(test)]
#[path = "../quality_bench.rs"]
mod quality_bench;

#[cfg(test)]
#[path = "search_engine_tests.rs"]
mod tests;

#[cfg(all(test, feature = "flutter-bridge"))]
#[path = "search_responsiveness_tests.rs"]
mod search_responsiveness_tests;

#[cfg(test)]
#[path = "../corpus_performance.rs"]
mod corpus_performance;
