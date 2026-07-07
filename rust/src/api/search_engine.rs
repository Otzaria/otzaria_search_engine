use crate::frb_generated::StreamSink;
use anyhow::{Context, Result};
use flutter_rust_bridge::frb;
use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA, SINK_STATE};
use log::{debug, error, info, warn};
use lru::LruCache;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
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
use crate::gap_phrase::GapVerifiedPhraseQuery;
use crate::hebrew_query;
use crate::hebrew_query::VocalizedFlags;
use crate::hebrew_tokenizer::{HebrewTokenizer, VocalizedHebrewTokenizer};
use crate::lexicons::{
    AcronymLexicon, TranslationLexicon, MAX_ACRONYM_EXPANSIONS, MAX_TRANSLATION_EXPANSIONS,
};
use crate::magic::{MagicDictionary, MAX_LEXICAL_FORMS};
use crate::section_scope::{SectionFilteredQuery, SectionIdsCollector};

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
    /// Book-level content fingerprint (see [`compute_content_fingerprint`]).
    /// The same value is stamped on every document of a book, so
    /// [`SearchEngine::get_book_fingerprints`] can compare an index against the
    /// current library source. `None`/`0` means "no fingerprint recorded"
    /// (e.g. PDF books).
    pub content_hash: Option<u64>,
    /// הטקסט המנוקד של השורה (נרמול [`normalize_vocalized_text_for_indexing`])
    /// עבור השדה `textVocalized`. `None` לשורה ללא ניקוד/טעמים — השורה
    /// פשוט לא תשתתף בחיפוש מנוקד. מסלול [`SearchEngine::add_text_book`]
    /// מחשב זאת בעצמו; השדה קיים לצינורות שמוסיפים מסמכים מוכנים.
    pub text_vocalized: Option<String>,
    /// מזהה הסעיף (בלוק הכותרת) של השורה — ראו השדה `sectionId` בסכימה.
    /// `None` = השורה סעיף לעצמה (`id` משמש כמזהה), כך שחיפוש "תחת אותה
    /// כותרת" מתנהג כמו "באותה פסקה" עבור מסמכים שהוזנו בלי מזהה סעיף.
    pub section_id: Option<u64>,
    /// סדר הדור של הספר (נמוך = מוקדם). `None` ממוין לסוף הרשימה.
    pub generation_order: Option<u32>,
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
    /// [`SearchEngine::single_regex_term_query`]). Only the regex and advanced
    /// paths can degrade this way; the exact/fuzzy paths always report `false`.
    pub truncated: bool,
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
    /// result chunks and on the exact/fuzzy paths, which never degrade this way.
    /// The UI surfaces this as a "results may be partial — narrow the search"
    /// warning.
    pub truncated: bool,
}

pub struct FacetCount {
    pub path: String,
    pub count: u64,
}

/// A total hit count paired with the single-word truncation flag — the
/// status-bearing return of [`SearchEngine::count_with_status`] and its
/// advanced variant. `truncated` carries the same meaning as
/// [`SearchStreamUpdate::truncated`]: `true` when a broad single-word query
/// overflowed its collection budget, so `count` undercounts the true total.
pub struct CountResult {
    pub count: u32,
    pub truncated: bool,
}

/// Per-`filePath` live-document counts paired with the truncation flag — the
/// status-bearing return of [`SearchEngine::count_by_book_with_status`] and
/// its advanced variant. When `truncated`, the per-book counts are partial.
pub struct BookCountResult {
    pub counts: HashMap<String, u32>,
    pub truncated: bool,
}

/// Per-child facet counts paired with the truncation flag — the status-bearing
/// return of [`SearchEngine::get_facet_counts_with_status`] and its advanced
/// variant. When `truncated`, the facet counts are partial.
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

pub enum ResultsOrder {
    Catalogue,
    Relevance,
    Generation,
}

/// Per-word index-term sets plus the intermediate-word allowance, used by the
/// snippet renderer to keep only highlights that form an in-order *phrase*
/// occurrence.
///
/// tantivy's `SnippetGenerator` highlights term-by-term: it paints every
/// occurrence of every query term with no positional constraint. For the
/// multi-word phrase paths (exact `PhraseQuery`, advanced/lexical-fuzzy
/// `RegexPhraseQuery`) that over-paints — a lone "משה" lights up even when the
/// search matched only the adjacent phrase "משה ואהרן". This filter drops such
/// stray highlights so the snippet paints exactly what the search matched.
///
/// `gaps[i]` is the maximum number of intermediate index tokens allowed
/// between query words `i` and `i+1` — the same per-pair intermediate-word
/// model `display_highlight` and the engine's phrase verification use, so the
/// results snippet, the search, and the book agree.
struct PhraseHighlight {
    /// One index-term set per query word, in query (= phrase) order.
    per_word_terms: Vec<HashSet<String>>,
    /// Per adjacent pair; length `per_word_terms.len() - 1`.
    gaps: Vec<u32>,
    /// The analyzer that tokenizes snippet fragments for re-highlighting —
    /// must match the field the terms came from (`"hebrew"` for `text`,
    /// `"hebrew_vocalized"` for `textVocalized`).
    analyzer: &'static str,
}

/// What a highlight-query builder resolves to: the flat term query that drives
/// tantivy's fragment selection (and, absent a phrase filter, its
/// highlighting), plus the optional phrase constraint above.
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
/// resolved per-pair gap allowances (fold `custom_spacing`/`distance` in — the
/// phrase highlight filter's gap allowances), and whether single-word
/// collection truncated. The fifth element carries the acronym-expansion
/// alternatives ("ראשי תיבות") as per-word literal-pattern lists — the
/// highlight builders materialize their terms so a document matched through
/// an expansion (רמב"ם → "רבי משה בן מיימון") still gets its snippet painted.
type AdvancedQueryBuild = (
    Box<dyn Query>,
    Vec<String>,
    Vec<u32>,
    bool,
    Vec<Vec<String>>,
);

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
const INDEX_SCHEMA_VERSION: u32 = 3;
const TANTIVY_INDEX_VERSION: &str = "0.26.1";
const DEFAULT_GENERATION_ORDER: u32 = 5;
const GENERATION_SORT_SHIFT: u32 = 56;
const GENERATION_SORT_ID_MASK: u64 = (1u64 << GENERATION_SORT_SHIFT) - 1;

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
const FUZZY_BOOST_EXACT_REL: Score = 1.0;
const FUZZY_BOOST_LEXICAL: Score = 30.0;
const FUZZY_BOOST_FUZZY: Score = 1.0;

/// The schema fields resolved together by [`SearchEngine::all_fields`]:
/// `(title, reference, text, id, segment, isPdf, filePath, topics,
/// contentHash, textVocalized, sectionId, generationSort)`.
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
    Field,
);

fn generation_sort_key(generation_order: u32, id: u64) -> u64 {
    (u64::from(generation_order.min(255)) << GENERATION_SORT_SHIFT) | (id & GENERATION_SORT_ID_MASK)
}

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
/// source text. Stamp it on every [`DocumentInput`] of the book at indexing
/// time; recompute it from the current library source and compare against
/// [`SearchEngine::get_book_fingerprints`] to detect books whose content
/// changed without reindexing everything.
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

/// Borrowing form of [`compute_content_fingerprint`] so in-engine callers
/// ([`SearchEngine::add_text_book`]) never clone a whole book to hash it.
fn content_fingerprint(text: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    if hash == 0 {
        1
    } else {
        hash
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
fn stored_schema_mismatch(index_path: &Path) -> Option<String> {
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
        Ok(stored) if stored == current_schema() => None,
        Ok(_) => Some("tantivy schema on disk differs from the engine schema".to_string()),
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

    if let Some(reason) = stored_schema_mismatch(index_path) {
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
    // Deliberately NOT fast: a text fast field stores every raw line in a
    // columnar dictionary — a second full copy of the corpus — and nothing
    // reads it (collectors use only the filePath/contentHash/id columns).
    schema_builder.add_text_field(
        "text",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("hebrew")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored(),
    );
    // השדה המנוקד: מאוכלס רק בשורות שנושאות ניקוד/טעמים (שאר השורות פשוט
    // אינן קיימות בו — tantivy מטפל בשדה חסר בחינם). מאונדקס בטוקנייזר
    // ששומר את הסימנים, ומאוחסן כדי שתוצאות חיפוש מנוקד יציגו את הטקסט
    // המנוקד. חיפוש רגיל אינו נוגע בשדה הזה כלל.
    schema_builder.add_text_field(
        "textVocalized",
        TextOptions::default()
            .set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer("hebrew_vocalized")
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
            .set_stored(),
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
    // Book-level content fingerprint, stamped identically on all documents of
    // a book. FAST-only: read columnar by get_book_fingerprints, never searched
    // or returned. 0 = no fingerprint recorded.
    schema_builder.add_u64_field("contentHash", FAST);
    // מזהה סעיף: כל השורות שתחת אותה כותרת (אותו בלוק reference) נושאות
    // אותו ערך, ייחודי גלובלית — id_base של הספר + אינדקס בלוק הכותרת
    // (ב-PDF: מספר העמוד). FAST בלבד: מסלול "תחת אותה כותרת"
    // (SearchScope::SameSection) קורא אותו עמודתית לחיתוך סעיפים; אינו
    // מאוחסן ואינו מחופש ישירות.
    schema_builder.add_u64_field("sectionId", FAST);
    schema_builder.add_u64_field("generationSort", FAST);
    schema_builder.add_facet_field("topics", FacetOptions::default());
    schema_builder.build()
}

/// Cache key for materialized single-word term sets. The searcher
/// `generation_id` changes on every reader reload (i.e. after each commit),
/// so entries from a stale index snapshot can never be served.
#[derive(Hash, PartialEq, Eq)]
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

/// Ceiling on the summed per-segment `doc_freq` a single-word term set may
/// accumulate. This — not the term count — is the true cost guard: executing
/// the resulting `TermSetQuery` unions one postings list per matched term per
/// segment into a BitSet, O(Σ doc_freq). The term-count ceiling
/// (`max_expansions`) remains only as a memory guard on the materialized
/// `Vec<Term>`. Initial value pending empirical calibration via
/// `benchmark_cli` (VARIATION_CEILING_RESEARCH.md §3.א).
const SINGLE_WORD_POSTINGS_BUDGET: u64 = 1_000_000;

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
    /// Bulk-indexing mode (see [`SearchEngine::set_bulk_indexing`]): while
    /// on, the live writer (and any lazily-reopened one) uses `NoMergePolicy`.
    bulk_indexing: bool,
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
    #[frb(sync)]
    pub fn new(path: &str) -> Self {
        init_engine_logger();
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
        // אנליזטורי השדות (מצב אינדוקס): מילה עם גרש/גרשיים מוטמעת גם
        // בצורתה הנקייה באותה עמדה — חיפוש `רמבם` מוצא `רמב"ם`.
        index.tokenizers().register(
            "hebrew",
            TextAnalyzer::builder(HebrewTokenizer {
                emit_quote_free: true,
            })
            .filter(LowerCaser)
            .build(),
        );
        index.tokenizers().register(
            "hebrew_vocalized",
            TextAnalyzer::builder(VocalizedHebrewTokenizer {
                emit_quote_free: true,
            })
            .filter(LowerCaser)
            .build(),
        );
        // גרסאות צד-שאילתה: בלי הפליטה הכפולה — שאילתה מטוקננת לטוקן אחד
        // לכל מילה (מסלול ה-exact בונה PhraseQuery לפי מספר הטוקנים).
        index.tokenizers().register(
            "hebrew_query",
            TextAnalyzer::builder(HebrewTokenizer {
                emit_quote_free: false,
            })
            .filter(LowerCaser)
            .build(),
        );
        index.tokenizers().register(
            "hebrew_vocalized_query",
            TextAnalyzer::builder(VocalizedHebrewTokenizer {
                emit_quote_free: false,
            })
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
            translation_dict: None,
            acronym_dict: None,
            term_cache: Mutex::new(LruCache::new(
                NonZeroUsize::new(TERM_CACHE_ENTRIES).expect("cache size is non-zero"),
            )),
            bulk_indexing: false,
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

    /// טוען את מילון התרגום הארמי-עברי (ה-`dictionary.json` של האפליקציה)
    /// עבור אפשרות "תרגום ארמי" בחיפוש המתקדם. מחזיר `true` אם הקובץ נטען;
    /// `false` אם חסר/פגום — ואז האפשרות לא מרחיבה דבר (אין שגיאה כלפי
    /// האפליקציה, שיכולה לקרוא לזה ללא תנאי באתחול).
    #[frb(sync)]
    pub fn set_translation_dictionary_path(&mut self, path: String) -> bool {
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
    #[frb(sync)]
    pub fn has_translation_dictionary(&self) -> bool {
        self.translation_dict.is_some()
    }

    /// טוען את מילון ראשי-התיבות (ה-`Acronyms.json` של האפליקציה) עבור
    /// אפשרות "ראשי תיבות" בחיפוש המתקדם. מחזיר `true` אם הקובץ נטען;
    /// `false` אם חסר/פגום — ואז האפשרות לא מרחיבה דבר (אין שגיאה כלפי
    /// האפליקציה, שיכולה לקרוא לזה ללא תנאי באתחול).
    #[frb(sync)]
    pub fn set_acronyms_dictionary_path(&mut self, path: String) -> bool {
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
    #[frb(sync)]
    pub fn has_acronyms_dictionary(&self) -> bool {
        self.acronym_dict.is_some()
    }

    // ── Write API ──────────────────────────────────────────────────────────────

    /// Add a single document. Does not commit.
    /// Writes no content fingerprint (`contentHash` = 0) — batch ingestion via
    /// [`Self::add_documents_batch`] is the fingerprint-aware path.
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
        _section_id: Option<u64>,
        _generation_order: Option<u32>,
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
            content_hash_f,
            text_vocalized_f,
            section_id_f,
            generation_sort_f,
        ) = self.all_fields()?;
        let topics_facet = Facet::from_text(_topics)?;
        let mut document = doc!(
            title_f        => _title,
            reference_f    => _reference,
            text_f         => _text,
            id_f           => _id,
            segment_f      => _segment,
            is_pdf_f       => _is_pdf,
            file_path_f    => _file_path,
            topics_f       => topics_facet,
            content_hash_f => 0u64,
            section_id_f   => _section_id.unwrap_or(_id),
            generation_sort_f => generation_sort_key(
                _generation_order.unwrap_or(DEFAULT_GENERATION_ORDER),
                _id
            )
        );
        // שורה שנושאת סימנים משתתפת גם בחיפוש המנוקד; `_text` מגיע בדרך
        // כלל מנורמל (נטול סימנים) ואז אין תוספת.
        if hebrew_query::contains_attached_marks(_text) {
            document.add_text(
                text_vocalized_f,
                hebrew_query::normalize_vocalized_text_for_indexing(_text),
            );
        }
        self.writer_mut()?.add_document(document)?;
        Ok(())
    }

    /// Add many documents in a single FFI call. Does not commit.
    /// For initial bulk loads – no duplicate checking.
    pub fn add_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        let (
            title_f,
            reference_f,
            text_f,
            id_f,
            segment_f,
            is_pdf_f,
            file_path_f,
            topics_f,
            content_hash_f,
            text_vocalized_f,
            section_id_f,
            generation_sort_f,
        ) = self.all_fields()?;
        let writer = self.writer_mut()?;
        for doc in docs {
            let topics_facet = Facet::from_text(&doc.topics)?;
            let mut document = doc!(
                title_f        => doc.title,
                reference_f    => doc.reference,
                text_f         => doc.text,
                id_f           => doc.id,
                segment_f      => doc.segment,
                is_pdf_f       => doc.is_pdf,
                file_path_f    => doc.file_path,
                topics_f       => topics_facet,
                content_hash_f => doc.content_hash.unwrap_or(0),
                section_id_f   => doc.section_id.unwrap_or(doc.id),
                generation_sort_f => generation_sort_key(
                    doc.generation_order.unwrap_or(DEFAULT_GENERATION_ORDER),
                    doc.id
                )
            );
            if let Some(vocalized) = doc.text_vocalized {
                if !vocalized.is_empty() {
                    document.add_text(text_vocalized_f, vocalized);
                }
            }
            writer.add_document(document)?;
        }
        Ok(())
    }

    /// Indexes a whole text book in ONE FFI call. Does not commit.
    ///
    /// Splits `text` into lines, tracks the `<h…>` heading reference trail,
    /// normalizes each line ([`normalize_text_for_indexing`]), stamps the
    /// book's raw-text content fingerprint on every document, and adds one
    /// document per line. Returns the number of documents added (0 for empty
    /// text — the caller writes its empty-book marker in that case).
    ///
    /// This is the whole-book replacement for the app's per-line pipeline
    /// (Dart isolate → per-batch FFI normalize → SendPort copy → batch add):
    /// the raw text crosses the bridge exactly once and only a count comes
    /// back. Document ids encode catalogue order exactly like the Dart
    /// `buildCatalogueDocumentId`: `((catalogue_order+1) << 32) + ordinal+1`.
    pub fn add_text_book(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        catalogue_order: u32,
        generation_order: u32,
        text: String,
    ) -> Result<u32> {
        self.add_text_book_impl(
            title,
            topics,
            file_path,
            catalogue_order,
            generation_order,
            &text,
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
        catalogue_order: u32,
        generation_order: u32,
        text: Vec<u8>,
    ) -> Result<u32> {
        let text = match String::from_utf8_lossy(&text) {
            std::borrow::Cow::Borrowed(_) => {
                // Valid UTF-8 — take ownership without re-copying.
                unsafe { String::from_utf8_unchecked(text) }
            }
            std::borrow::Cow::Owned(fixed) => fixed,
        };
        self.add_text_book_impl(
            title,
            topics,
            file_path,
            catalogue_order,
            generation_order,
            &text,
        )
    }

    fn add_text_book_impl(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        catalogue_order: u32,
        generation_order: u32,
        text: &str,
    ) -> Result<u32> {
        if text.is_empty() {
            return Ok(0);
        }
        let started = Instant::now();
        let text_bytes = text.len();
        let (
            title_f,
            reference_f,
            text_f,
            id_f,
            segment_f,
            is_pdf_f,
            file_path_f,
            topics_f,
            content_hash_f,
            text_vocalized_f,
            section_id_f,
            generation_sort_f,
        ) = self.all_fields()?;
        let topics_facet = Facet::from_text(&topics)?;
        let content_hash = content_fingerprint(text);
        let id_base = (u64::from(catalogue_order) + 1) << 32;
        let writer = self.writer_mut()?;

        // "prepare" — the pure-CPU phase (trail + normalization); "enqueue" —
        // writer.add_document (queue push; grows only when tantivy's indexing
        // threads apply backpressure).
        let prepare_started = Instant::now();
        let lines: Vec<&str> = text.split('\n').collect();

        // Sequential cheap pass: the reference trail is stateful across
        // lines, so resolve each line to its reference index first. The
        // stripped trail is recomputed only when a heading changes it.
        let mut trail: Vec<&str> = Vec::new();
        let mut references: Vec<String> = vec![String::new()];
        let mut reference_of_line: Vec<u32> = Vec::with_capacity(lines.len());
        for raw_line in &lines {
            if raw_line.starts_with("<h") {
                update_reference_trail(&mut trail, raw_line);
                references.push(hebrew_query::strip_html_for_indexing(&trail.join(", ")));
            }
            reference_of_line.push((references.len() - 1) as u32);
        }

        // The expensive pass — normalization is order-independent across
        // lines, so it fans out over all cores. A line carrying nikud/taamim
        // also gets its vocalized rendering, for the `textVocalized` field
        // (the mark check is cheap and almost always short-circuits false).
        use rayon::prelude::*;
        let normalized: Vec<(String, Option<String>)> = lines
            .par_iter()
            .map(|raw_line| {
                let plain = hebrew_query::normalize_text_for_indexing(raw_line);
                let vocalized = hebrew_query::contains_attached_marks(raw_line)
                    .then(|| hebrew_query::normalize_vocalized_text_for_indexing(raw_line))
                    .filter(|v| !v.is_empty());
                (plain, vocalized)
            })
            .collect();
        let prepare_time = prepare_started.elapsed();

        let enqueue_started = Instant::now();
        let mut ordinal: u64 = 0;
        for (segment, (normalized_line, vocalized_line)) in normalized.into_iter().enumerate() {
            let reference = references[reference_of_line[segment] as usize].as_str();
            let id = id_base + ordinal + 1;
            let mut document = doc!(
                title_f        => title.as_str(),
                reference_f    => reference,
                text_f         => normalized_line,
                id_f           => id,
                segment_f      => segment as u64,
                is_pdf_f       => false,
                file_path_f    => file_path.as_str(),
                topics_f       => topics_facet.clone(),
                content_hash_f => content_hash,
                // כל השורות של אותו בלוק כותרת חולקות ערך; id_base מבדל
                // בין ספרים, אז המזהה ייחודי גלובלית.
                section_id_f   => id_base + u64::from(reference_of_line[segment]),
                generation_sort_f => generation_sort_key(generation_order, id)
            );
            if let Some(vocalized) = vocalized_line {
                document.add_text(text_vocalized_f, vocalized);
            }
            writer.add_document(document)?;
            ordinal += 1;
        }
        let enqueue_time = enqueue_started.elapsed();
        info!(
            "add_text_book '{title}': {ordinal} docs, {text_bytes} bytes in {:?} \
             (prepare {prepare_time:?}, enqueue {enqueue_time:?})",
            started.elapsed()
        );
        Ok(ordinal as u32)
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
    /// ids encoding catalogue order like [`Self::add_text_book`]. PDFs record
    /// no content fingerprint (`contentHash` = 0, their text is not in the
    /// library DB).
    ///
    /// Returns the number of documents added; 0 means the PDF yielded no
    /// usable text (scanned/garbage) — the caller falls back to a sidecar or
    /// writes its empty-book marker.
    pub fn add_pdf_book(
        &mut self,
        title: String,
        topics: String,
        file_path: String,
        catalogue_order: u32,
        generation_order: u32,
        pages: Vec<PdfPageInput>,
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
            content_hash_f,
            // PDF אינו משתתף בחיפוש מנוקד: ניקוד שמגיע מ-OCR אינו אמין,
            // והנרמול של PDF ממילא מוחק אותו.
            _text_vocalized_f,
            section_id_f,
            generation_sort_f,
        ) = self.all_fields()?;
        let topics_facet = Facet::from_text(&topics)?;
        let id_base = (u64::from(catalogue_order) + 1) << 32;
        let writer = self.writer_mut()?;

        // Normalization + garbage heuristic are per-line pure functions —
        // fan the whole book out over all cores, then enqueue sequentially.
        use rayon::prelude::*;
        let prepare_started = Instant::now();
        let lines: Vec<(usize, &str)> = pages
            .iter()
            .enumerate()
            .flat_map(|(page_idx, page)| page.text.split('\n').map(move |line| (page_idx, line)))
            .collect();
        let prepared: Vec<(usize, String, bool)> = lines
            .par_iter()
            .map(|(page_idx, raw_line)| {
                let normalized = hebrew_query::normalize_pdf_text_for_indexing(raw_line);
                let is_garbage = hebrew_query::is_probably_garbage_pdf_text(&normalized);
                (*page_idx, normalized, is_garbage)
            })
            .collect();
        let prepare_time = prepare_started.elapsed();

        let enqueue_started = Instant::now();
        let mut ordinal: u64 = 0;
        let mut garbage_lines: u64 = 0;
        for (page_idx, normalized, is_garbage) in prepared {
            if is_garbage {
                garbage_lines += 1;
                continue;
            }
            let page = &pages[page_idx];
            let id = id_base + ordinal + 1;
            writer.add_document(doc!(
                title_f        => title.as_str(),
                reference_f    => page.reference.as_str(),
                text_f         => normalized,
                id_f           => id,
                segment_f      => u64::from(page.page_index),
                is_pdf_f       => true,
                file_path_f    => file_path.as_str(),
                topics_f       => topics_facet.clone(),
                content_hash_f => 0u64,
                // ב-PDF אין שרשרת כותרות — עמוד = סעיף.
                section_id_f   => id_base + u64::from(page.page_index),
                generation_sort_f => generation_sort_key(generation_order, id)
            ))?;
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
        _section_id: Option<u64>,
        _generation_order: Option<u32>,
    ) -> Result<()> {
        self.delete_document_by_id(_id)?;
        self.add_document(
            _id,
            _title,
            _reference,
            _topics,
            _text,
            _segment,
            _is_pdf,
            _file_path,
            _section_id,
            _generation_order,
        )
    }

    /// Upsert many documents in a single FFI call. Does not commit.
    pub fn upsert_documents_batch(&mut self, docs: Vec<DocumentInput>) -> Result<()> {
        let (
            title_f,
            reference_f,
            text_f,
            id_f,
            segment_f,
            is_pdf_f,
            file_path_f,
            topics_f,
            content_hash_f,
            text_vocalized_f,
            section_id_f,
            generation_sort_f,
        ) = self.all_fields()?;
        let writer = self.writer_mut()?;
        for doc in docs {
            writer.delete_term(Term::from_field_u64(id_f, doc.id));
            let topics_facet = Facet::from_text(&doc.topics)?;
            let mut document = doc!(
                title_f        => doc.title,
                reference_f    => doc.reference,
                text_f         => doc.text,
                id_f           => doc.id,
                segment_f      => doc.segment,
                is_pdf_f       => doc.is_pdf,
                file_path_f    => doc.file_path,
                topics_f       => topics_facet,
                content_hash_f => doc.content_hash.unwrap_or(0),
                section_id_f   => doc.section_id.unwrap_or(doc.id),
                generation_sort_f => generation_sort_key(
                    doc.generation_order.unwrap_or(DEFAULT_GENERATION_ORDER),
                    doc.id
                )
            );
            if let Some(vocalized) = doc.text_vocalized {
                if !vocalized.is_empty() {
                    document.add_text(text_vocalized_f, vocalized);
                }
            }
            writer.add_document(document)?;
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

    /// Delete every document of one book, addressed by its `filePath` value —
    /// the stable book key the app stamps on all of a book's documents at
    /// indexing time. `filePath` is a raw STRING field, so this is a single
    /// exact `delete_term` and cannot touch other books (unlike
    /// [`Self::remove_documents_by_title`], which matches any book sharing the
    /// title). Does not commit.
    pub fn delete_documents_by_file_path(&mut self, file_path: &str) -> Result<()> {
        let file_path_f = self.schema.get_field("filePath")?;
        self.writer_mut()?
            .delete_term(Term::from_field_text(file_path_f, file_path));
        Ok(())
    }

    /// Batch form of [`Self::delete_documents_by_file_path`] — one FFI call
    /// for e.g. removing a whole custom folder of personal books. Does not
    /// commit.
    pub fn delete_documents_by_file_paths(&mut self, file_paths: Vec<String>) -> Result<()> {
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

    /// Bulk-indexing mode: while enabled, the live writer skips background
    /// segment merges (`NoMergePolicy`). During a full-library build the
    /// default `LogMergePolicy` repeatedly merges intermediate segments —
    /// CPU and IO that are thrown away, because the caller runs `optimize`
    /// (merge-all) once at the end anyway. Call with `true` before a bulk
    /// build and `false` when done — `optimize` does NOT reset the flag, and
    /// while it is set every (re)opened writer keeps `NoMergePolicy`. Off by
    /// default; incremental indexing keeps normal merging.
    pub fn set_bulk_indexing(&mut self, enabled: bool) -> Result<()> {
        self.bulk_indexing = enabled;
        let writer = self.writer_mut()?;
        if enabled {
            writer.set_merge_policy(Box::new(NoMergePolicy));
        } else {
            writer.set_merge_policy(Box::<tantivy::indexer::LogMergePolicy>::default());
        }
        debug!("bulk_indexing={enabled}");
        Ok(())
    }

    /// Flush pending writes to disk and refresh the reader.
    pub fn commit(&mut self) -> Result<()> {
        let started = Instant::now();
        self.writer_mut()?.commit()?;
        let commit_elapsed = started.elapsed();
        let reload_started = Instant::now();
        self.index_reader.reload()?;
        info!(
            "commit: {commit_elapsed:?} (reader reload {:?})",
            reload_started.elapsed()
        );
        Ok(())
    }

    /// Discard all pending writes since the last commit.
    pub fn rollback(&mut self) -> Result<()> {
        self.writer_mut()?.rollback()?;
        Ok(())
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
    ) -> Result<Vec<SearchResult>> {
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
        )
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
        )
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

    /// Merge all segments into one. Run occasionally in the background after
    /// many upserts/deletes to reclaim disk space and improve read performance.
    /// Pending (uncommitted) changes are committed first, since only committed
    /// segments participate in manual merge maintenance.
    pub fn optimize(&mut self) -> Result<()> {
        let started = Instant::now();
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
        info!(
            "optimize: {before_count} → {after_count} segments in {:?}",
            started.elapsed()
        );
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

    /// Content fingerprint per distinct `filePath`, read columnar from the
    /// live documents (like [`Self::count_documents_by_file_path`], no stored
    /// fields are touched).
    ///
    /// A value of 0 means "unverifiable": either the book was indexed without
    /// a fingerprint (e.g. PDF), or its live documents disagree (partial
    /// reindex) — callers should treat such books as changed or skip them.
    pub fn get_book_fingerprints(&self) -> Result<HashMap<String, u64>> {
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&AllQuery, &BookFingerprintCollector)?)
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
            |s| self.fuzzy_highlight_plan(s, &terms, max_distance),
            self.schema.get_field("text")?,
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
    ///
    /// Drops the single-word truncation flag — see [`Self::search`]; use
    /// [`Self::search_and_count`] ([`SearchPageResult::truncated`]) when
    /// partiality must surface.
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

    pub fn search_exact(
        &self,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<Vec<SearchResult>> {
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
        )
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
    ) -> Result<SearchPageResult> {
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
        )
    }

    #[allow(clippy::too_many_arguments)]
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
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let result = (|| {
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
                &sink,
            )
        })();
        Self::surface_stream_error(&sink, result)
    }

    /// Like [`Self::search_exact_stream`] but the first event also carries
    /// the total hit count and per-book counts from the same index pass —
    /// replacing the separate `count_exact` + `count_by_book_exact` calls a
    /// search screen would otherwise issue for the same query.
    #[allow(clippy::too_many_arguments)]
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
        sink: StreamSink<SearchStreamUpdate>,
    ) -> Result<()> {
        let result = (|| {
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
                &sink,
            )
        })();
        Self::surface_stream_error(&sink, result)
    }

    pub fn count_exact(
        &self,
        query: String,
        facets: Vec<String>,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<u32> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _) = self.build_exact_query(&query, &facets, &voc)?;
        self.run_count(q)
    }

    pub fn count_by_book_exact(
        &self,
        query: String,
        facets: Vec<String>,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<HashMap<String, u32>> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _) = self.build_exact_query(&query, &facets, &voc)?;
        self.run_count_by_book(q)
    }

    pub fn get_facet_counts_exact(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<Vec<FacetCount>> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _) = self.build_exact_query(&query, &facets, &voc)?;
        self.run_facet_counts(q, facet_prefix)
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
    ) -> Result<Vec<SearchResult>> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        // מצב-שדה: הדגלים הגלובליים או אפשרות "ניקוד"/"טעמים" פר-מילה —
        // בחירת השדה וה-analyzer של ההדגשה, בעוד הדרישה פר-תו נגזרת פר-מילה
        // בתוך בניית השאילתה.
        let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
        let (q, regex_terms, gaps, truncated, acronym_alts) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
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
        )?;
        self.run_search(
            q,
            |s| {
                self.advanced_highlight_plan_for_scope(
                    s,
                    &regex_terms,
                    &gaps,
                    &voc_mode,
                    &scope,
                    &acronym_alts,
                )
            },
            self.search_text_field(&voc_mode)?,
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
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
    ) -> Result<SearchPageResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
        let (q, regex_terms, gaps, truncated, acronym_alts) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
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
        )?;
        self.run_search_and_count(
            q,
            |s| {
                self.advanced_highlight_plan_for_scope(
                    s,
                    &regex_terms,
                    &gaps,
                    &voc_mode,
                    &scope,
                    &acronym_alts,
                )
            },
            self.search_text_field(&voc_mode)?,
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
            truncated,
        )
    }

    /// Advanced-query result stream. Drops the single-word truncation flag —
    /// see [`Self::search`]; use [`Self::search_advanced_stream_with_counts`]
    /// (its first [`SearchStreamUpdate`] carries `truncated`) when the UI
    /// must flag partial results.
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
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let result = (|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
            let (q, regex_terms, gaps, truncated, acronym_alts) = self.build_advanced_query(
                &query,
                distance,
                &custom_spacing,
                &alternative_words,
                &search_options,
                facets,
                &voc,
                &scope,
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
            )?;
            self.run_search_stream(
                q,
                |s| {
                    self.advanced_highlight_plan_for_scope(
                        s,
                        &regex_terms,
                        &gaps,
                        &voc_mode,
                        &scope,
                        &acronym_alts,
                    )
                },
                self.search_text_field(&voc_mode)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                &sink,
            )
        })();
        Self::surface_stream_error(&sink, result)
    }

    /// Like [`Self::search_advanced_stream`] but the first event also carries
    /// the total hit count and per-book counts from the same index pass —
    /// replacing the separate `count_advanced` + `count_by_book_advanced`
    /// calls a search screen would otherwise issue for the same query.
    #[allow(clippy::too_many_arguments)]
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
        sink: StreamSink<SearchStreamUpdate>,
    ) -> Result<()> {
        let result = (|| {
            let voc = VocalizedFlags::new(match_nikud, match_taamim);
            let voc_mode = voc.or(hebrew_query::options_vocalized_flags(&search_options));
            let (q, regex_terms, gaps, truncated, acronym_alts) = self.build_advanced_query(
                &query,
                distance,
                &custom_spacing,
                &alternative_words,
                &search_options,
                facets,
                &voc,
                &scope,
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
            )?;
            self.run_search_stream_with_counts(
                q,
                |s| {
                    self.advanced_highlight_plan_for_scope(
                        s,
                        &regex_terms,
                        &gaps,
                        &voc_mode,
                        &scope,
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
                &sink,
            )
        })();
        Self::surface_stream_error(&sink, result)
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
    ) -> Result<CountResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _, _, truncated, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
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
        )?;
        Ok(CountResult {
            count: self.run_count(q)?,
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
    ) -> Result<BookCountResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _, _, truncated, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
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
        )?;
        Ok(BookCountResult {
            counts: self.run_count_by_book(q)?,
            truncated,
        })
    }

    /// Advanced-query facet counts. Drops the truncation flag — see
    /// [`Self::count`]; use [`Self::get_facet_counts_advanced_with_status`].
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
    ) -> Result<FacetCountsResult> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let (q, _, _, truncated, _) = self.build_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            facets,
            &voc,
            &scope,
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
    ) -> Result<Vec<SearchResult>> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        if voc.any() {
            // The vocalized query is a materialized TermSetQuery per token —
            // it exposes its terms to the snippet generator by itself.
            let (q, _) = self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?;
            return self.run_search(
                q,
                |_| Ok(HighlightPlan::none()),
                self.search_text_field(&voc)?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
            );
        }
        let token_texts = self.index_token_texts(&query)?;
        let rank = matches!(order, ResultsOrder::Relevance);
        let q = self.build_fuzzy_search_query(&token_texts, &facets, max_distance, rank)?;
        self.run_search(
            q,
            |s| self.fuzzy_highlight_plan(s, &token_texts, max_distance),
            self.schema.get_field("text")?,
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
        )
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
    ) -> Result<SearchPageResult> {
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
            );
        }
        let token_texts = self.index_token_texts(&query)?;
        let rank = matches!(order, ResultsOrder::Relevance);
        let q = self.build_fuzzy_search_query(&token_texts, &facets, max_distance, rank)?;
        self.run_search_and_count(
            q,
            |s| self.fuzzy_highlight_plan(s, &token_texts, max_distance),
            self.schema.get_field("text")?,
            limit,
            offset,
            &order,
            &HighlightConfig::default(),
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
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
        sink: StreamSink<Vec<SearchResult>>,
    ) -> Result<()> {
        let result = (|| {
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
                    &sink,
                );
            }
            let token_texts = self.index_token_texts(&query)?;
            let rank = matches!(order, ResultsOrder::Relevance);
            let q = self.build_fuzzy_search_query(&token_texts, &facets, max_distance, rank)?;
            self.run_search_stream(
                q,
                |s| self.fuzzy_highlight_plan(s, &token_texts, max_distance),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                &sink,
            )
        })();
        Self::surface_stream_error(&sink, result)
    }

    /// Like [`Self::search_fuzzy_stream`] but the first event also carries
    /// the total hit count and per-book counts from the same index pass —
    /// replacing the separate `count_fuzzy` + `count_by_book_fuzzy` calls a
    /// search screen would otherwise issue for the same query.
    #[allow(clippy::too_many_arguments)]
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
        sink: StreamSink<SearchStreamUpdate>,
    ) -> Result<()> {
        let result = (|| {
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
                    &sink,
                );
            }
            let token_texts = self.index_token_texts(&query)?;
            let rank = matches!(order, ResultsOrder::Relevance);
            let q = self.build_fuzzy_search_query(&token_texts, &facets, max_distance, rank)?;
            self.run_search_stream_with_counts(
                q,
                |s| self.fuzzy_highlight_plan(s, &token_texts, max_distance),
                self.schema.get_field("text")?,
                limit,
                offset,
                &order,
                &HighlightConfig::default(),
                chunk_size,
                // The fuzzy path uses its own automaton budgets, not the
                // single-word degrade mechanism.
                false,
                &sink,
            )
        })();
        Self::surface_stream_error(&sink, result)
    }

    pub fn count_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<u32> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let q = if voc.any() {
            self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?
                .0
        } else {
            self.build_fuzzy_query(&query, &facets, max_distance)?
        };
        self.run_count(q)
    }

    pub fn count_by_book_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<HashMap<String, u32>> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let q = if voc.any() {
            self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?
                .0
        } else {
            self.build_fuzzy_query(&query, &facets, max_distance)?
        };
        self.run_count_by_book(q)
    }

    pub fn get_facet_counts_fuzzy(
        &self,
        query: String,
        facets: Vec<String>,
        facet_prefix: String,
        max_distance: u8,
        match_nikud: bool,
        match_taamim: bool,
    ) -> Result<Vec<FacetCount>> {
        let voc = VocalizedFlags::new(match_nikud, match_taamim);
        let q = if voc.any() {
            self.build_fuzzy_query_vocalized(&query, &facets, max_distance, &voc)?
                .0
        } else {
            self.build_fuzzy_query(&query, &facets, max_distance)?
        };
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
            self.schema.get_field("contentHash")?,
            self.schema.get_field("textVocalized")?,
            self.schema.get_field("sectionId")?,
            self.schema.get_field("generationSort")?,
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
        let writer = self.index.writer(self.writer_heap_size)?;
        if self.bulk_indexing {
            writer.set_merge_policy(Box::new(NoMergePolicy));
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
        let gaps = vec![slop; patterns.len().saturating_sub(1)];
        let text_field = self.schema.get_field("text")?;
        self.build_query_from_patterns(
            patterns,
            &[],
            facets,
            &gaps,
            max_expansions,
            text_field,
            &SearchScope::WordDistance,
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
    ) -> Result<(Box<dyn Query>, bool)> {
        if hebrew_query::split_query_words(negative_query).is_empty() {
            return Ok((positive_query, positive_truncated));
        }

        let (negative, _, _, negative_truncated, _) = self.build_advanced_query(
            negative_query,
            negative_distance,
            negative_custom_spacing,
            negative_alternative_words,
            negative_search_options,
            Vec::new(),
            voc,
            negative_scope,
        )?;

        Ok((
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, positive_query),
                (Occur::MustNot, negative),
            ])),
            positive_truncated || negative_truncated,
        ))
    }

    /// `text_field` selects the dictionary the patterns run against: the
    /// plain `text` field, or `textVocalized` on the vocalized paths (whose
    /// patterns carry free-mark runs and must never hit the plain field).
    ///
    /// `gaps` is the per-pair intermediate-word allowance (`gaps[i]` between
    /// words `i` and `i+1`). The phrase branch hands tantivy the *sum* as its
    /// slop — tantivy spends slop cumulatively across the phrase, so the max
    /// would reject a match using its allowance in two different gaps — and
    /// then wraps the query in [`GapVerifiedPhraseQuery`], which re-checks
    /// candidates against the positional postings so only in-order,
    /// per-pair-within-allowance occurrences survive.
    #[allow(clippy::too_many_arguments)]
    fn build_query_from_patterns(
        &self,
        regex_terms: Vec<hebrew_query::WordPattern>,
        typo_tokens: &[String],
        facets: Vec<String>,
        gaps: &[u32],
        max_expansions: u32,
        text_field: Field,
        scope: &SearchScope,
    ) -> Result<(Box<dyn Query>, bool)> {
        let topics_field = self.schema.get_field("topics")?;

        // Resolved up front: the same facet filter both narrows the section
        // pre-pass (fewer candidate sections to intersect) and gates the
        // final result set.
        let facets_query: Option<TermSetQuery> = if facets.is_empty() {
            None
        } else {
            let facet_terms: Vec<Term> = facets
                .iter()
                .map(|f| Ok(Term::from_facet(topics_field, &Facet::from_text(f)?)))
                .collect::<Result<Vec<_>>>()?;
            Some(TermSetQuery::new(facet_terms))
        };

        // Only the word-materialization paths (single word / paragraph /
        // section scopes) degrade under their collection budgets; the empty
        // and phrase branches either match nothing or carry their own in-DFA
        // caps, so neither reports truncation.
        let (main_query, truncated): (Box<dyn Query>, bool) = match regex_terms.len() {
            0 => (Box::new(EmptyQuery), false),
            1 => self.single_regex_term_query(
                regex_terms[0].branches(),
                typo_tokens,
                text_field,
                max_expansions,
            )?,
            _ => match scope {
                // הטווחים "פסקה"/"כותרת" מוותרים על סדר ומרווח — כל מילה
                // מתממשת ל-TermSetQuery משלה והצירוף נעשה בין מסמכים
                // (פסקה) או בין סעיפים (כותרת).
                SearchScope::SameParagraph | SearchScope::SameSection => self.scoped_words_query(
                    &regex_terms,
                    text_field,
                    max_expansions,
                    scope,
                    facets_query.as_ref(),
                )?,
                SearchScope::WordDistance => {
                    debug_assert_eq!(gaps.len() + 1, regex_terms.len());
                    // The phrase path needs one pattern string per word position:
                    // `RegexPhraseQuery` compiles each as a single DFA (branch
                    // splitting cannot help here — phrase matching intersects
                    // positional postings, so the per-word budget caps in
                    // `hebrew_query` remain load-bearing for this path).
                    let joined: Vec<String> = regex_terms
                        .iter()
                        .map(hebrew_query::WordPattern::joined)
                        .collect();
                    let slop_budget = gaps.iter().fold(0u32, |acc, &g| acc.saturating_add(g));
                    let mut phrase_query = RegexPhraseQuery::new(text_field, joined.clone());
                    phrase_query.set_slop(slop_budget);
                    phrase_query.set_max_expansions(max_expansions);
                    if slop_budget == 0 {
                        // Slop 0 is already strict in-order adjacency — nothing
                        // for the position verifier to trim.
                        (Box::new(phrase_query), false)
                    } else {
                        (
                            Box::new(GapVerifiedPhraseQuery::new(
                                phrase_query,
                                text_field,
                                joined,
                                gaps.to_vec(),
                            )),
                            false,
                        )
                    }
                }
            },
        };

        let Some(facets_query) = facets_query else {
            return Ok((main_query, truncated));
        };

        Ok((
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, main_query),
                (Occur::Must, Box::new(facets_query) as Box<dyn Query>),
            ])),
            truncated,
        ))
    }

    /// Multi-word query for the paragraph/section scopes: every word is
    /// materialized into its own `TermSetQuery` (same budgets and cache as
    /// the single-word path — each word executes exactly like it would
    /// alone), then combined:
    ///
    /// - **Paragraph** — a boolean AND: all words must occur in the same
    ///   document (= book line), in any order, at any distance.
    /// - **Section** — a two-pass plan (see the `section_scope` module):
    ///   intersect the per-word `sectionId` sets, then serve the lines that
    ///   carry any query word inside a fully-matching section. The facet
    ///   filter narrows the pre-pass so a facet-excluded book cannot bloat
    ///   the intersection sets (correctness never depends on it — section
    ///   ids are unique per book).
    ///
    /// `truncated` is the OR over the per-word collection truncations.
    fn scoped_words_query(
        &self,
        regex_terms: &[hebrew_query::WordPattern],
        text_field: Field,
        max_expansions: u32,
        scope: &SearchScope,
        facets_query: Option<&TermSetQuery>,
    ) -> Result<(Box<dyn Query>, bool)> {
        let mut truncated = false;
        let mut word_queries: Vec<Box<dyn Query>> = Vec::with_capacity(regex_terms.len());
        for pattern in regex_terms {
            let (word_query, word_truncated) =
                self.single_regex_term_query(pattern.branches(), &[], text_field, max_expansions)?;
            truncated |= word_truncated;
            word_queries.push(word_query);
        }

        if matches!(scope, SearchScope::SameParagraph) {
            let clauses: Vec<(Occur, Box<dyn Query>)> =
                word_queries.into_iter().map(|q| (Occur::Must, q)).collect();
            return Ok((Box::new(BooleanQuery::new(clauses)), truncated));
        }

        // Section scope — pass 1: the sections every word appears in.
        let searcher = self.index_reader.searcher();
        let mut allowed: Option<HashSet<u64>> = None;
        for word_query in &word_queries {
            let sections = match facets_query {
                Some(fq) => searcher.search(
                    &BooleanQuery::new(vec![
                        (Occur::Must, word_query.box_clone()),
                        (Occur::Must, Box::new(fq.clone()) as Box<dyn Query>),
                    ]),
                    &SectionIdsCollector,
                )?,
                None => searcher.search(word_query.as_ref(), &SectionIdsCollector)?,
            };
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
        };
        if let Some(entry) = self.term_cache.lock().unwrap().get(&cache_key) {
            return Ok((
                Box::new(TermSetQuery::new(entry.terms.iter().cloned())),
                entry.truncated,
            ));
        }

        let regexes: Vec<tantivy_fst::Regex> = branches
            .iter()
            .map(|branch| {
                tantivy_fst::Regex::new(branch).map_err(|e| {
                    // Surface the failing branch loudly: the historical
                    // failure mode here was a compile error silently becoming
                    // "0 results" in the UI.
                    error!(
                        "regex branch compilation failed ({} chars): {e}. Branch prefix: {}",
                        branch.chars().count(),
                        branch.chars().take(80).collect::<String>(),
                    );
                    anyhow::anyhow!(
                        "invalid regex branch ({} chars): {e}",
                        branch.chars().count()
                    )
                })
            })
            .collect::<Result<_>>()?;
        let inverted_indexes = searcher
            .segment_readers()
            .iter()
            .map(|reader| reader.inverted_index(text_field))
            .collect::<tantivy::Result<Vec<_>>>()?;
        let mut matched: HashSet<String> = HashSet::new();
        // Sum of doc_freq over every stream hit — the real cost of executing
        // the TermSetQuery (BitSet union of one postings list per matched term
        // per segment). A term matched by several automatons in the same
        // segment is counted once per automaton — a slight over-estimate that
        // only errs toward earlier truncation.
        let mut postings_cost: u64 = 0;
        let mut truncated = false;
        // Automatons run most-important-first: branches (exact forms before
        // typo variants — the `build_word_regex` contract), then the
        // Levenshtein typo automatons. Each automaton covers *all* segments
        // before the next starts, so when a budget runs out mid-collection
        // the query degrades to the highest-priority automaton prefix rather
        // than over-serving whichever segment happened to be scanned first.
        'branches: for regex in &regexes {
            for inverted in &inverted_indexes {
                if Self::collect_automaton_terms(
                    inverted,
                    regex,
                    max_expansions,
                    &mut matched,
                    &mut postings_cost,
                )? {
                    truncated = true;
                    break 'branches;
                }
            }
        }
        if !truncated && !typo_tokens.is_empty() {
            // Same builder configuration as the fuzzy path (distance 1,
            // transposition counts as one edit): the whole edit-distance-1
            // neighborhood in one scan per token per segment, replacing the
            // ≤128 sampled literal-variant scans (VARIATION_CEILING_RESEARCH
            // §3.ג). Guarded by `!truncated` on purpose — typo expansion has
            // the lowest priority, so when the exact branches alone exhaust a
            // budget (an extremely common word) the scan is skipped entirely
            // rather than pushed past the budget; the query then behaves as
            // if typo tolerance found nothing, and the warn! below records it.
            let builder = LevenshteinAutomatonBuilder::new(1, true);
            'typo: for token in typo_tokens {
                let automaton = DfaWrapper(builder.build_dfa(token));
                for inverted in &inverted_indexes {
                    if Self::collect_automaton_terms(
                        inverted,
                        &automaton,
                        max_expansions,
                        &mut matched,
                        &mut postings_cost,
                    )? {
                        truncated = true;
                        break 'typo;
                    }
                }
            }
        }
        if truncated {
            warn!(
                "single-word term collection truncated at {} terms / ~{postings_cost} postings \
                 (caps: {max_expansions} terms, {SINGLE_WORD_POSTINGS_BUDGET} postings); \
                 serving the highest-priority branches collected so far",
                matched.len(),
            );
        }
        let terms: Arc<Vec<Term>> = Arc::new(
            matched
                .into_iter()
                .map(|t| Term::from_field_text(text_field, &t))
                .collect(),
        );
        self.term_cache.lock().unwrap().put(
            cache_key,
            CachedTermSet {
                terms: terms.clone(),
                truncated,
            },
        );
        Ok((
            Box::new(TermSetQuery::new(terms.iter().cloned())),
            truncated,
        ))
    }

    /// Streams every term `automaton` matches in one segment's dictionary
    /// into `matched`, charging each hit's per-segment `doc_freq` against
    /// `postings_cost` (the streamer decodes `TermInfo` in-memory on
    /// `advance()`, so reading the value adds no IO). Returns `true` when a
    /// collection budget was hit — degrade, never error: the check runs after
    /// insertion, so even a first term costlier than the whole budget is
    /// kept and the caller serves what was gathered.
    fn collect_automaton_terms<A>(
        inverted: &tantivy::InvertedIndexReader,
        automaton: &A,
        max_expansions: u32,
        matched: &mut HashSet<String>,
        postings_cost: &mut u64,
    ) -> Result<bool>
    where
        A: Automaton,
        A::State: Clone,
    {
        let mut stream = inverted.terms().search(automaton).into_stream()?;
        while stream.advance() {
            if let Ok(term) = std::str::from_utf8(stream.key()) {
                *postings_cost += u64::from(stream.value().doc_freq);
                // contains-before-insert avoids re-allocating the term string
                // when it was already seen in an earlier segment or matched
                // by an earlier automaton.
                if !matched.contains(term) {
                    matched.insert(term.to_string());
                }
                if matched.len() >= max_expansions as usize
                    || *postings_cost >= SINGLE_WORD_POSTINGS_BUDGET
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
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
    /// by facets. No regex — fastest path. With a vocalized flag on, the query
    /// runs against the `textVocalized` dictionary instead: each token becomes
    /// a required-marks regex ([`hebrew_query::vocalized_token_pattern`]), a
    /// single word materializes via [`Self::single_regex_term_query`] (which
    /// may truncate — the returned flag), a phrase becomes a
    /// `RegexPhraseQuery`.
    fn build_exact_query(
        &self,
        query_str: &str,
        facets: &[String],
        voc: &VocalizedFlags,
    ) -> Result<(Box<dyn Query>, bool)> {
        if voc.any() {
            return self.build_exact_query_vocalized(query_str, facets, voc);
        }
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
            Ok((main_query, false))
        } else {
            Ok((
                Box::new(BooleanQuery::new(vec![
                    (Occur::Must, main_query),
                    (Occur::Must, self.facet_filter_query(facets)?),
                ])),
                false,
            ))
        }
    }

    /// The vocalized arm of [`Self::build_exact_query`].
    fn build_exact_query_vocalized(
        &self,
        query_str: &str,
        facets: &[String],
        voc: &VocalizedFlags,
    ) -> Result<(Box<dyn Query>, bool)> {
        let voc_field = self.schema.get_field("textVocalized")?;
        let tokens = self.index_token_texts_with("hebrew_vocalized_query", query_str)?;
        let patterns: Vec<String> = tokens
            .iter()
            .map(|t| hebrew_query::vocalized_token_pattern(t, voc))
            .collect();
        let (main_query, truncated): (Box<dyn Query>, bool) = match patterns.len() {
            0 => (Box::new(EmptyQuery), false),
            1 => self.single_regex_term_query(
                &patterns,
                &[],
                voc_field,
                VOC_EXACT_SINGLE_MAX_EXPANSIONS,
            )?,
            _ => {
                let mut phrase_query = RegexPhraseQuery::new(voc_field, patterns);
                phrase_query.set_slop(0);
                phrase_query.set_max_expansions(VOC_PHRASE_MAX_EXPANSIONS);
                (Box::new(phrase_query), false)
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
        let builder = LevenshteinAutomatonBuilder::new(distance, true);
        let mut branches = Vec::new();
        for base in bases {
            let automaton = DfaWrapper(builder.build_dfa(base));
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
                                Box::new(TermQuery::new(
                                    Term::from_field_text(text_f, &clean),
                                    IndexRecordOption::Basic,
                                )),
                            ),
                        ])),
                        None => Box::new(fuzzy),
                    }
                } else {
                    let mut should = Self::exact_rank_clauses(text_f, t);
                    if let Some(clean) = Self::quoteless_variant(t) {
                        should.extend(Self::exact_rank_clauses(text_f, &clean));
                    }
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
            let clean = Self::quoteless_variant(token);
            let mut forms = dict.recall_forms(token, MAX_LEXICAL_FORMS);
            forms.retain(|f| f != token && Some(f.as_str()) != clean.as_deref());

            // Unranked: the original recall shape — `fuzzy OR termset`, or just
            // `fuzzy` when the dictionary has no extra forms. Ranked: prepend the
            // exact tier (the exact term is a subset of the fuzzy match, so this
            // never changes recall) and boost the lexical tier. `BooleanQuery`
            // sums `Should` scores, so exact-floor + BM25 > lexical > fuzzy.
            // A quote-bearing token also carries its quote-free spelling in
            // the exact tier — clean-typography editions match at distance 0
            // and rank as exact, not as edit-distance tail.
            let mut should: Vec<(Occur, Box<dyn Query>)> = if rank {
                Self::exact_rank_clauses(text_f, token)
            } else {
                Vec::with_capacity(3)
            };
            if let Some(clean) = &clean {
                if rank {
                    should.extend(Self::exact_rank_clauses(text_f, clean));
                } else {
                    should.push((
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(text_f, clean),
                            IndexRecordOption::Basic,
                        )),
                    ));
                }
            }
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
        voc: &VocalizedFlags,
        scope: &SearchScope,
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
        let gaps = prepared.gaps.clone();
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
        )?;
        let (main_query, main_truncated) = self.build_query_from_patterns(
            prepared.regex_terms,
            &prepared.typo_tokens,
            facets,
            &gaps,
            prepared.max_expansions,
            text_field,
            scope,
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
        Ok((query, regex_terms, gaps, truncated, acronym_patterns))
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
    ) -> Result<Vec<(Box<dyn Query>, bool, Vec<String>)>> {
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
            // פענוח הוא ביטוי קנוני — מילים צמודות (slop 0, ללא GapVerified).
            let gaps = vec![0u32; patterns.len().saturating_sub(1)];
            let (alt_query, truncated) = self.build_query_from_patterns(
                patterns,
                &[],
                facets.clone(),
                &gaps,
                max_expansions,
                text_field,
                scope,
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

    fn run_search<F>(
        &self,
        query: Box<dyn Query>,
        make_highlight: F,
        text_field: Field,
        limit: u32,
        offset: u32,
        order: &ResultsOrder,
        hl: &HighlightConfig,
    ) -> Result<Vec<SearchResult>>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        let searcher = self.index_reader.searcher();
        let addresses = Self::collect_addresses(&searcher, &*query, limit, offset, order)?;
        if addresses.is_empty() {
            return Ok(Vec::new());
        }
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
    ) -> Result<SearchPageResult>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
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
            ResultsOrder::Generation => {
                let top_collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_fast_field::<u64>("generationSort", Order::Asc);
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
                truncated,
            });
        }
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
    fn surface_stream_error<T: crate::frb_generated::SseEncode>(
        sink: &StreamSink<T>,
        result: Result<()>,
    ) -> Result<()> {
        if let Err(err) = result {
            error!("stream search failed: {err:#}");
            let _ = sink.add_error(err);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
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
        sink: &StreamSink<Vec<SearchResult>>,
    ) -> Result<()>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        let searcher = self.index_reader.searcher();
        let chunk_size = (chunk_size.max(1)) as usize;
        let addresses = Self::collect_addresses(&searcher, &*query, limit, offset, order)?;
        if addresses.is_empty() {
            return Ok(());
        }
        let plan = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = plan.query.as_deref().unwrap_or(query.as_ref());
        let phrase = plan.phrase.as_ref();
        // One generator for the whole stream: creating it resolves term
        // doc-frequencies, which is too expensive to repeat per chunk.
        let snippet_generator = Self::make_snippet_generator(&searcher, hl_q, text_field, hl)?;
        for chunk in addresses.chunks(chunk_size) {
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
        sink: &StreamSink<SearchStreamUpdate>,
    ) -> Result<()>
    where
        F: FnOnce(&Searcher) -> Result<HighlightPlan>,
    {
        let searcher = self.index_reader.searcher();
        let chunk_size = (chunk_size.max(1)) as usize;

        let (addresses, total_count, book_counts): (Vec<DocAddress>, u32, HashMap<String, u32>) =
            match order {
                ResultsOrder::Catalogue => {
                    let top_collector = TopDocs::with_limit(limit as usize)
                        .and_offset(offset as usize)
                        .order_by_fast_field::<u64>("id", Order::Asc);
                    let (top_docs, count, by_book) =
                        searcher.search(&*query, &(top_collector, Count, BookCountCollector))?;
                    let addrs = top_docs.into_iter().map(|(_, addr)| addr).collect();
                    (addrs, count as u32, by_book)
                }
                ResultsOrder::Generation => {
                    let top_collector = TopDocs::with_limit(limit as usize)
                        .and_offset(offset as usize)
                        .order_by_fast_field::<u64>("generationSort", Order::Asc);
                    let (top_docs, count, by_book) =
                        searcher.search(&*query, &(top_collector, Count, BookCountCollector))?;
                    let addrs = top_docs.into_iter().map(|(_, addr)| addr).collect();
                    (addrs, count as u32, by_book)
                }
                ResultsOrder::Relevance => {
                    let top_collector = TopDocs::with_limit(limit as usize)
                        .and_offset(offset as usize)
                        .order_by_score();
                    let (top_docs, count, by_book) =
                        searcher.search(&*query, &(top_collector, Count, BookCountCollector))?;
                    let addrs = top_docs.into_iter().map(|(_, addr)| addr).collect();
                    (addrs, count as u32, by_book)
                }
            };

        // Counts first — the page addresses may be empty (offset past the
        // end) while the totals are still meaningful.
        if sink
            .add(SearchStreamUpdate {
                total_count: Some(total_count),
                book_counts: Some(book_counts),
                results: Vec::new(),
                truncated,
            })
            .is_err()
        {
            return Ok(());
        }
        if addresses.is_empty() {
            return Ok(());
        }

        let plan = Self::resolve_highlight(&searcher, make_highlight);
        let hl_q: &dyn Query = plan.query.as_deref().unwrap_or(query.as_ref());
        let phrase = plan.phrase.as_ref();
        // One generator for the whole stream: creating it resolves term
        // doc-frequencies, which is too expensive to repeat per chunk.
        let snippet_generator = Self::make_snippet_generator(&searcher, hl_q, text_field, hl)?;
        for chunk in addresses.chunks(chunk_size) {
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
            if sink
                .add(SearchStreamUpdate {
                    total_count: None,
                    book_counts: None,
                    results,
                    truncated: false,
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
            ResultsOrder::Generation => {
                let collector = TopDocs::with_limit(limit as usize)
                    .and_offset(offset as usize)
                    .order_by_fast_field::<u64>("generationSort", Order::Asc);
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
    ) -> Result<Option<HighlightPattern>> {
        let advanced = hebrew_query::prepare_advanced_query(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
        );
        let searcher = self.index_reader.searcher();

        // Per word: compile the word's branches and collect the index terms
        // they match. `automaton_highlight_terms` splits its budget across the
        // word's branches, so a wide word (partial + typo) still spreads its
        // term allowance over all variants instead of exhausting it on the
        // first branch; the display char budget bounds the final pattern size.
        let mut per_word_terms: Vec<Vec<String>> = Vec::with_capacity(advanced.regex_terms.len());
        for word_pattern in &advanced.regex_terms {
            let automatons: Vec<tantivy_fst::Regex> = word_pattern
                .branches()
                .iter()
                .filter_map(|branch| match tantivy_fst::Regex::new(branch) {
                    Ok(re) => Some(re),
                    Err(e) => {
                        // A branch the search itself would reject: skip it for
                        // highlighting (the search surfaces the error), keep
                        // painting what the remaining branches match.
                        log::error!(
                            "highlight branch failed to compile ({} chars): {e}",
                            branch.chars().count()
                        );
                        None
                    }
                })
                .collect();
            let mut matched: HashSet<String> = if automatons.is_empty() {
                HashSet::new()
            } else {
                self.automaton_highlight_terms(&searcher, &automatons)?
            };
            // Single-word typo path: the search expands typo coverage through
            // Levenshtein-1 automatons, not regex branches — run the same
            // scan here so a document found via any edit-distance-1 variant
            // highlights that variant (search↔highlight parity).
            if !advanced.typo_tokens.is_empty() {
                let builder = LevenshteinAutomatonBuilder::new(1, true);
                let typo_automatons: Vec<DfaWrapper> = advanced
                    .typo_tokens
                    .iter()
                    .map(|t| DfaWrapper(builder.build_dfa(t)))
                    .collect();
                matched.extend(self.automaton_highlight_terms(&searcher, &typo_automatons)?);
            }
            per_word_terms.push(matched.into_iter().collect());
        }

        Ok(display_highlight::build_display_highlight_from_terms(
            &query,
            distance,
            &custom_spacing,
            &alternative_words,
            &search_options,
            &per_word_terms,
        )
        .map(|hl| HighlightPattern {
            combined_pattern: hl.combined_pattern,
            word_patterns: hl.word_patterns,
            word_boundary_eligible: hl.word_boundary_eligible,
        }))
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
    ) -> Result<Option<HighlightPattern>> {
        anyhow::ensure!(
            max_distance <= 2,
            "fuzzy highlight distance is limited to 2, got {max_distance}"
        );
        let tokens = self.index_token_texts(&query)?;
        // `build_display_highlight_from_terms` walks `split_query_words`;
        // the analyzer tokens are aligned with it by design, but if they ever
        // diverge, feeding misaligned terms would paint the wrong word — fall
        // back to the query-shape pattern instead.
        let words = hebrew_query::split_query_words(&hebrew_query::normalize_for_index(&query));
        let per_word_terms: Vec<Vec<String>> = if tokens.len() != words.len() {
            Vec::new()
        } else {
            let searcher = self.index_reader.searcher();
            let builder = LevenshteinAutomatonBuilder::new(max_distance, true);
            let mut collected = Vec::with_capacity(tokens.len());
            for token in &tokens {
                let mut matched = if max_distance == 0 {
                    HashSet::new()
                } else {
                    self.automaton_highlight_terms(
                        &searcher,
                        &[DfaWrapper(builder.build_dfa(token))],
                    )?
                };
                matched.insert(token.clone());
                if max_distance > 0 {
                    if let Some(dict) = self.magic_dict.as_ref() {
                        for form in dict.highlight_forms(token, MAX_LEXICAL_FORMS) {
                            matched.insert(form);
                        }
                    }
                }
                collected.push(matched.into_iter().collect());
            }
            collected
        };

        Ok(display_highlight::build_display_highlight_from_terms(
            &query,
            0,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &per_word_terms,
        )
        .map(|hl| HighlightPattern {
            combined_pattern: hl.combined_pattern,
            word_patterns: hl.word_patterns,
            word_boundary_eligible: hl.word_boundary_eligible,
        }))
    }

    /// One index-term set per query word (in phrase order), each materialized by
    /// streaming the term dictionary through that word's regex automaton — the
    /// same FST scan the search uses, so the sets contain exactly the
    /// morphological variants (prefixes, suffixes, alternatives) that genuinely
    /// matched. The per-word budget split mirrors
    /// [`Self::automaton_highlight_terms`], so their union equals the merged
    /// highlight term set the flat query paints with.
    fn phrase_per_word_terms(
        &self,
        searcher: &Searcher,
        regex_terms: &[String],
        field: Field,
    ) -> Result<Vec<HashSet<String>>> {
        let cap = (MAX_HIGHLIGHT_TERMS / regex_terms.len().max(1)).max(1);
        regex_terms
            .iter()
            .map(|pattern| {
                let re = tantivy_fst::Regex::new(pattern)
                    .map_err(|e| anyhow::anyhow!("invalid highlight regex {pattern:?}: {e}"))?;
                Ok(self
                    .automaton_terms_in_field(searcher, field, &re, cap)?
                    .into_iter()
                    .collect())
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
        let terms: Vec<Term> = per_word_terms
            .iter()
            .flat_map(|set| set.iter())
            .map(|t| Term::from_field_text(text_f, t))
            .collect();
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
    /// חלופה מתממשות ומצטרפות לאיחוד ההדגשה השטוח, כך שמסמך שנמצא דרך
    /// החלופה ייצבע (דרך נפילת מסנן-הביטוי לצביעת-הטרמים הרחבה).
    fn advanced_highlight_plan_for_scope(
        &self,
        searcher: &Searcher,
        regex_terms: &[String],
        gaps: &[u32],
        voc: &VocalizedFlags,
        scope: &SearchScope,
        acronym_alts: &[Vec<String>],
    ) -> Result<HighlightPlan> {
        match scope {
            SearchScope::WordDistance => {
                self.advanced_highlight_plan(searcher, regex_terms, gaps, voc, acronym_alts)
            }
            SearchScope::SameParagraph | SearchScope::SameSection => {
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
        gaps: &[u32],
        voc: &VocalizedFlags,
        acronym_alts: &[Vec<String>],
    ) -> Result<HighlightPlan> {
        if regex_terms.len() < 2 && acronym_alts.is_empty() {
            return Ok(HighlightPlan::none());
        }
        let field = self.search_text_field(voc)?;
        let analyzer = if voc.any() {
            "hebrew_vocalized"
        } else {
            "hebrew"
        };
        let per_word_terms = self.phrase_per_word_terms(searcher, regex_terms, field)?;
        // איחוד ההדגשה השטוח נושא גם את מילות חלופות הר"ת; מסנן-הביטוי
        // נשאר של השאילתה הראשית בלבד (רק במסלול הרב-מילי) — פרגמנט
        // שנמצא דרך חלופה נופל לצביעה הרחבה וכל מילות החלופה נצבעות.
        let mut all_word_sets = per_word_terms.clone();
        for alt in acronym_alts {
            all_word_sets.extend(self.phrase_per_word_terms(searcher, alt, field)?);
        }
        let query = self.terms_query_from_word_sets(&all_word_sets, field)?;
        let phrase = (per_word_terms.len() >= 2).then(|| PhraseHighlight {
            per_word_terms,
            gaps: gaps.to_vec(),
            analyzer,
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
            let gaps = vec![0; per_word_terms.len().saturating_sub(1)];
            return Ok(HighlightPlan {
                query,
                phrase: Some(PhraseHighlight {
                    per_word_terms,
                    gaps,
                    analyzer: "hebrew_vocalized",
                }),
            });
        }
        let tokens = self.index_token_texts(query_str)?;
        if tokens.len() < 2 {
            return Ok(HighlightPlan::none());
        }
        let per_word_terms: Vec<HashSet<String>> =
            tokens.into_iter().map(|t| HashSet::from([t])).collect();
        let gaps = vec![0; per_word_terms.len().saturating_sub(1)];
        Ok(HighlightPlan {
            query: None,
            phrase: Some(PhraseHighlight {
                per_word_terms,
                gaps,
                analyzer: "hebrew",
            }),
        })
    }

    /// Highlight plan for the approximate (`fuzzy`) search. Always builds the
    /// flat highlight query (fuzzy/lexical automatons expose no static terms).
    /// Adds a phrase filter only for the lexical multi-word path — the sole
    /// fuzzy path that builds a `RegexPhraseQuery`. Plain fuzzy multi-word is a
    /// per-token AND, where every occurrence of every word is a real hit and
    /// must stay highlighted, so it carries no filter.
    fn fuzzy_highlight_plan(
        &self,
        searcher: &Searcher,
        term_texts: &[String],
        max_distance: u8,
    ) -> Result<HighlightPlan> {
        let query = self
            .build_fuzzy_highlight(searcher, term_texts, max_distance)
            .ok();
        let phrase = if term_texts.len() >= 2 && self.magic_dict.is_some() && max_distance > 0 {
            let per_word_terms =
                self.lexical_phrase_per_word_terms(searcher, term_texts, max_distance)?;
            let gaps = vec![LEXICAL_FUZZY_PHRASE_SLOP; per_word_terms.len().saturating_sub(1)];
            Some(PhraseHighlight {
                per_word_terms,
                gaps,
                analyzer: "hebrew",
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
        let builder = LevenshteinAutomatonBuilder::new(max_distance, true);
        tokens
            .iter()
            .map(|token| {
                let mut matched = self
                    .automaton_highlight_terms(searcher, &[DfaWrapper(builder.build_dfa(token))])?;
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
        let mut matched = Vec::new();
        let mut seen = HashSet::new();
        'segments: for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(text_f)?;
            let mut stream = inverted.terms().search(automaton).into_stream()?;
            while stream.advance() {
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
        let mut snippet_generator = SnippetGenerator::create(searcher, query, text_field)?;
        snippet_generator.set_max_num_chars(hl.max_chars as usize);
        Ok(snippet_generator)
    }

    /// Re-derives a snippet's highlight markup so only complete, in-order
    /// phrase occurrences stay painted (see [`PhraseHighlight`]).
    ///
    /// Tokenizes the fragment with the same `text`-field analyzer the index and
    /// `SnippetGenerator` use, tags each token with the query words it can fill,
    /// then keeps the byte ranges of tokens forming an occurrence
    /// `w0 … w1 … w_{k-1}` where each adjacent pair `w-1, w` is at most
    /// `gaps[w-1]` intermediate tokens apart — the greedy, leftmost,
    /// non-overlapping match `display_highlight`'s combined pattern performs
    /// in an opened book.
    ///
    /// Returns `None` when the fragment holds no complete occurrence, so the
    /// caller falls back to the plain term highlight instead of painting
    /// nothing (never less context than before).
    fn phrase_filtered_snippet_html(
        searcher: &Searcher,
        fragment: &str,
        phrase: &PhraseHighlight,
        hl: &HighlightConfig,
    ) -> Option<String> {
        let word_count = phrase.per_word_terms.len();
        if word_count < 2 {
            return None;
        }
        let mut analyzer = searcher.index().tokenizers().get(phrase.analyzer)?;

        // Candidate = a fragment token that can fill at least one query word.
        // `order` is the tokenizer's `position` — one increment per *word*:
        // the quote-free twin token an indexing analyzer emits (ראו
        // `emit_quote_free`) shares its word's position, so it must not
        // inflate the intermediate-word gap `order_b - order_a - 1`.
        struct Candidate {
            order: usize,
            from: usize,
            to: usize,
            words: Vec<usize>,
        }
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut stream = analyzer.token_stream(fragment);
        while let Some(token) = stream.next() {
            let words: Vec<usize> = phrase
                .per_word_terms
                .iter()
                .enumerate()
                .filter_map(|(w, set)| set.contains(token.text.as_str()).then_some(w))
                .collect();
            if !words.is_empty() {
                candidates.push(Candidate {
                    order: token.position,
                    from: token.offset_from,
                    to: token.offset_to,
                    words,
                });
            }
        }

        // Greedy leftmost, non-overlapping scan.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        let mut ci = 0usize;
        while ci < candidates.len() {
            if candidates[ci].words.contains(&0) {
                let mut chosen = vec![ci];
                let mut cur = ci;
                let mut ok = true;
                for w in 1..word_count {
                    let max_gap = phrase.gaps.get(w - 1).copied().unwrap_or(0) as usize;
                    let mut m = cur + 1;
                    let mut found = None;
                    while m < candidates.len() {
                        // The gap grows monotonically with m, so once it exceeds
                        // the allowance no later candidate can match this word.
                        // saturating: טוקן-תאום חולק עמדה עם מילתו (אין הפרש).
                        if candidates[m]
                            .order
                            .saturating_sub(candidates[cur].order + 1)
                            > max_gap
                        {
                            break;
                        }
                        if candidates[m].words.contains(&w) {
                            found = Some(m);
                            break;
                        }
                        m += 1;
                    }
                    match found {
                        Some(m) => {
                            chosen.push(m);
                            cur = m;
                        }
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    for &c in &chosen {
                        ranges.push((candidates[c].from, candidates[c].to));
                    }
                    ci = cur + 1;
                    continue;
                }
            }
            ci += 1;
        }

        if ranges.is_empty() {
            return None;
        }

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
        Some(html)
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
        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        // המסלול המנוקד מציג את העותק המנוקד השמור; שדה `text` הרגיל נשאר
        // fallback הגנתי (לא אמור לקרות — כל מסמך שנמצא דרך השדה המנוקד
        // נכתב עם עותק שמור).
        let plain_text_field = schema.get_field("text")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;

        let mut results = Vec::with_capacity(addresses.len());
        for doc_address in addresses {
            let retrieved_doc = match searcher.doc::<TantivyDocument>(doc_address) {
                Ok(d) => d,
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
                    continue;
                }
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
                .or_else(|| {
                    retrieved_doc
                        .get_first(plain_text_field)
                        .and_then(|v| v.as_str())
                })
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
            // For multi-word phrase queries, tantivy's term-based highlighter
            // paints every occurrence of every query term; re-derive the
            // highlights so only in-order, within-gap phrase occurrences stay
            // painted. Falls back to the plain term highlight when the chosen
            // fragment holds no complete phrase occurrence (never paints less
            // context than before).
            let snippet_html = match phrase {
                Some(pf) => {
                    Self::phrase_filtered_snippet_html(searcher, snippet.fragment(), pf, hl)
                        .unwrap_or_else(|| snippet.to_html())
                }
                None => snippet.to_html(),
            };
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

// ── BookFingerprintCollector ───────────────────────────────────────────────────

/// Collects the `contentHash` fast-field value per `filePath` fast field.
/// Per-segment work uses term ordinals; strings are decoded only in harvest().
/// Documents of the same book that disagree on the hash collapse to 0
/// ("unverifiable"), and 0 wins over any value when merging segments.
struct BookFingerprintCollector;

struct BookFingerprintSegmentCollector {
    str_col: Option<tantivy::columnar::StrColumn>,
    hash_col: Option<tantivy::columnar::Column<u64>>,
    fingerprints: HashMap<u64, u64>,
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
        let hash_col = reader.fast_fields().u64("contentHash").ok();
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
        per_segment: Vec<tantivy::Result<HashMap<String, u64>>>,
    ) -> tantivy::Result<HashMap<String, u64>> {
        let mut merged: HashMap<String, u64> = HashMap::new();
        for seg_result in per_segment {
            for (path, hash) in seg_result? {
                merged
                    .entry(path)
                    .and_modify(|existing| {
                        if *existing != hash {
                            *existing = 0;
                        }
                    })
                    .or_insert(hash);
            }
        }
        Ok(merged)
    }
}

impl SegmentCollector for BookFingerprintSegmentCollector {
    type Fruit = tantivy::Result<HashMap<String, u64>>;

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        let Some(str_col) = &self.str_col else {
            return;
        };
        let Some(term_ord) = str_col.term_ords(doc_id).next() else {
            return;
        };
        let hash = self
            .hash_col
            .as_ref()
            .and_then(|col| col.first(doc_id))
            .unwrap_or(0);
        self.fingerprints
            .entry(term_ord)
            .and_modify(|existing| {
                if *existing != hash {
                    *existing = 0;
                }
            })
            .or_insert(hash);
    }

    fn harvest(self) -> tantivy::Result<HashMap<String, u64>> {
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

    #[allow(clippy::too_many_arguments)]
    fn search_advanced_default(
        engine: &SearchEngine,
        query: String,
        facets: Vec<String>,
        limit: u32,
        offset: u32,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        order: ResultsOrder,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
    ) -> Result<Vec<SearchResult>> {
        let negative_scope = same_search_scope(&scope);
        engine.search_advanced(
            query,
            String::new(),
            facets,
            limit,
            offset,
            distance,
            distance,
            custom_spacing,
            HashMap::new(),
            alternative_words,
            HashMap::new(),
            search_options,
            HashMap::new(),
            order,
            match_nikud,
            match_taamim,
            scope,
            negative_scope,
        )
    }

    fn same_search_scope(scope: &SearchScope) -> SearchScope {
        match scope {
            SearchScope::WordDistance => SearchScope::WordDistance,
            SearchScope::SameParagraph => SearchScope::SameParagraph,
            SearchScope::SameSection => SearchScope::SameSection,
        }
    }

    fn count_advanced_default(
        engine: &SearchEngine,
        query: String,
        facets: Vec<String>,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
    ) -> Result<u32> {
        let negative_scope = same_search_scope(&scope);
        engine.count_advanced(
            query,
            String::new(),
            facets,
            distance,
            distance,
            custom_spacing,
            HashMap::new(),
            alternative_words,
            HashMap::new(),
            search_options,
            HashMap::new(),
            match_nikud,
            match_taamim,
            scope,
            negative_scope,
        )
    }

    fn count_by_book_advanced_default(
        engine: &SearchEngine,
        query: String,
        facets: Vec<String>,
        distance: u32,
        custom_spacing: HashMap<String, String>,
        alternative_words: HashMap<u32, Vec<String>>,
        search_options: HashMap<String, HashMap<String, bool>>,
        match_nikud: bool,
        match_taamim: bool,
        scope: SearchScope,
    ) -> Result<HashMap<String, u32>> {
        let negative_scope = same_search_scope(&scope);
        engine.count_by_book_advanced(
            query,
            String::new(),
            facets,
            distance,
            distance,
            custom_spacing,
            HashMap::new(),
            alternative_words,
            HashMap::new(),
            search_options,
            HashMap::new(),
            match_nikud,
            match_taamim,
            scope,
            negative_scope,
        )
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
            INSERT INTO base (id, value) VALUES (1, 'הלכ'), (2, 'ישנ'), (3, 'אדמור');
            INSERT INTO surface (id, value, base_id) VALUES
                (1, 'הלכתי', 1),
                (2, 'הולכ', 1),
                (3, 'לכו', 1),
                (4, 'הולכימ', 1),
                (5, 'לישונ', 2),
                (6, 'ישנ', 2),
                (7, 'בלשונ', 2),
                -- מסמן את הפער השיורי של אופציה A (ראו §5.2 בתכנון): ה-DB
                -- מחזיק צורות נקיות בלבד, בעוד שהאינדקס עשוי לשאת אדמו"ר.
                (8, 'אדמור', 3),
                (9, 'אדמורימ', 3);
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
    fn sidecar_version_match_but_schema_drift_requires_rebuild() {
        // שחזור התקלה מהשטח: אינדקס שנבנה בגרסת-ביניים של אותה
        // schema_version (למשל `text` עם fast=true לפני ההסרה) — הקובץ
        // הצדדי מצהיר על הגרסה הנכונה, אבל open_or_create היה נופל על
        // SchemaError. הבדיקה חייבת לדרוש בנייה מחדש, לא "תואם".
        let dir = TempDir::new().unwrap();
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
        let drifted = schema_builder.build();
        Index::create_in_dir(dir.path(), drifted).unwrap();
        // הקובץ הצדדי נכתב במפורש עם הגרסה הנוכחית — כמו אינדקס שנבנה
        // ע"י גרסת-הביניים עצמה, שחתמה "3" עם הסכימה הישנה.
        write_current_index_metadata(dir.path()).unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible);
        assert_eq!(compatibility.status, "rebuild_required");
        assert_eq!(
            compatibility.found_schema_version,
            Some(INDEX_SCHEMA_VERSION)
        );
        assert!(compatibility
            .reason
            .unwrap()
            .contains("differs from the engine schema"));
    }

    #[test]
    fn valid_sidecar_without_tantivy_meta_requires_rebuild() {
        // sidecar תקין לבדו לא מספיק: בלי meta.json של tantivy (חסר או
        // פגום) פתיחת האינדקס תיכשל, ולכן הבדיקה חייבת לדרוש בנייה מחדש
        // ולא להחזיר "תואם" רק על סמך ההצהרה בקובץ הצדדי.
        let dir = TempDir::new().unwrap();
        write_current_index_metadata(dir.path()).unwrap();

        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible);
        assert_eq!(compatibility.status, "rebuild_required");
        assert_eq!(
            compatibility.found_schema_version,
            Some(INDEX_SCHEMA_VERSION)
        );
        assert!(compatibility
            .reason
            .unwrap()
            .contains("missing or unreadable"));

        // אותו דין ל-meta.json קיים אך פגום.
        fs::write(dir.path().join("meta.json"), "not json").unwrap();
        let compatibility = check_index_compatibility(dir_path_string(&dir));
        assert!(!compatibility.compatible);
        assert_eq!(compatibility.status, "rebuild_required");
        assert!(compatibility.reason.unwrap().contains("not valid JSON"));
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
            .add_document(
                id, "title", "ref", "/root", text, 0, false, file_path, None, None,
            )
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
    fn generation_order_can_prioritize_sources_before_commentaries() {
        let (mut engine, _dir) = make_engine();
        engine
            .add_document(
                10,
                "פירוש מוקדם בקטלוג",
                "ref",
                "/root",
                "שלום",
                0,
                false,
                "/books/commentary.txt",
                None,
                Some(65),
            )
            .unwrap();
        engine
            .add_document(
                30,
                "מקור שני",
                "ref",
                "/root",
                "שלום",
                0,
                false,
                "/books/source-b.txt",
                None,
                Some(2),
            )
            .unwrap();
        engine
            .add_document(
                20,
                "מקור ראשון",
                "ref",
                "/root",
                "שלום",
                0,
                false,
                "/books/source-a.txt",
                None,
                Some(2),
            )
            .unwrap();
        engine.commit().unwrap();

        let by_generation: Vec<u64> = engine
            .search_exact(
                "שלום".to_string(),
                vec!["/root".to_string()],
                10,
                0,
                ResultsOrder::Generation,
                false,
                false,
            )
            .unwrap()
            .into_iter()
            .map(|result| result.id)
            .collect();

        assert_eq!(by_generation, vec![20, 30, 10]);
    }

    #[test]
    fn add_text_book_builds_reference_trail_ids_and_fingerprint() {
        let (mut engine, _dir) = make_engine();
        let text =
            "<h1>ספר בראשית</h1>\n<h2>פרק א</h2>\nבְּרֵאשִׁית ברא אלהים\n<h2>פרק ב</h2>\nויכלו השמים";
        let added = engine
            .add_text_book(
                "בראשית".to_string(),
                "/root".to_string(),
                "/books/bereshit.txt".to_string(),
                5,
                DEFAULT_GENERATION_ORDER,
                text.to_string(),
            )
            .unwrap();
        assert_eq!(added, 5);
        engine.commit().unwrap();

        let results = engine
            .search_exact(
                "ויכלו".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        let hit = &results[0];
        // הכותרת החדשה של פרק ב החליפה את פרק א ב-trail (אותו prefix "<h2>").
        assert_eq!(hit.reference, "ספר בראשית, פרק ב");
        assert_eq!(hit.segment, 4);
        // id = ((catalogue_order+1) << 32) + ordinal+1, כמו ב-Dart.
        assert_eq!(hit.id, ((5u64 + 1) << 32) + 5);
        assert_eq!(hit.title, "בראשית");
        assert!(!hit.is_pdf);

        // הטקסט המאונדקס מנורמל (הניקוד הוסר) והחיפוש מוצא אותו.
        let nikud_hit = engine
            .search_exact(
                "בראשית ברא".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
            )
            .unwrap();
        assert_eq!(nikud_hit.len(), 1);

        // טביעת האצבע נחתמה על הספר, זהה לחישוב הציבורי על הטקסט הגולמי.
        let fingerprints = engine.get_book_fingerprints().unwrap();
        assert_eq!(
            fingerprints.get("/books/bereshit.txt"),
            Some(&compute_content_fingerprint(text.to_string()))
        );
    }

    #[test]
    fn add_text_book_bytes_matches_string_path() {
        // מסלול הבייטים (SQLite BLOB → Uint8List) חייב לייצר בדיוק את אותם
        // מסמכים ואותה טביעת אצבע כמו מסלול ה-String.
        let text =
            "<h1>ספר בראשית</h1>\n<h2>פרק א</h2>\nבְּרֵאשִׁית ברא אלהים\n<h2>פרק ב</h2>\nויכלו השמים";
        let (mut engine, _dir) = make_engine();
        let added = engine
            .add_text_book_bytes(
                "בראשית".to_string(),
                "/root".to_string(),
                "/books/bereshit.txt".to_string(),
                5,
                DEFAULT_GENERATION_ORDER,
                text.as_bytes().to_vec(),
            )
            .unwrap();
        assert_eq!(added, 5);
        engine.commit().unwrap();

        let results = engine
            .search_exact(
                "ויכלו".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reference, "ספר בראשית, פרק ב");
        assert_eq!(results[0].id, ((5u64 + 1) << 32) + 5);

        let fingerprints = engine.get_book_fingerprints().unwrap();
        assert_eq!(
            fingerprints.get("/books/bereshit.txt"),
            Some(&compute_content_fingerprint(text.to_string()))
        );
    }

    #[test]
    fn add_text_book_empty_text_adds_nothing() {
        let (mut engine, _dir) = make_engine();
        let added = engine
            .add_text_book(
                "ריק".to_string(),
                "/root".to_string(),
                "/books/empty.txt".to_string(),
                1,
                DEFAULT_GENERATION_ORDER,
                String::new(),
            )
            .unwrap();
        assert_eq!(added, 0);
        engine.commit().unwrap();
        assert_eq!(engine.get_document_count(), 0);
    }

    #[test]
    fn add_pdf_book_filters_garbage_and_encodes_ids() {
        let (mut engine, _dir) = make_engine();
        let pages = vec![
            PdfPageInput {
                reference: "ספר, עמוד 1".to_string(),
                text: "שורה ראשונה בעמוד\n\n≡≡≡ ∴∴∴ ⊕⊗⊘".to_string(),
                page_index: 0,
            },
            PdfPageInput {
                reference: "ספר, עמוד 2".to_string(),
                text: "בְּרֵאשִׁית ברא אלהים".to_string(),
                page_index: 1,
            },
        ];
        let added = engine
            .add_pdf_book(
                "ספר".to_string(),
                "/root".to_string(),
                "C:/books/sefer.pdf".to_string(),
                5,
                DEFAULT_GENERATION_ORDER,
                pages,
            )
            .unwrap();
        // השורה הריקה ושורת הסימנים סוננו כזבל — נותרו שתי שורות תוכן.
        assert_eq!(added, 2);
        engine.commit().unwrap();

        // הטקסט מנורמל לפני האינדוקס — שאילתה ללא ניקוד מוצאת אותו.
        let results = engine
            .search_exact(
                "בראשית ברא".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        let hit = &results[0];
        assert_eq!(hit.reference, "ספר, עמוד 2");
        // segment = אינדקס העמוד; ordinal רץ על שורות התוכן בלבד.
        assert_eq!(hit.segment, 1);
        assert_eq!(hit.id, ((5u64 + 1) << 32) + 2);
        assert!(hit.is_pdf);

        // ל-PDF אין טביעת אצבע — contentHash נחתם כ-0 (לא נרשם).
        let fingerprints = engine.get_book_fingerprints().unwrap();
        assert_eq!(fingerprints.get("C:/books/sefer.pdf"), Some(&0u64));
    }

    #[test]
    fn bulk_indexing_skips_merges_and_optimize_still_collapses() {
        let (mut engine, _dir) = make_engine();
        engine.set_bulk_indexing(true).unwrap();
        // כמה commit-ים ⇒ כמה סגמנטים; ב-bulk אין מיזוג רקע שמאחד אותם.
        for i in 0..3u64 {
            add(&mut engine, i + 1, "שלום עולם", "/books/a.txt");
            engine.commit().unwrap();
        }
        assert!(engine.index.searchable_segment_ids().unwrap().len() > 1);

        engine.set_bulk_indexing(false).unwrap();
        engine.optimize().unwrap();
        assert_eq!(engine.index.searchable_segment_ids().unwrap().len(), 1);
        assert_eq!(engine.get_document_count(), 3);
    }

    #[test]
    fn add_pdf_book_all_garbage_adds_nothing() {
        let (mut engine, _dir) = make_engine();
        let added = engine
            .add_pdf_book(
                "סרוק".to_string(),
                "/root".to_string(),
                "C:/books/scanned.pdf".to_string(),
                1,
                DEFAULT_GENERATION_ORDER,
                vec![PdfPageInput {
                    reference: "סרוק, עמוד 1".to_string(),
                    text: "\n≡≡≡≡≡\n∴ ⊕ ⊗ ⊘ ∴".to_string(),
                    page_index: 0,
                }],
            )
            .unwrap();
        assert_eq!(added, 0);
        engine.commit().unwrap();
        assert_eq!(engine.get_document_count(), 0);
    }

    #[test]
    fn exact_search_raw_maqaf_query_becomes_phrase() {
        // רגרסיה: strip_nikud לפני הטוקניזציה מחק את המקף והדביק
        // "אשר־שמע" לטרם בודד ("אשרשמע") שאינו קיים באינדקס.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ברוך אֲשֶׁר־שָׁמַע את הדבר", "/books/a.txt");
        add(&mut engine, 2, "אשר לא שמע דבר", "/books/b.txt");
        engine.commit().unwrap();

        let ids: Vec<u64> = engine
            .search_exact(
                "אשר־שמע".to_string(),
                vec![],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        // phrase של שתי מילים סמוכות — תופס את 1, לא את 2 (מילים מרוחקות).
        assert_eq!(ids, vec![1]);
    }

    // ── Single-word alternation splitting (per-branch DFA) ───────────────

    /// The exact pattern the generator builds for `משה` with "חלק ממילה" +
    /// "שגיאות כתיב": 48 wildcard-wrapped branches, 806 chars. As one regex it
    /// exceeded the upstream tantivy-fst 1 000-state DFA cap (the vendored
    /// copy raises it to 8 192); each branch alone is tiny.
    const BOTH_OPTIONS_PATTERN: &str = "(.{0,3}משה.{0,3}|.{0,3}מסה.{0,3}|.{0,2}משׁה.{0,2}|.{0,2}משׂה.{0,2}|.{0,3}משא.{0,3}|.{0,3}משע.{0,3}|.{0,3}משח.{0,3}|.{0,3}שמה.{0,3}|.{0,3}מהש.{0,3}|.{0,3}שה.{0,3}|.{0,3}מה.{0,3}|.{0,3}מש.{0,3}|.{0,2}ומשה.{0,2}|.{0,2}ימשה.{0,2}|.{0,2}אמשה.{0,2}|.{0,2}המשה.{0,2}|.{0,2}פמשה.{0,2}|.{0,2}למשה.{0,2}|.{0,2}ממשה.{0,2}|.{0,2}נמשה.{0,2}|.{0,2}במשה.{0,2}|.{0,2}כמשה.{0,2}|.{0,2}שמשה.{0,2}|.{0,2}תמשה.{0,2}|.{0,2}רמשה.{0,2}|.{0,2}משהו.{0,2}|.{0,2}משהי.{0,2}|.{0,2}משהא.{0,2}|.{0,2}משהה.{0,2}|.{0,2}משהפ.{0,2}|.{0,2}משהל.{0,2}|.{0,2}משהמ.{0,2}|.{0,2}משהנ.{0,2}|.{0,2}משהב.{0,2}|.{0,2}משהכ.{0,2}|.{0,2}משהש.{0,2}|.{0,2}משהת.{0,2}|.{0,2}משהר.{0,2}|.{0,2}מושה.{0,2}|.{0,2}מישה.{0,2}|.{0,2}מאשה.{0,2}|.{0,2}מהשה.{0,2}|.{0,2}מפשה.{0,2}|.{0,2}מלשה.{0,2}|.{0,2}מנשה.{0,2}|.{0,2}מבשה.{0,2}|.{0,2}מכשה.{0,2}|.{0,2}מששה.{0,2})";

    #[test]
    fn whole_pattern_compiles_under_vendored_state_limit_and_split_succeeds() {
        // Baseline for the vendored tantivy-fst patch: this combined pattern
        // could not compile as one DFA under the upstream 1 000-state cap
        // (the bug the per-branch split fixed). Under the vendored 8 192-state
        // cap it compiles again — the fact that unlocks the relaxed phrase
        // budgets, since a phrase word is compiled joined.
        assert!(
            tantivy_fst::Regex::new(BOTH_OPTIONS_PATTERN).is_ok(),
            "combined 48-branch pattern no longer compiles — did the vendored \
             tantivy-fst STATE_LIMIT patch get lost?"
        );
        // The split path answers the query either way.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר משה אל העם", "/books/a.txt");
        add(&mut engine, 2, "ספר בראשית", "/books/b.txt");
        engine.commit().unwrap();
        assert_eq!(search_ids(&mut engine, BOTH_OPTIONS_PATTERN), vec![1]);
    }

    #[test]
    fn both_options_pattern_now_returns_results() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר משה אל העם", "/books/a.txt");
        add(&mut engine, 2, "ספר בראשית", "/books/b.txt");
        engine.commit().unwrap();

        // The exact pattern built for `משה` with "חלק ממילה" + "שגיאות כתיב".
        // As a single regex it exceeded the upstream 1000-state limit; the
        // per-branch split must find the document regardless of the cap.
        assert_eq!(search_ids(&mut engine, BOTH_OPTIONS_PATTERN), vec![1]);
    }

    #[test]
    fn single_alternation_matches_same_as_combined_regex() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "משה", "/books/a.txt");
        add(&mut engine, 2, "מסה", "/books/b.txt");
        add(&mut engine, 3, "בראשית", "/books/c.txt");
        engine.commit().unwrap();

        // OR of two branches — finds both documents, not the third.
        let mut ids = search_ids(&mut engine, "(משה|מסה)");
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn split_matching_parity_with_whole_pattern() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "משה", "/books/a.txt");
        add(&mut engine, 2, "מסה", "/books/b.txt");
        add(&mut engine, 3, "בראשית", "/books/c.txt");
        engine.commit().unwrap();

        // Capturing and non-capturing wrappers split identically (R1).
        let mut capturing = search_ids(&mut engine, "(משה|מסה)");
        capturing.sort_unstable();
        let mut non_capturing = search_ids(&mut engine, "(?:משה|מסה)");
        non_capturing.sort_unstable();
        assert_eq!(capturing, vec![1, 2]);
        assert_eq!(capturing, non_capturing);

        // A leading empty branch (all-optional-letter word, R7) contributes
        // nothing: no indexed term is empty.
        assert_eq!(search_ids(&mut engine, "(?:|משה)"), vec![1]);

        // A nested (non-top-level) alternation still compiles whole and
        // matches the same set.
        let mut nested = search_ids(&mut engine, ".{0,1}(משה|מסה).{0,1}");
        nested.sort_unstable();
        assert_eq!(nested, vec![1, 2]);
    }

    #[test]
    fn char_class_and_escape_branches_match_end_to_end() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספרים", "/books/a.txt");
        add(&mut engine, 2, "אב", "/books/b.txt");
        add(&mut engine, 3, "shalom", "/books/c.txt");
        engine.commit().unwrap();

        // Top-level alternation between two groups, with `|` inside a char
        // class in the second branch — the class pipe must not be split on.
        let mut ids = search_ids(&mut engine, "([א-ת]{2,4}(ים|ות|ה)?)|([א-ת]+[יו][ם|ן])");
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);

        // A bare char class containing `|` is a single literal pattern.
        assert_eq!(search_ids(&mut engine, "[ם|ן]"), Vec::<u64>::new());
    }

    #[test]
    fn global_max_expansions_enforced_across_all_branches() {
        let (mut engine, _dir) = make_engine();
        // Terms matched by *different* branches, so only the shared union can
        // cross the cap — no single branch does.
        add(&mut engine, 1, "אאא", "/books/a.txt");
        add(&mut engine, 2, "בבב", "/books/b.txt");
        add(&mut engine, 3, "גגג", "/books/c.txt");
        engine.commit().unwrap();

        let run = |engine: &mut SearchEngine, max_expansions: u32| {
            engine.search(
                vec!["(אאא|בבב|גגג)".to_string()],
                vec!["/root".to_string()],
                100,
                0,
                0,
                max_expansions,
                ResultsOrder::Catalogue,
                None,
            )
        };
        // Union of 3 terms exceeds a cap of 2 — the single-word path degrades
        // instead of erroring, keeping the first branches' terms (branch
        // order is the priority contract).
        let ids: Vec<u64> = run(&mut engine, 2)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![1, 2]);
        // A cap of 3 fits the whole union.
        let ids: Vec<u64> = run(&mut engine, 3)
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn advanced_typo_partial_single_word_finds_document() {
        // End-to-end through the app path (search_advanced): the exact option
        // combination that used to blow the 1000-state DFA cap and silently
        // return nothing. Runs on the relaxed single-word budget.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר משה אל העם", "/books/a.txt");
        add(&mut engine, 2, "ספר בראשית", "/books/b.txt");
        engine.commit().unwrap();

        let mut options: HashMap<String, HashMap<String, bool>> = HashMap::new();
        options.insert(
            "משה_0".to_string(),
            HashMap::from([
                ("שגיאות כתיב".to_string(), true),
                ("חלק ממילה".to_string(), true),
            ]),
        );
        let ids: Vec<u64> = search_advanced_default(
            &engine,
            "משה".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn advanced_typo_only_single_word_covers_full_edit_distance() {
        // typo-only single word expands through a Levenshtein-1 automaton
        // scan (VARIATION_CEILING_RESEARCH §3.ג): full edit-distance-1
        // coverage, a superset of the historical literal variant list.
        // "ספגר" — an insertion of ג, which is NOT in INSERTION_LETTERS — was
        // invisible to the sampled variants and must now match; a distance-2
        // word must not.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר בראשית", "/books/a.txt");
        add(&mut engine, 2, "ספגר אחר", "/books/b.txt");
        add(&mut engine, 3, "תורה צוה", "/books/c.txt");
        engine.commit().unwrap();

        let mut options: HashMap<String, HashMap<String, bool>> = HashMap::new();
        options.insert(
            "ספר_0".to_string(),
            HashMap::from([("שגיאות כתיב".to_string(), true)]),
        );
        let mut ids: Vec<u64> = search_advanced_default(
            &engine,
            "ספר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
        ids.sort_unstable();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn invalid_branch_pattern_surfaces_error() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "משה", "/books/a.txt");
        engine.commit().unwrap();

        // One malformed branch must fail the search loudly, not silently
        // return nothing.
        let result = engine.search(
            vec!["משה|[".to_string()],
            vec!["/root".to_string()],
            100,
            0,
            0,
            100,
            ResultsOrder::Catalogue,
            None,
        );
        assert!(result.is_err());
    }

    // ── Snippet phrase-highlight filtering (results view ↔ search parity) ─

    /// Counts highlight spans (the `<font color=red>` opening tag the default
    /// `HighlightConfig` emits and the app's snippet parser expects).
    fn highlight_count(text: &str) -> usize {
        text.matches("<font color=red>").count()
    }

    #[test]
    fn advanced_phrase_snippet_highlights_only_adjacent_occurrence() {
        // The reported bug: searching the phrase "משה ואהרן" (no spacing) must
        // NOT light up a lone "משה" that is not followed by "ואהרן". tantivy's
        // term-based SnippetGenerator paints all three occurrences; the phrase
        // filter keeps only the adjacent pair.
        let (mut engine, _dir) = make_engine();
        add(
            &mut engine,
            1,
            "משה ואהרן אמרו שלום ואחר כך משה כהן הלך",
            "/books/a.txt",
        );
        engine.commit().unwrap();

        let results = search_advanced_default(
            &engine,
            "משה ואהרן".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        let text = &results[0].text;
        // Exactly the two words of the one adjacent phrase — not the lone משה.
        assert_eq!(highlight_count(text), 2, "snippet: {text}");
        assert!(text.contains("<font color=red>משה</font>"));
        assert!(text.contains("<font color=red>ואהרן</font>"));
        // The stray occurrence stays plain.
        assert!(text.contains("כך משה כהן"), "snippet: {text}");
    }

    #[test]
    fn advanced_single_word_snippet_still_highlights_every_occurrence() {
        // A single word is not a phrase: every occurrence is a real hit and must
        // stay highlighted — the filter only constrains multi-word phrases.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "משה דיבר ואחר כך משה שתק", "/books/a.txt");
        engine.commit().unwrap();

        let results = search_advanced_default(
            &engine,
            "משה".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(
            highlight_count(&results[0].text),
            2,
            "snippet: {}",
            results[0].text
        );
    }

    #[test]
    fn exact_phrase_snippet_highlights_only_adjacent_occurrence() {
        // The exact (PhraseQuery) path has the same term-based over-painting; a
        // strict-adjacency (all-zero gaps) filter drops the lone occurrence too.
        let (mut engine, _dir) = make_engine();
        add(
            &mut engine,
            1,
            "משה ואהרן אמרו שלום ואחר כך משה כהן הלך",
            "/books/a.txt",
        );
        engine.commit().unwrap();

        let results = engine
            .search_exact(
                "משה ואהרן".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        let text = &results[0].text;
        assert_eq!(highlight_count(text), 2, "snippet: {text}");
        assert!(text.contains("כך משה כהן"), "snippet: {text}");
    }

    #[test]
    fn advanced_phrase_custom_spacing_gap_is_highlighted_not_dropped() {
        // `distance` is 0 but `custom_spacing` permits one intermediate word, so
        // the search matches "משה <word> ואהרן". The filter's gap allowance must
        // come from the resolved gaps (= the custom spacing), NOT the raw
        // `distance`: with `distance` it would find no occurrence, fall back to
        // the plain term highlight, and re-paint the lone trailing "משה" (3
        // spans). With the gaps it paints exactly the gapped phrase (2 spans).
        let (mut engine, _dir) = make_engine();
        add(
            &mut engine,
            1,
            "משה רבנו ואהרן אמר ואחר כך משה לבדו הלך",
            "/books/a.txt",
        );
        engine.commit().unwrap();

        let results = search_advanced_default(
            &engine,
            "משה ואהרן".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,                                                     // distance 0 …
            HashMap::from([("0-1".to_string(), "1".to_string())]), // … but spacing allows 1
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        let text = &results[0].text;
        assert_eq!(highlight_count(text), 2, "snippet: {text}");
        assert!(text.contains("<font color=red>משה</font>"));
        assert!(text.contains("<font color=red>ואהרן</font>"));
        // The stray trailing occurrence stays plain.
        assert!(text.contains("כך משה לבדו"), "snippet: {text}");
    }

    // ── Per-pair gap enforcement (GapVerifiedPhraseQuery) ─────────────────

    #[test]
    fn advanced_custom_spacing_is_enforced_per_pair() {
        // spacing = {0-1: 2, 1-2: 0}: up to two words between ויאמר and אל,
        // but אל and משה must be adjacent. Historically the engine collapsed
        // this to one global slop, so doc 2 — where the allowance is "spent"
        // in the wrong gap — also matched.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר ה' צבאות אל משה", "/books/a.txt");
        add(&mut engine, 2, "ויאמר אל העם ואל משה", "/books/b.txt");
        engine.commit().unwrap();

        let spacing = HashMap::from([
            ("0-1".to_string(), "2".to_string()),
            ("1-2".to_string(), "0".to_string()),
        ]);
        let ids: Vec<u64> = search_advanced_default(
            &engine,
            "ויאמר אל משה".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            spacing.clone(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(ids, vec![1]);

        // The counting path runs through the same verified query.
        let count = count_advanced_default(
            &engine,
            "ויאמר אל משה".to_string(),
            vec!["/root".to_string()],
            0,
            spacing,
            HashMap::new(),
            HashMap::new(),
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn advanced_distance_allows_full_gap_in_every_pair() {
        // distance = 2 means "up to two words between EACH adjacent pair".
        // tantivy's slop is a cumulative budget, so passing the raw distance
        // used to reject a match that uses its allowance in both gaps at once.
        let (mut engine, _dir) = make_engine();
        add(
            &mut engine,
            1,
            "ויאמר ה' צבאות אל בני ישראל משה",
            "/books/a.txt",
        );
        // One gap over the allowance must still be rejected.
        add(
            &mut engine,
            2,
            "ויאמר אל דוד המלך איש הבינים משה",
            "/books/b.txt",
        );
        engine.commit().unwrap();

        let ids: Vec<u64> = search_advanced_default(
            &engine,
            "ויאמר אל משה".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            2,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn advanced_phrase_requires_query_word_order() {
        // tantivy's sloppy phrase matches unordered (positions compare by
        // abs_diff); the gap verifier restores the in-order semantics every
        // comment, highlight filter, and display pattern already promise.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר להם משה", "/books/a.txt");
        // Reversed within the slop budget: tantivy's phrase scorer accepts
        // it, only the gap verifier rejects it.
        add(&mut engine, 2, "אמר משה ויאמר", "/books/b.txt");
        engine.commit().unwrap();

        let ids: Vec<u64> = search_advanced_default(
            &engine,
            "ויאמר משה".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            2,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn advanced_per_pair_gap_verifies_across_repeated_words() {
        // The feasibility sweep must consider EVERY chain start, not just the
        // earliest: here the first "אל" satisfies pair 0 but only the second
        // one can reach "משה" within pair 1's allowance.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר אל העם ושוב אל משה", "/books/a.txt");
        engine.commit().unwrap();

        let spacing = HashMap::from([
            ("0-1".to_string(), "3".to_string()),
            ("1-2".to_string(), "0".to_string()),
        ]);
        let ids: Vec<u64> = search_advanced_default(
            &engine,
            "ויאמר אל משה".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            spacing,
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(ids, vec![1]);
    }

    #[test]
    fn advanced_per_pair_spacing_composes_with_word_options() {
        // Per-pair enforcement must hold when a word expands through an
        // option (grammatical prefixes): "ולמשה" matches the expanded word
        // pattern, and the pair allowances still gate the match.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר שוב אל ולמשה", "/books/a.txt");
        add(&mut engine, 2, "ויאמר אל העם ולמשה", "/books/b.txt");
        engine.commit().unwrap();

        let options: HashMap<String, HashMap<String, bool>> = HashMap::from([(
            "משה_2".to_string(),
            HashMap::from([("קידומות דקדוקיות".to_string(), true)]),
        )]);
        let spacing = HashMap::from([
            ("0-1".to_string(), "1".to_string()),
            ("1-2".to_string(), "0".to_string()),
        ]);
        let ids: Vec<u64> = search_advanced_default(
            &engine,
            "ויאמר אל משה".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            spacing,
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
        assert_eq!(ids, vec![1]);
    }

    // ── Index-aware display highlight (parity with search, R5) ───────────

    /// Charwise display form of a nikud-free term, as the Dart layer receives
    /// it (each Hebrew letter may carry attached marks in displayed text, and
    /// between letters optional geresh/gershayim — the quote-free twin token).
    fn charwise(term: &str) -> String {
        let mut out = String::new();
        for (i, c) in term.chars().enumerate() {
            if i > 0 {
                out.push_str(crate::display_highlight::OPTIONAL_QUOTES);
            }
            out.push(c);
            out.push_str(crate::display_highlight::ATTACHED_MARKS_CLASS);
        }
        out
    }

    fn typo_options(word: &str) -> HashMap<String, HashMap<String, bool>> {
        HashMap::from([(
            format!("{word}_0"),
            HashMap::from([("שגיאות כתיב".to_string(), true)]),
        )])
    }

    #[test]
    fn index_highlight_paints_typo_matched_term() {
        let (mut engine, _dir) = make_engine();
        // The document is findable only via the typo variant מסה — the
        // query-shape pattern (which knows only משה + spelling) would leave
        // it with no highlight at all.
        add(&mut engine, 1, "ויקח מסה גדולה", "/books/a.txt");
        engine.commit().unwrap();

        let hl = engine
            .generate_index_highlight_pattern(
                "משה".to_string(),
                0,
                HashMap::new(),
                HashMap::new(),
                typo_options("משה"),
            )
            .unwrap()
            .expect("pattern");
        // Only מסה exists in this index, so it is the single branch.
        assert_eq!(hl.combined_pattern, charwise("מסה"));
        assert_eq!(hl.word_boundary_eligible, vec![true]);
    }

    #[test]
    fn index_highlight_partial_word_paints_whole_token_with_boundaries() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "הספרים הקדושים", "/books/a.txt");
        add(&mut engine, 2, "ספר תורה", "/books/b.txt");
        engine.commit().unwrap();

        let options = HashMap::from([(
            "ספר_0".to_string(),
            HashMap::from([("חלק ממילה".to_string(), true)]),
        )]);
        let hl = engine
            .generate_index_highlight_pattern(
                "ספר".to_string(),
                0,
                HashMap::new(),
                HashMap::new(),
                options,
            )
            .unwrap()
            .expect("pattern");

        // Both matched tokens are branches, longest first, and — unlike the
        // query-shape path, which waives boundaries for "חלק ממילה" — the
        // whole inflected word highlights under token boundaries.
        assert!(hl.combined_pattern.contains(&charwise("הספרים")));
        assert!(hl.combined_pattern.contains(&charwise("ספר")));
        assert!(
            hl.combined_pattern.find(&charwise("הספרים")).unwrap()
                < hl.combined_pattern
                    .find(&format!("|{}", charwise("ספר")))
                    .unwrap(),
            "longer term must precede its prefix in the alternation"
        );
        assert_eq!(hl.word_boundary_eligible, vec![true]);
    }

    #[test]
    fn index_highlight_falls_back_to_query_shape_when_nothing_matches() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "בראשית ברא", "/books/a.txt");
        engine.commit().unwrap();

        let from_index = engine
            .generate_index_highlight_pattern(
                "משה".to_string(),
                0,
                HashMap::new(),
                HashMap::new(),
                typo_options("משה"),
            )
            .unwrap()
            .expect("pattern");
        let query_shape = generate_highlight_pattern(
            "משה".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            typo_options("משה"),
        )
        .expect("pattern");
        // Nothing in the index matches → never worse than the pure-string
        // pattern the app used until now.
        assert_eq!(from_index.combined_pattern, query_shape.combined_pattern);
        assert_eq!(
            from_index.word_boundary_eligible,
            query_shape.word_boundary_eligible
        );
    }

    #[test]
    fn index_highlight_multi_word_paints_each_words_variants() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויאמר מסה אל העם", "/books/a.txt");
        engine.commit().unwrap();

        let options = HashMap::from([(
            "משה_1".to_string(),
            HashMap::from([("שגיאות כתיב".to_string(), true)]),
        )]);
        let hl = engine
            .generate_index_highlight_pattern(
                "ויאמר משה".to_string(),
                0,
                HashMap::new(),
                HashMap::new(),
                options,
            )
            .unwrap()
            .expect("pattern");

        assert_eq!(hl.word_patterns.len(), 2);
        assert_eq!(hl.word_patterns[0], charwise("ויאמר"));
        assert_eq!(hl.word_patterns[1], charwise("מסה"));
        // Combined phrase pattern chains both words.
        assert!(hl.combined_pattern.starts_with(&charwise("ויאמר")));
        assert!(hl.combined_pattern.ends_with(&charwise("מסה")));
    }

    #[test]
    fn index_fuzzy_highlight_paints_edit_distance_matches() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ויקח מסה גדולה", "/books/a.txt");
        engine.commit().unwrap();

        let hl = engine
            .generate_index_fuzzy_highlight_pattern("משה".to_string(), 1)
            .unwrap()
            .expect("pattern");
        // The edit-distance-1 match from the index plus the literal token.
        assert!(hl.combined_pattern.contains(&charwise("מסה")));
        assert!(hl.combined_pattern.contains(&charwise("משה")));

        // Distance above the FuzzyTermQuery limit is rejected loudly.
        assert!(engine
            .generate_index_fuzzy_highlight_pattern("משה".to_string(), 3)
            .is_err());
    }

    #[test]
    fn index_highlight_plain_query_matches_exact_token() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();

        // No options at all (the plain path of prepare_advanced_query).
        let hl = engine
            .generate_index_highlight_pattern(
                "שלום".to_string(),
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
            )
            .unwrap()
            .expect("pattern");
        assert_eq!(hl.combined_pattern, charwise("שלום"));
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
                None,
                None,
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
    fn compute_content_fingerprint_is_stable_and_never_zero() {
        let a = compute_content_fingerprint("בראשית ברא".to_string());
        let b = compute_content_fingerprint("בראשית ברא".to_string());
        let c = compute_content_fingerprint("בראשית ברה".to_string());
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, 0);
        assert_ne!(compute_content_fingerprint(String::new()), 0);
    }

    fn fingerprint_doc(id: u64, text: &str, file_path: &str, hash: Option<u64>) -> DocumentInput {
        DocumentInput {
            id,
            title: "ספר".to_string(),
            reference: "ref".to_string(),
            topics: "/root".to_string(),
            text: text.to_string(),
            segment: 0,
            is_pdf: false,
            file_path: file_path.to_string(),
            content_hash: hash,
            text_vocalized: None,
            section_id: None,
            generation_order: None,
        }
    }

    #[test]
    fn get_book_fingerprints_returns_per_book_hash() {
        let (mut engine, _dir) = make_engine();
        let hash_a = compute_content_fingerprint("ספר א".to_string());
        let hash_b = compute_content_fingerprint("ספר ב".to_string());
        engine
            .add_documents_batch(vec![
                fingerprint_doc(1, "שורה ראשונה", "id:1", Some(hash_a)),
                fingerprint_doc(2, "שורה שנייה", "id:1", Some(hash_a)),
                fingerprint_doc(3, "טקסט אחר", "id:2", Some(hash_b)),
                // PDF-כמו: ללא טביעת אצבע — צריך להופיע כ-0.
                fingerprint_doc(4, "עמוד", "C:/books/a.pdf", None),
            ])
            .unwrap();
        engine.commit().unwrap();

        let fingerprints = engine.get_book_fingerprints().unwrap();
        assert_eq!(fingerprints.get("id:1"), Some(&hash_a));
        assert_eq!(fingerprints.get("id:2"), Some(&hash_b));
        assert_eq!(fingerprints.get("C:/books/a.pdf"), Some(&0));
    }

    #[test]
    fn get_book_fingerprints_reflects_reindex_and_delete() {
        let (mut engine, _dir) = make_engine();
        let old_hash = compute_content_fingerprint("ישן".to_string());
        let new_hash = compute_content_fingerprint("חדש".to_string());
        engine
            .add_documents_batch(vec![
                fingerprint_doc(1, "ישן", "id:1", Some(old_hash)),
                fingerprint_doc(2, "אחר", "id:2", Some(old_hash)),
            ])
            .unwrap();
        engine.commit().unwrap();

        // אינדוקס מחדש של ספר: מחיקה לפי כותרת קודם הייתה מוחקת את שניהם —
        // כאן מדמים דרך upsert לפי id של מסמכי הספר בלבד.
        engine
            .upsert_documents_batch(vec![fingerprint_doc(1, "חדש", "id:1", Some(new_hash))])
            .unwrap();
        engine.commit().unwrap();

        let fingerprints = engine.get_book_fingerprints().unwrap();
        assert_eq!(fingerprints.get("id:1"), Some(&new_hash));
        assert_eq!(fingerprints.get("id:2"), Some(&old_hash));

        engine.remove_documents_by_title("ספר").unwrap();
        engine.commit().unwrap();
        assert!(engine.get_book_fingerprints().unwrap().is_empty());
    }

    #[test]
    fn delete_documents_by_file_path_removes_only_that_book() {
        let (mut engine, _dir) = make_engine();
        engine
            .add_documents_batch(vec![
                fingerprint_doc(1, "שורה ראשונה", "uid:1", Some(3)),
                fingerprint_doc(2, "שורה שנייה", "uid:1", Some(3)),
                fingerprint_doc(3, "טקסט אחר", "id:2", Some(5)),
            ])
            .unwrap();
        engine.commit().unwrap();

        engine.delete_documents_by_file_path("uid:1").unwrap();
        engine.commit().unwrap();

        // כל מסמכי uid:1 נמחקו — הספר האחר (בעל אותה כותרת) לא נפגע.
        let counts = engine.count_documents_by_file_path().unwrap();
        assert_eq!(counts.get("uid:1"), None);
        assert_eq!(counts.get("id:2"), Some(&1));

        // הצורה הקבוצתית מוחקת כמה ספרים בקריאה אחת.
        engine
            .delete_documents_by_file_paths(vec!["id:2".to_string(), "uid:404".to_string()])
            .unwrap();
        engine.commit().unwrap();
        assert!(engine.count_documents_by_file_path().unwrap().is_empty());
    }

    #[test]
    fn get_book_fingerprints_conflicting_docs_collapse_to_zero() {
        let (mut engine, _dir) = make_engine();
        engine
            .add_documents_batch(vec![
                fingerprint_doc(1, "שורה", "id:1", Some(7)),
                fingerprint_doc(2, "שורה", "id:1", Some(9)),
            ])
            .unwrap();
        engine.commit().unwrap();

        let fingerprints = engine.get_book_fingerprints().unwrap();
        assert_eq!(fingerprints.get("id:1"), Some(&0));
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
                false,
                false,
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
                false,
                false,
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
                false,
                false,
            )
            .unwrap();
        assert_eq!(fuzzy_lex.len(), 1, "lexical fuzzy must find the inflection");
        assert!(fuzzy_lex[0].text.contains("הלכתי"));

        // count_fuzzy must agree with search_fuzzy (same matching logic).
        let count = engine
            .count_fuzzy(
                "הלך".to_string(),
                vec!["/root".to_string()],
                2,
                false,
                false,
            )
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
                false,
                false,
            )
            .unwrap();
        assert!(
            fuzzy_zero.is_empty(),
            "max_distance=0 must not inject lexical expansions"
        );

        let count = engine
            .count_fuzzy(
                "הלך".to_string(),
                vec!["/root".to_string()],
                0,
                false,
                false,
            )
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
                false,
                false,
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
            .count_fuzzy(
                "הלך".to_string(),
                vec!["/root".to_string()],
                2,
                false,
                false,
            )
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
                false,
                false,
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
                false,
                false,
            )
            .unwrap());
        assert_eq!(
            got,
            vec![1, 5],
            "multi-token lexical fuzzy search should preserve order while allowing one intervening token"
        );

        let count = engine
            .count_fuzzy(
                "הלכתי לישון".to_string(),
                vec!["/root".to_string()],
                2,
                false,
                false,
            )
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
                false,
                false,
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
                false,
                false,
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
                None,
                None,
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
            .add_document(
                1,
                "title",
                "ref",
                "/root",
                "שלום",
                0,
                false,
                "/books/a.txt",
                None,
                None,
            )
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
                false,
                false,
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
                false,
                false,
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
                false,
                false,
            )
            .unwrap());
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn test_stored_text_keeps_punctuation_and_highlight_lands_on_it() {
        let (mut engine, _dir) = make_engine();
        // מקצה-לקצה של issue #446/#500: הטקסט השמור משמר פיסוק ונקי מניקוד
        // ומ-Presentation Forms, ושאילתה רגילה מוצאת ומדגישה אותו.
        let raw = "וּבָזֶה יוּבַן שַׁ\"ס (עא:) הַמְמַלֵּא גְּרוֹנָם, מ\u{FB1D}ם וכו'";
        let stored = crate::hebrew_query::normalize_text_for_indexing(raw);
        assert_eq!(stored, "ובזה יובן ש\"ס (עא:) הממלא גרונם, מים וכו'");
        engine
            .add_document(
                1,
                "title",
                "ref",
                "/root",
                &stored,
                0,
                false,
                "/books/a.txt",
                None,
                None,
            )
            .unwrap();
        engine.commit().unwrap();

        for query in ["הממלא", "מים", "עא"] {
            let results = engine
                .search_exact(
                    query.to_string(),
                    vec!["/root".to_string()],
                    100,
                    0,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                )
                .unwrap();
            assert_eq!(ids(results.clone()), vec![1], "no hit for {query}");
            assert!(
                results[0]
                    .text
                    .contains(&format!("<font color=red>{query}</font>")),
                "highlight missing for {query}: {}",
                results[0].text
            );
        }
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

        let got = ids(search_advanced_default(
            &engine,
            "ספר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
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

        let result = search_advanced_default(
            &engine,
            "ספר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
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

        // Two index terms match; a cap of 1 truncates the term set (degrade,
        // not error) — exactly one document comes back. Which term survives
        // depends on segment order (each doc may land in its own segment),
        // so only the count is pinned.
        let truncated = ids(engine
            .search(
                vec![".*ספר".to_string()],
                vec![],
                100,
                0,
                0,
                1,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap());
        assert_eq!(
            truncated.len(),
            1,
            "a cap of 1 should serve exactly one term's documents, got {truncated:?}"
        );
        assert!(
            truncated[0] == 1 || truncated[0] == 2,
            "unexpected document {truncated:?}"
        );

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
    fn test_single_term_truncation_flag_surfaces() {
        // The degrade path must report itself so the stream can flag partial
        // results to the UI, instead of silently serving a subset.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        add(&mut engine, 2, "הספר", "/books/b.txt");
        engine.commit().unwrap();

        // Two terms match `.*ספר`; a cap of 1 stops collection early.
        let (_, truncated) = engine
            .build_query(vec![".*ספר".to_string()], vec![], 0, 1)
            .unwrap();
        assert!(
            truncated,
            "a cap of 1 over two matching terms must report truncation"
        );

        // A generous cap collects everything — no degrade, no flag.
        let (_, not_truncated) = engine
            .build_query(vec![".*ספר".to_string()], vec![], 0, 100)
            .unwrap();
        assert!(
            !not_truncated,
            "a cap that fits every term must not report truncation"
        );
    }

    #[test]
    fn test_search_and_count_propagates_truncation_flag() {
        // The page/count API must surface the same degrade signal the stream
        // does — dropping it lets a consumer show partial results unwarned.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        add(&mut engine, 2, "הספר", "/books/b.txt");
        engine.commit().unwrap();

        let page = engine
            .search_and_count(
                vec![".*ספר".to_string()],
                vec![],
                100,
                0,
                0,
                1,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap();
        assert!(
            page.truncated,
            "a cap of 1 over two matching terms must flag the page result"
        );

        let page = engine
            .search_and_count(
                vec![".*ספר".to_string()],
                vec![],
                100,
                0,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
            )
            .unwrap();
        assert!(!page.truncated, "an uncapped query must not flag the page");
        assert_eq!(page.total_count, 2);
    }

    #[test]
    fn test_count_apis_with_status_surface_truncation() {
        // count/count_by_book/get_facet_counts feed the facet filter tree; the
        // *_with_status variants must carry the same degrade signal so a
        // consumer can flag partial counts instead of showing them as exact.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר", "/books/a.txt");
        add(&mut engine, 2, "הספר", "/books/b.txt");
        engine.commit().unwrap();

        let capped = engine
            .count_with_status(vec![".*ספר".to_string()], &[], 0, 1)
            .unwrap();
        assert!(capped.truncated, "a cap of 1 must flag count_with_status");

        let books = engine
            .count_by_book_with_status(vec![".*ספר".to_string()], vec![], 0, 1)
            .unwrap();
        assert!(books.truncated, "a cap of 1 must flag the per-book counts");

        let facets = engine
            .get_facet_counts_with_status(vec![".*ספר".to_string()], vec![], "/".to_string(), 0, 1)
            .unwrap();
        assert!(facets.truncated, "a cap of 1 must flag the facet counts");

        let uncapped = engine
            .count_with_status(vec![".*ספר".to_string()], &[], 0, 100)
            .unwrap();
        assert!(!uncapped.truncated, "an uncapped count must not flag");
        assert_eq!(uncapped.count, 2);

        let facets = engine
            .get_facet_counts_with_status(
                vec![".*ספר".to_string()],
                vec![],
                "/".to_string(),
                0,
                100,
            )
            .unwrap();
        assert!(!facets.truncated, "an uncapped facet count must not flag");
        assert!(
            facets
                .counts
                .iter()
                .any(|f| f.path == "/root" && f.count == 2),
            "facet counts should be exact when not truncated"
        );

        // The bare API stays backward compatible: same count, flag dropped.
        let bare = engine
            .count(vec![".*ספר".to_string()], &[], 0, 100)
            .unwrap();
        assert_eq!(bare, uncapped.count);
    }

    #[test]
    fn test_advanced_count_apis_with_status_surface_truncation() {
        // The advanced *_with_status trio must propagate the degrade signal
        // through build_advanced_query. "חלק ממילה" on a one-letter word gets
        // the relaxed 20 000-term cap (plain_max_expansions), so an index
        // with 20 812 matching terms overflows it.
        let (mut engine, _dir) = make_engine();
        let letters = [
            'א', 'ב', 'ג', 'ד', 'ה', 'ו', 'ז', 'ח', 'ט', 'י', 'כ', 'ל', 'מ', 'נ', 'ס', 'ע', 'פ',
            'צ', 'ק', 'ר', 'ש', 'ת',
        ];
        // 22³ words starting with א plus 21×22² with א second — all distinct,
        // all inside the `.{0,3}א.{0,3}` partial window.
        let mut words: Vec<String> = Vec::new();
        for a in letters {
            for b in letters {
                for c in letters {
                    words.push(format!("א{a}{b}{c}"));
                    if a != 'א' {
                        words.push(format!("{a}א{b}{c}"));
                    }
                }
            }
        }
        for (i, chunk) in words.chunks(4_000).enumerate() {
            add(&mut engine, i as u64 + 1, &chunk.join(" "), "/books/a.txt");
        }
        // Control docs for the under-cap path (no א anywhere).
        add(&mut engine, 100, "שלום עולם", "/books/b.txt");
        add(&mut engine, 101, "שלום", "/books/c.txt");
        engine.commit().unwrap();

        let partial_on = |word: &str| -> HashMap<String, HashMap<String, bool>> {
            HashMap::from([(
                format!("{word}_0"),
                HashMap::from([("חלק ממילה".to_string(), true)]),
            )])
        };

        let count = engine
            .count_advanced_with_status(
                "א".to_string(),
                String::new(),
                vec!["/root".to_string()],
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                partial_on("א"),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
            )
            .unwrap();
        assert!(
            count.truncated,
            "20 812 matching terms over the 20 000 cap must flag the advanced count"
        );

        let books = engine
            .count_by_book_advanced_with_status(
                "א".to_string(),
                String::new(),
                vec!["/root".to_string()],
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                partial_on("א"),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
            )
            .unwrap();
        assert!(
            books.truncated,
            "the advanced per-book counts must carry the same flag"
        );

        let facets = engine
            .get_facet_counts_advanced_with_status(
                "א".to_string(),
                String::new(),
                vec!["/root".to_string()],
                "/".to_string(),
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                partial_on("א"),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
            )
            .unwrap();
        assert!(
            facets.truncated,
            "the advanced facet counts must carry the same flag"
        );

        // A word matching a single term stays far under the cap: no flag,
        // exact counts on every path.
        let count = engine
            .count_advanced_with_status(
                "שלום".to_string(),
                String::new(),
                vec!["/root".to_string()],
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                partial_on("שלום"),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
            )
            .unwrap();
        assert!(!count.truncated, "under the cap the count must not flag");
        assert_eq!(count.count, 2, "both control documents match");

        let facets = engine
            .get_facet_counts_advanced_with_status(
                "שלום".to_string(),
                String::new(),
                vec!["/root".to_string()],
                "/".to_string(),
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                partial_on("שלום"),
                HashMap::new(),
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
            )
            .unwrap();
        assert!(!facets.truncated, "under the cap the facets must not flag");
        assert!(
            facets
                .counts
                .iter()
                .any(|f| f.path == "/root" && f.count == 2),
            "facet counts should be exact when not truncated"
        );
    }

    #[test]
    fn test_search_advanced_strips_query_nikud() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "ספר תורה", "/books/a.txt");
        engine.commit().unwrap();

        // Pasted vocalized text must still match the nikud-free index terms.
        let got = ids(search_advanced_default(
            &engine,
            "סֵפֶר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
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
            let results = search_advanced_default(
                &engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
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

        let results = search_advanced_default(
            &engine,
            "ספר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
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
        let got = ids(search_advanced_default(
            &engine,
            "שר".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            alts,
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap());
        assert_eq!(got, vec![1, 2], "alternatives should OR שר with מלך");
    }

    #[test]
    fn advanced_search_matches_gershayim_tokens_end_to_end() {
        // HebrewTokenizer שומר גרשיים וגרש פנימי בטוקן (ז"ל, רמב"ם — טרם
        // אחד); split_query_words חייב לפצל את השאילתה באותה צורה — אחרת
        // ז"ל לעולם לא יימצא. כולל נורמליזציה של ״ עברי משני הצדדים
        // וקיפול זוג-הגרשים (רמב''ם) בשאילתה.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "הרב פלוני ז\"ל אמר", "/books/a.txt");
        add(&mut engine, 2, "כתב הרמב\u{05F4}ם בהלכות", "/books/b.txt");
        add(&mut engine, 3, "דברי תוס' שם", "/books/c.txt");
        add(&mut engine, 4, "מסמך עם רמב ם כשתי מילים", "/books/d.txt");
        add(&mut engine, 5, "אמר ג'ורג' לד'אש", "/books/e.txt");
        engine.commit().unwrap();

        let advanced_ids = |engine: &mut SearchEngine, query: &str| {
            ids(search_advanced_default(
                &engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
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
        assert_eq!(
            advanced_ids(&mut engine, "הרמב''ם"),
            vec![2],
            "זוג גרשים בשאילתה (מוסכמת קבצים ישנים)"
        );
        assert_eq!(advanced_ids(&mut engine, "תוס'"), vec![3], "גרש סופי");
        assert_eq!(
            advanced_ids(&mut engine, "ג'ורג'"),
            vec![5],
            "גרש פנימי + סופי"
        );
        assert!(
            !advanced_ids(&mut engine, "רמב\"ם").contains(&4),
            "רמב ם כשתי מילים אינו צירוף מקרי של רמב\"ם"
        );
    }

    #[test]
    fn exact_search_gershayim_token_is_a_single_term() {
        // כל צורות הדפוס של השאילתה מתלכדות לטרם `רמב"ם` אחד ומוצאות
        // מסמך שנדפס ב-״; צירוף מקרי `רמב ם` (שתי מילים) לא נתפס עוד —
        // זו בדיוק מטרת השינוי. המחיר המקובל (D1): שאילתה נטולת-גרשיים
        // לא מוצאת את המהדורה המנוקדת-בגרשיים בחיפוש מדויק.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "דברי רמב\u{05F4}ם בהלכות", "/books/a.txt");
        add(&mut engine, 2, "צירוף רמב ם מקרי", "/books/b.txt");
        engine.commit().unwrap();

        let exact_ids = |engine: &SearchEngine, query: &str| {
            ids(engine
                .search_exact(
                    query.to_string(),
                    vec![],
                    100,
                    0,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                )
                .unwrap())
        };

        for query in ["רמב\"ם", "רמב\u{05F4}ם", "רמב''ם"] {
            assert_eq!(exact_ids(&engine, query), vec![1], "query {query:?}");
        }
        // המחיר ההיסטורי של אופציה A (מדויק רגיש-גרשיים) בוטל: האינדקס
        // מטמיע לכל מילת-גרשיים גם טוקן-תאום נקי, כך ששאילתה נטולת-גרשיים
        // מוצאת את המהדורה המנוקדת-בגרשיים גם בחיפוש מדויק.
        assert_eq!(
            exact_ids(&engine, "רמבם"),
            vec![1],
            "הטוקן-התאום נטול-הגרשיים"
        );
        // ביטוי רב-מילים עם טוקן-גרש: PhraseQuery על הטרמים החדשים.
        add(&mut engine, 3, "דברי תוס' ד\"ה אמר שם", "/books/c.txt");
        engine.commit().unwrap();
        assert_eq!(exact_ids(&engine, "תוס' ד\"ה"), vec![3]);
    }

    #[test]
    fn fuzzy_bridges_gershayim_and_clean_editions() {
        // הגישור המקורב: `"` = עריכת codepoint אחת, ו-Fix 2 מזריק את
        // הצורה הנקייה גם במרחק 0.
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "דברי רמב\"ם בהלכות", "/books/a.txt");
        add(&mut engine, 2, "דברי רמבם בהלכות", "/books/b.txt");
        engine.commit().unwrap();

        let fuzzy_ids = |engine: &SearchEngine, query: &str, d: u8| {
            ids(engine
                .search_fuzzy(
                    query.to_string(),
                    vec!["/root".to_string()],
                    100,
                    0,
                    d,
                    ResultsOrder::Relevance,
                    false,
                    false,
                )
                .unwrap())
        };

        // במרחק 1 — בשני הכיוונים.
        let got = fuzzy_ids(&engine, "רמב\"ם", 1);
        assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
        let got = fuzzy_ids(&engine, "רמבם", 1);
        assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
        // במרחק 0 — הודות לווריאנט הנקי (Fix 2).
        let got = fuzzy_ids(&engine, "רמב\"ם", 0);
        assert!(
            got.contains(&1) && got.contains(&2),
            "הווריאנט הנקי מגשר גם במרחק 0, got {got:?}"
        );
        // תקציב העריכה לא נבלע ע"י הגרשיים: רמכם→רמבם עריכה אחת — ומאז
        // שהאינדקס מטמיע טוקן-תאום נקי, הטרם רמבם קיים גם במסמך הגרשיים,
        // כך ששני המסמכים נתפסים כבר במרחק 1.
        let got = fuzzy_ids(&engine, "רמכם", 1);
        assert!(
            got.contains(&1) && got.contains(&2),
            "got {got:?}: רמכם→רמבם עריכה אחת, בשני המסמכים"
        );
        let got = fuzzy_ids(&engine, "רמכם", 2);
        assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
    }

    #[test]
    fn lexical_fuzzy_expands_quote_bearing_token() {
        // Fix 1 מקצה-לקצה: מפתח ה-lookup של טוקן-גרשיים פוגע ב-lexical.db
        // (אחרי מחיקת `"` ASCII), וההרחבה הלקסיקלית מזריקה קרובים שמרחק
        // העריכה לבדו לעולם לא היה תופס.
        let (mut engine, dir) = make_engine();
        assert!(engine.set_magic_dictionary_path(make_lexical_db(&dir)));
        add(&mut engine, 1, "דברי אדמורים רבים כאן", "/books/a.txt");
        add(&mut engine, 2, "דברי אדמו\"ר אחד כאן", "/books/b.txt");
        engine.commit().unwrap();

        let got = ids(engine
            .search_fuzzy(
                "אדמו\"ר".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                1,
                ResultsOrder::Relevance,
                false,
                false,
            )
            .unwrap());
        assert!(
            got.contains(&1),
            "אדמורים רחוק 3 עריכות — מושג רק דרך ההרחבה הלקסיקלית, got {got:?}"
        );
        assert!(got.contains(&2), "הטרם המדויק עצמו, got {got:?}");

        // הפער השיורי ההיסטורי (§5.2) נסגר: המילון פולט צורות נקיות בלבד,
        // אבל האינדקס מטמיע כעת טוקן-תאום נקי (אדמור) לצד אדמו"ר — כך
        // שהצורה הנקייה שהמילון מחזיר פוגעת בו ישירות.
        let got = ids(engine
            .search_fuzzy(
                "אדמורים".to_string(),
                vec!["/root".to_string()],
                100,
                0,
                1,
                ResultsOrder::Relevance,
                false,
                false,
            )
            .unwrap());
        assert!(got.contains(&1), "got {got:?}");
        assert!(
            got.contains(&2),
            "הטוקן-התאום סוגר את הפער השיורי, got {got:?}"
        );
    }

    #[test]
    fn advanced_typo_and_prefix_flags_work_on_gershayim_tokens() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "דברי רמבם בהלכות", "/books/a.txt");
        add(&mut engine, 2, "כתב הרמב\"ם על כך", "/books/b.txt");
        engine.commit().unwrap();

        let search = |engine: &mut SearchEngine, query: &str, opt: &str| {
            let mut word_opts = HashMap::new();
            word_opts.insert(opt.to_string(), true);
            let mut options = HashMap::new();
            options.insert(format!("{query}_0"), word_opts);
            ids(search_advanced_default(
                &engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
            )
            .unwrap())
        };

        // דגל typo: וריאנט-המחיקה של הגרפמה `"` מגשר למהדורות נקיות.
        let got = search(&mut engine, "רמב\"ם", hebrew_query::OPT_TYPO);
        assert!(got.contains(&1), "מחיקת `\"` מייצרת את רמבם, got {got:?}");
        // קידומות דקדוקיות סביב שורש עם `"` literal.
        let got = search(&mut engine, "רמב\"ם", "קידומות דקדוקיות");
        assert!(got.contains(&2), "ה־רמב\"ם עם קידומת, got {got:?}");
    }

    #[test]
    fn advanced_aramaic_option_matches_prefixes_and_final_swaps() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "מלכא קדישא", "/books/a.txt");
        add(&mut engine, 2, "אמרו דמלכא הוא", "/books/b.txt");
        add(&mut engine, 3, "כדמלכה בשעתו", "/books/c.txt");
        add(&mut engine, 4, "מדמלכא נפק", "/books/d.txt");
        add(&mut engine, 5, "אדמלכה קאי", "/books/e.txt");
        add(&mut engine, 6, "חכמין אמרין", "/books/f.txt");
        add(&mut engine, 7, "ספרא אחרינא", "/books/g.txt");
        engine.commit().unwrap();

        let search = |engine: &mut SearchEngine, query: &str, opts: &[&str]| {
            let mut options = HashMap::new();
            if !opts.is_empty() {
                let word_opts: HashMap<String, bool> =
                    opts.iter().map(|o| (o.to_string(), true)).collect();
                options.insert(format!("{query}_0"), word_opts);
            }
            ids(search_advanced_default(
                &engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
            )
            .unwrap())
        };

        // שתי האפשרויות יחד — ההתנהגות ההיסטורית: שקילות סופית ה↔א +
        // הקידומות ד/כד/מד/אד על שני הווריאנטים.
        let got = search(&mut engine, "מלכה", &["קידומות ארמיות", "סיומות ארמיות"]);
        for id in [1, 2, 3, 4, 5] {
            assert!(got.contains(&id), "ארמית החמיצה את מסמך {id}, got {got:?}");
        }
        assert!(!got.contains(&7), "ארמית רחבה מדי, got {got:?}");

        // סיומות בלבד: השקילות עובדת (מלכא, id 1) אבל אין קידומות (לא 2-5).
        let got = search(&mut engine, "מלכה", &["סיומות ארמיות"]);
        assert!(
            got.contains(&1),
            "סיומות ארמיות החמיצו את מלכא, got {got:?}"
        );
        for id in [2, 3, 4, 5] {
            assert!(
                !got.contains(&id),
                "סיומות בלבד לא אמורות לתת קידומות (מסמך {id}), got {got:?}"
            );
        }

        // קידומות בלבד: דמלכה עם קידומת נתפס דרך וריאנט? לא — אין שקילות
        // סופית, אז רק צורות של "מלכה" עם קידומת; המסמכים כאן נושאים מלכא
        // חוץ מ-3 ו-5 (כדמלכה, אדמלכה).
        let got = search(&mut engine, "מלכה", &["קידומות ארמיות"]);
        for id in [3, 5] {
            assert!(
                got.contains(&id),
                "קידומות ארמיות החמיצו את מסמך {id}, got {got:?}"
            );
        }
        for id in [1, 2, 4] {
            assert!(
                !got.contains(&id),
                "קידומות בלבד לא אמורות לתת שקילות סופית (מסמך {id}), got {got:?}"
            );
        }

        // ם↔ן: חכמים מוצא חכמין דרך סיומות ארמיות.
        let got = search(&mut engine, "חכמים", &["סיומות ארמיות"]);
        assert!(got.contains(&6), "ם↔ן לא עבד, got {got:?}");

        // בלי האפשרויות — אין שקילות ארמית.
        let got = search(&mut engine, "מלכה", &[]);
        assert!(got.is_empty(), "בלי ארמית לא אמור להימצא דבר, got {got:?}");
    }

    #[test]
    fn quote_free_indexing_and_ignore_quotes_option() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "כתב רמב\"ם על כך", "/books/a.txt");
        add(&mut engine, 2, "דברי רמבם בהלכות", "/books/b.txt");
        engine.commit().unwrap();

        let search = |engine: &mut SearchEngine, query: &str, opt: Option<&str>| {
            let mut options = HashMap::new();
            if let Some(opt) = opt {
                options.insert(
                    format!("{query}_0"),
                    HashMap::from([(opt.to_string(), true)]),
                );
            }
            ids(search_advanced_default(
                &engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
            )
            .unwrap())
        };

        // בלי שום אפשרות: הצורה הנקייה מוצאת גם את המקור עם הגרשיים —
        // הטוקן-התאום שהאינדקס מטמיע.
        let got = search(&mut engine, "רמבם", None);
        assert!(got.contains(&1) && got.contains(&2), "got {got:?}");

        // רמב"ם בלי האפשרות: התנהגות היסטורית — רק הצורה עם הגרשיים.
        let got = search(&mut engine, "רמב\"ם", None);
        assert!(got.contains(&1) && !got.contains(&2), "got {got:?}");

        // עם "התעלם מגרשיים": שתי הצורות.
        let got = search(&mut engine, "רמב\"ם", Some("התעלם מגרשיים"));
        assert!(got.contains(&1) && got.contains(&2), "got {got:?}");

        // ההדגשה מכסה את הצורה המקורית עם הגרשיים (התאום יורש offsets).
        let results = search_advanced_default(
            &engine,
            "רמבם".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        // הטקסט עובר escape של HTML — הגרשיים מופיעות כ-&quot;.
        let doc1 = results.iter().find(|r| r.id == 1).unwrap();
        assert!(
            doc1.text.contains("<font color=red>רמב&quot;ם</font>"),
            "highlight: {}",
            doc1.text
        );
    }

    #[test]
    fn advanced_translation_option_expands_from_dictionary() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "איתא בגמרא", "/books/a.txt");
        add(&mut engine, 2, "יש דברים בגו", "/books/b.txt");
        engine.commit().unwrap();

        let mut dict_file = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write as _;
        dict_file
            .write_all(r#"{ "מילון פשיטא": [ { "אִיתָא": "יש" } ] }"#.as_bytes())
            .unwrap();
        assert!(
            engine.set_translation_dictionary_path(dict_file.path().to_string_lossy().into_owned())
        );
        assert!(engine.has_translation_dictionary());

        let search = |engine: &mut SearchEngine, query: &str, opt: Option<&str>| {
            let mut options = HashMap::new();
            if let Some(opt) = opt {
                options.insert(
                    format!("{query}_0"),
                    HashMap::from([(opt.to_string(), true)]),
                );
            }
            ids(search_advanced_default(
                &engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
            )
            .unwrap())
        };

        // עברי→ארמי: "יש" עם תרגום ארמי מוצא גם את "איתא".
        let got = search(&mut engine, "יש", Some("תרגום ארמי"));
        assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
        // בלי האפשרות — אין הרחבה.
        let got = search(&mut engine, "יש", None);
        assert!(!got.contains(&1) && got.contains(&2), "got {got:?}");
        // ארמי→עברי.
        let got = search(&mut engine, "איתא", Some("תרגום ארמי"));
        assert!(got.contains(&1) && got.contains(&2), "got {got:?}");
    }

    #[test]
    fn advanced_acronym_option_expands_bidirectionally() {
        let (mut engine, _dir) = make_engine();
        // id 1 — הר"ת עצמו (מאונדקס גם כ-"רמבם" דרך הטוקן-התאום).
        add(&mut engine, 1, "אמר רמב\"ם בהלכות", "/books/a.txt");
        // id 2 — הפענוח המלא ככתוב.
        add(
            &mut engine,
            2,
            "כתב רבי משה בן מיימון בספרו",
            "/books/b.txt",
        );
        // id 3 — לא קשור.
        add(&mut engine, 3, "דבר אחר לגמרי", "/books/c.txt");
        engine.commit().unwrap();

        let mut dict_file = tempfile::NamedTempFile::new().unwrap();
        use std::io::Write as _;
        dict_file
            .write_all(r#"{ "רמב\"ם": ["רבי משה בן מיימון"] }"#.as_bytes())
            .unwrap();
        assert!(
            engine.set_acronyms_dictionary_path(dict_file.path().to_string_lossy().into_owned())
        );
        assert!(engine.has_acronyms_dictionary());

        // האפשרות דלוקה על המילה במיקום `word_index`.
        let search = |engine: &SearchEngine, query: &str, opt_on_word: Option<(&str, usize)>| {
            let mut options = HashMap::new();
            if let Some((word, i)) = opt_on_word {
                options.insert(
                    format!("{word}_{i}"),
                    HashMap::from([("ראשי תיבות".to_string(), true)]),
                );
            }
            ids(search_advanced_default(
                engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
            )
            .unwrap())
        };

        // כיוון א' (ר"ת→פענוח): "רמב\"ם" עם האפשרות מוצא גם את הפענוח המלא.
        let got = search(&engine, "רמב\"ם", Some(("רמב\"ם", 0)));
        assert!(got.contains(&1) && got.contains(&2), "forward: {got:?}");
        assert!(!got.contains(&3), "forward must not over-match: {got:?}");
        // בלי האפשרות — רק ההתאמה הישירה.
        let got = search(&engine, "רמב\"ם", None);
        assert!(got.contains(&1) && !got.contains(&2), "no-opt: {got:?}");

        // כיוון ב' (פענוח→ר"ת): הביטוי המלא עם האפשרות מוצא גם את הר"ת.
        let got = search(&engine, "רבי משה בן מיימון", Some(("רבי", 0)));
        assert!(got.contains(&1) && got.contains(&2), "reverse: {got:?}");
        // בלי האפשרות — רק ההתאמה הישירה לביטוי.
        let got = search(&engine, "רבי משה בן מיימון", None);
        assert!(
            !got.contains(&1) && got.contains(&2),
            "reverse no-opt: {got:?}"
        );

        // הדגשה: מסמך שנמצא דרך החלופה נצבע — בשני הכיוונים.
        let texts = |query: &str, word: &str| -> Vec<String> {
            let options = HashMap::from([(
                format!("{word}_0"),
                HashMap::from([("ראשי תיבות".to_string(), true)]),
            )]);
            search_advanced_default(
                &engine,
                query.to_string(),
                vec!["/root".to_string()],
                100,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::WordDistance,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.text)
            .collect()
        };
        // ר"ת→פענוח: מילות הפענוח נצבעות במסמך 2.
        let got = texts("רמב\"ם", "רמב\"ם");
        assert!(
            got.iter().any(|t| t.contains("<font color=red>רבי</font>")
                && t.contains("<font color=red>מיימון</font>")),
            "forward highlight missing: {got:?}"
        );
        // וגם הר\"ת עצמו נצבע במסמך 1.
        assert!(
            got.iter().any(|t| t.contains("<font color=red>רמב")),
            "forward self-highlight missing: {got:?}"
        );
        // פענוח→ר"ת: הר"ת נצבע במסמך 1.
        let got = texts("רבי משה בן מיימון", "רבי");
        assert!(
            got.iter().any(|t| t.contains("<font color=red>רמב")),
            "reverse highlight missing: {got:?}"
        );
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
                false,
                false,
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
            false,
            false,
        );
        assert!(result.is_err(), "distance > 2 should error, not panic");

        let count = engine.count_fuzzy(
            "שלום".to_string(),
            vec!["/root".to_string()],
            3,
            false,
            false,
        );
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
                false,
                false,
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
                    false,
                    false,
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
                .count_exact("שלום".to_string(), vec!["/root".to_string()], false, false)
                .unwrap(),
            2
        );

        let by_book = engine
            .count_by_book_exact("שלום".to_string(), vec!["/root".to_string()], false, false)
            .unwrap();
        assert_eq!(by_book.get("/books/a.txt").copied(), Some(2));

        let page = engine
            .search_and_count_exact(
                "שלום".to_string(),
                vec!["/root".to_string()],
                1,
                0,
                ResultsOrder::Relevance,
                false,
                false,
            )
            .unwrap();
        assert_eq!(page.total_count, 2);
        assert_eq!(page.results.len(), 1);
    }

    // ── חיפוש מנוקד (textVocalized) ─────────────────────────────────────

    /// ספר קטן: שורה מנוקדת, שורה נטולת ניקוד עם אותן מילים, שורה עם
    /// ניקוד+טעמים, ושורה מנוקדת עם קידומת.
    fn make_vocalized_engine() -> (SearchEngine, TempDir) {
        let (mut engine, dir) = make_engine();
        let text = "<h1>בראשית</h1>\n\
                    בְּרֵאשִׁית בָּרָא אֱלֹהִים\n\
                    ובראשית ברא אלהים בלי ניקוד\n\
                    וַיֹּ\u{05A3}אמֶר אֱלֹהִים יְהִי אוֹר\n\
                    וּבָרָא עוֹלָם";
        engine
            .add_text_book(
                "בראשית".to_string(),
                "/tanakh".to_string(),
                "/books/b.txt".to_string(),
                1,
                DEFAULT_GENERATION_ORDER,
                text.to_string(),
            )
            .unwrap();
        engine.commit().unwrap();
        (engine, dir)
    }

    #[test]
    fn vocalized_exact_requires_typed_marks_frees_untyped() {
        let (engine, _dir) = make_vocalized_engine();
        // קמץ שהוקלד חייב; הדגש שלא הוקלד חופשי — בָרָא מוצא את בָּרָא.
        let hits = engine
            .search_exact(
                "בָרָא".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        // התוצאה מציגה את העותק המנוקד השמור.
        assert!(hits[0].text.contains("בָּרָא"), "text: {}", hits[0].text);

        // תנועה שגויה במקום קמץ — נפסל.
        let miss = engine
            .search_exact(
                "בֵרָא".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
            )
            .unwrap();
        assert!(miss.is_empty());
    }

    #[test]
    fn vocalized_flag_off_keeps_plain_behaviour() {
        let (engine, _dir) = make_vocalized_engine();
        // בלי דגלים: הניקוד מנורמל החוצה והחיפוש מוצא את שתי השורות.
        let plain = engine
            .search_exact(
                "ברא".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
            )
            .unwrap();
        assert_eq!(plain.len(), 2);
        // עם דגל ניקוד ושאילתה לא מנוקדת: כל סימן חופשי, אך רק שורות
        // מנוקדות קיימות בשדה — השורה הנקייה לא נמצאת.
        let voc = engine
            .search_exact(
                "ברא".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
            )
            .unwrap();
        assert_eq!(voc.len(), 1);
    }

    #[test]
    fn vocalized_taamim_flag_splits_classes() {
        let (engine, _dir) = make_vocalized_engine();
        // שאילתה מנוקדת בלי טעמים מוצאת טקסט עם טעם (הטעם חופשי).
        let nikud_only = engine
            .search_exact(
                "וַיֹּאמֶר".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
            )
            .unwrap();
        assert_eq!(nikud_only.len(), 1);
        // שאילתה עם הטעם שהוקלד ושני הדגלים — עדיין נמצא (הטעם קיים בטקסט).
        let with_taam = engine
            .search_exact(
                "וַיֹּ\u{05A3}אמֶר".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                true,
                true,
            )
            .unwrap();
        assert_eq!(with_taam.len(), 1);
        // טעם שגוי (זקף-קטן במקום מונח) עם דגל טעמים — נפסל.
        let wrong_taam = engine
            .search_exact(
                "וַיֹּ\u{0594}אמֶר".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                true,
                true,
            )
            .unwrap();
        assert!(wrong_taam.is_empty());
    }

    #[test]
    fn vocalized_exact_phrase_matches_and_counts() {
        let (engine, _dir) = make_vocalized_engine();
        let page = engine
            .search_and_count_exact(
                "בָּרָא אֱלֹהִים".to_string(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
            )
            .unwrap();
        assert_eq!(page.total_count, 1);
        assert_eq!(page.results.len(), 1);

        let by_book = engine
            .count_by_book_exact("בָּרָא".to_string(), vec![], true, false)
            .unwrap();
        assert_eq!(by_book.get("/books/b.txt"), Some(&1));
    }

    #[test]
    fn vocalized_advanced_prefix_option_matches_prefixed_word() {
        let (engine, _dir) = make_vocalized_engine();
        let options = HashMap::from([(
            "בָרָא_0".to_string(),
            HashMap::from([("קידומות".to_string(), true)]),
        )]);
        let hits = search_advanced_default(
            &engine,
            "בָרָא".to_string(),
            vec![],
            10,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            true,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        // גם בָּרָא וגם וּבָרָא (הקידומת המנוקדת בתוך חלון ה-prefix).
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn per_word_nikud_option_routes_to_vocalized_field() {
        let (engine, _dir) = make_vocalized_engine();
        let search = |query: &str, options: HashMap<String, HashMap<String, bool>>| {
            search_advanced_default(
                &engine,
                query.to_string(),
                vec![],
                10,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                options,
                ResultsOrder::Catalogue,
                // הדגלים הגלובליים כבויים — הבקשה מגיעה מהאפשרות הפר-מילה.
                false,
                false,
                SearchScope::WordDistance,
            )
            .unwrap()
        };
        // בלי האפשרות: מסלול רגיל, הניקוד נבלע בנרמול — מוצא את שתי השורות
        // (המנוקדת והלא-מנוקדת).
        assert_eq!(search("בָרָא", HashMap::new()).len(), 2);
        // עם "ניקוד" על המילה: רץ על השדה המנוקד ודורש את הקמץ שהוקלד —
        // רק השורה המנוקדת.
        let options = HashMap::from([(
            "בָרָא_0".to_string(),
            HashMap::from([(hebrew_query::OPT_MATCH_NIKUD.to_string(), true)]),
        )]);
        let hits = search("בָרָא", options);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("בָּרָא"), "text: {}", hits[0].text);
        // תנועה שגויה עם האפשרות — נפסל.
        let options = HashMap::from([(
            "בֻרָא_0".to_string(),
            HashMap::from([(hebrew_query::OPT_MATCH_NIKUD.to_string(), true)]),
        )]);
        assert!(search("בֻרָא", options).is_empty());
    }

    #[test]
    fn per_word_nikud_option_leaves_other_words_free() {
        let (engine, _dir) = make_vocalized_engine();
        // "ניקוד" מסומן רק על המילה הראשונה; השנייה מוקלדת בניקוד "שגוי"
        // בכוונה — הסימנים שלה חופשיים ולכן ההתאמה שורדת.
        let options = HashMap::from([(
            "בָּרָא_0".to_string(),
            HashMap::from([(hebrew_query::OPT_MATCH_NIKUD.to_string(), true)]),
        )]);
        let hits = search_advanced_default(
            &engine,
            "בָּרָא אֱלֹהִֻים".to_string(),
            vec![],
            10,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            options,
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::WordDistance,
        )
        .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.contains("בְּרֵאשִׁית"), "text: {}", hits[0].text);
    }

    #[test]
    fn vocalized_fuzzy_finds_edit_distance_variants() {
        let (engine, _dir) = make_vocalized_engine();
        // ברח במרחק עריכה 1 מ-ברא; הווריאנט חופשי-סימנים מוצא את בָּרָא.
        let hits = engine
            .search_fuzzy(
                "בָּרַח".to_string(),
                vec![],
                10,
                0,
                1,
                ResultsOrder::Catalogue,
                true,
                false,
            )
            .unwrap();
        assert!(!hits.is_empty());
        // מרחק 0: רק הצורה המדויקת (עם הסימנים שהוקלדו) — ברח לא קיים.
        let exact_only = engine
            .search_fuzzy(
                "בָּרַח".to_string(),
                vec![],
                10,
                0,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
            )
            .unwrap();
        assert!(exact_only.is_empty());
    }

    // ── SearchScope: "באותה פסקה" / "תחת אותה כותרת" ─────────────────────

    /// ספר עם שתי כותרות משנה: תחת "סימן א" המילים "מתכוין" ו"מתעסק"
    /// מפוזרות על שורות שונות; תחת "סימן ב" שורה אחת מכילה את שתיהן —
    /// בסדר הפוך לסדר השאילתה.
    fn scope_engine() -> (SearchEngine, TempDir, u64) {
        let (mut engine, dir) = make_engine();
        let text = "<h1>ספר הבדיקה</h1>\n\
                    <h2>סימן א</h2>\n\
                    דין אינו מתכוין בשבת\n\
                    ודין מתעסק בחלבים ועריות\n\
                    <h2>סימן ב</h2>\n\
                    כאן נדון רק במלאכת שבת\n\
                    מתעסק וגם אינו מתכוין באותה שורה";
        engine
            .add_text_book(
                "ספר הבדיקה".to_string(),
                "/root".to_string(),
                "/books/scope.txt".to_string(),
                7,
                0,
                text.to_string(),
            )
            .unwrap();
        engine.commit().unwrap();
        // ids: ((catalogue_order+1) << 32) + ordinal + 1
        let id_base = (7u64 + 1) << 32;
        (engine, dir, id_base)
    }

    fn scope_search(engine: &SearchEngine, query: &str, scope: SearchScope) -> Vec<u64> {
        search_advanced_default(
            &engine,
            query.to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            scope,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
    }

    #[test]
    fn same_paragraph_scope_matches_unordered_within_line() {
        let (engine, _dir, id_base) = scope_engine();
        // בסדר השאילתה אין התאמה בשום שורה — מסלול המרחק (סדר + צמידות)
        // לא מוצא דבר.
        assert_eq!(
            scope_search(&engine, "מתכוין מתעסק", SearchScope::WordDistance),
            Vec::<u64>::new()
        );
        // באותה פסקה: רק השורה שמכילה את שתי המילים, למרות הסדר ההפוך.
        assert_eq!(
            scope_search(&engine, "מתכוין מתעסק", SearchScope::SameParagraph),
            vec![id_base + 7]
        );
    }

    #[test]
    fn same_section_scope_matches_across_lines_under_one_heading() {
        let (engine, _dir, id_base) = scope_engine();
        // "מתכוין" ו"מתעסק" בשורות שונות תחת "סימן א", ובאותה שורה תחת
        // "סימן ב" — חוזרות כל השורות שנושאות מילה מהשאילתה בתוך סעיף
        // שמכיל את כל המילים.
        assert_eq!(
            scope_search(&engine, "מתכוין מתעסק", SearchScope::SameSection),
            vec![id_base + 3, id_base + 4, id_base + 7]
        );
        // "בשבת" (סימן א) ו"נדון" (סימן ב) — אף סעיף לא מכיל את שתיהן.
        assert_eq!(
            scope_search(&engine, "בשבת נדון", SearchScope::SameSection),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn same_section_scope_count_matches_search() {
        let (engine, _dir, _id_base) = scope_engine();
        let count = count_advanced_default(
            &engine,
            "מתכוין מתעסק".to_string(),
            vec!["/root".to_string()],
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            false,
            false,
            SearchScope::SameSection,
        )
        .unwrap();
        assert_eq!(count, 3);

        let by_book = count_by_book_advanced_default(
            &engine,
            "מתכוין מתעסק".to_string(),
            vec!["/root".to_string()],
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            false,
            false,
            SearchScope::SameSection,
        )
        .unwrap();
        assert_eq!(by_book.get("/books/scope.txt"), Some(&3));
    }

    #[test]
    fn same_paragraph_scope_snippet_highlights_every_word() {
        let (engine, _dir, _id_base) = scope_engine();
        let results = search_advanced_default(
            &engine,
            "מתכוין מתעסק".to_string(),
            vec!["/root".to_string()],
            100,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            ResultsOrder::Catalogue,
            false,
            false,
            SearchScope::SameParagraph,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        let text = &results[0].text;
        // שתי המילים מודגשות, בלי מסנן סדר/מרחק.
        assert!(
            text.contains("<font color=red>מתעסק</font>"),
            "snippet: {text}"
        );
        assert!(
            text.contains("<font color=red>מתכוין</font>"),
            "snippet: {text}"
        );
    }
}
