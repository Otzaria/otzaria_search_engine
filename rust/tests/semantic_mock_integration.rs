//! Integration coverage for the live `SearchEngine` → Tantivy → sidecar route,
//! without a real model or the FFI. The sidecar's explicitly test-only
//! deterministic backend is selected by the `semantic-mock` feature; production
//! builds use `semantic`.
//!
//! Every retrieval mode is exercised *through a configured sidecar*, not only
//! the lexical fallback: `Hybrid` and `LexicalOnly` are the only paths that run
//! the BM25 candidate collector, so a fallback-only suite would leave it
//! untested.
//!
//! Gated like `tests/build_semantic_artifact.rs`, and for its reasons: the
//! sidecar is opened on the stub ONNX package, which only the stand-in serves,
//! and `semantic-onnx` would take it ahead of the stand-in and fail to load it.

#![cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]

use otzaria_semantic_search::semantic::embedding::mock::write_stub_onnx_package;
use search_engine::api::search_engine::{
    ResultsOrder, SearchEngine, SemanticBookInput, SemanticBookLineInput,
    SemanticCancellationToken, SemanticConfigInput, SemanticError, SemanticErrorKind,
    SemanticExecutedMode, SemanticFusionStrategy, SemanticGroupingMode, SemanticIndexingSummary,
    SemanticLexicalMode, SemanticQueryTypeAlphas, SemanticRankingOptions, SemanticResultSource,
    SemanticRetrievalMode, SemanticSearchResponse, SemanticState,
};
use tempfile::TempDir;

const BOOK_KEY: &str = "/library/bereshit.json";
const TOPICS: &str = "/תורה";
const SECTION: u64 = 42;
/// Matches the engine's default `HighlightConfig::max_chars`, which bounds the
/// display string on both the sidecar and the fallback path.
const SNIPPET_BUDGET: usize = 800;

/// One line, shared by the lexical index and the sidecar so `line_id` lines up —
/// the invariant fusion and hydration both depend on.
struct Line {
    id: u64,
    reference: String,
    text: String,
    segment: u64,
}

fn line(id: u64, reference: &str, text: &str, segment: u64) -> Line {
    Line {
        id,
        reference: reference.to_owned(),
        text: text.to_owned(),
        segment,
    }
}

/// A Tantivy index holding `lines`, with no sidecar configured yet.
fn lexical_engine(lines: &[Line]) -> (SearchEngine, TempDir) {
    let root = TempDir::new().unwrap();
    let index_dir = root.path().join("tantivy");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut engine = SearchEngine::new(index_dir.to_str().unwrap());

    for line in lines {
        engine
            .add_document(
                line.id,
                "בראשית",
                &line.reference,
                TOPICS,
                &line.text,
                line.segment,
                false,
                BOOK_KEY,
                Some(SECTION),
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    (engine, root)
}

/// The folder [`write_stub_model`] writes the stub ONNX package into.
fn model_dir(root: &TempDir) -> std::path::PathBuf {
    root.path().join("model")
}

/// The stub ONNX package under `root`: a graph that passes the sidecar's structural
/// checks, and its `tokenizer.json`. Its path is the one [`mock_config`] names.
fn write_stub_model(root: &TempDir) {
    write_stub_onnx_package(&model_dir(root));
}

/// What every test opens the sidecar with: the stub ONNX graph under `root`, and
/// the pooling the stand-in claims for that format. Text recipe 1, so that a
/// query that is a line's exact text embeds exactly as the line does.
fn mock_config(root: &TempDir, model_id: &str) -> SemanticConfigInput {
    SemanticConfigInput {
        root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
        model_path: model_dir(root)
            .join("model.onnx")
            .to_string_lossy()
            .into_owned(),
        model_id: model_id.to_owned(),
        embedding_dim: 64,
        pooling: "in-graph".to_owned(),
        max_tokens: 512,
        model_quantization: "int8".to_owned(),
        embedding_text_version: 1,
        onnx_runtime_path: None,
    }
}

/// A field's name, and one edit of that field alone.
type FieldEdit = (&'static str, fn(&mut SemanticConfigInput));

/// Open the sidecar against `root`, writing the stub model it needs.
fn configure(engine: &mut SearchEngine, root: &TempDir) {
    write_stub_model(root);
    let status = engine
        .configure_semantic(mock_config(root, "test-mock"))
        .unwrap();
    assert!(
        status.enabled,
        "the sidecar should report itself configured"
    );
}

fn index_books(engine: &SearchEngine, lines: &[Line]) -> SemanticIndexingSummary {
    try_index_books(engine, lines).unwrap()
}

/// The refusal a call was expected to end in. The results it would otherwise have
/// returned are not `Debug`, so `expect_err` is not available.
fn refusal<T>(result: Result<T, SemanticError>, expected: &str) -> SemanticError {
    match result {
        Ok(_) => panic!("{expected}, and it succeeded"),
        Err(error) => error,
    }
}

fn try_index_books(
    engine: &SearchEngine,
    lines: &[Line],
) -> Result<SemanticIndexingSummary, SemanticError> {
    engine.semantic_index_books(vec![SemanticBookInput {
        source_book_key: BOOK_KEY.to_owned(),
        title: "בראשית".to_owned(),
        content_fingerprint: 123,
        is_pdf: false,
        topics: TOPICS.to_owned(),
        extra_facets: Vec::new(),
        lines: lines
            .iter()
            .map(|line| SemanticBookLineInput {
                line_id: line.id,
                section_id: SECTION,
                text: line.text.clone(),
                line_hash: 100 + line.id,
                reference: line.reference.clone(),
                segment: line.segment,
            })
            .collect(),
    }])
}

/// Lexical index + open sidecar + indexed vectors: the fully wired route.
fn fixture(lines: &[Line]) -> (SearchEngine, TempDir) {
    let (mut engine, root) = lexical_engine(lines);
    configure(&mut engine, &root);
    let indexed = index_books(&engine, lines);
    assert!(indexed.enabled);
    assert_eq!(indexed.books_indexed, 1);
    (engine, root)
}

#[allow(clippy::too_many_arguments)]
fn search(
    engine: &SearchEngine,
    query: &str,
    limit: u32,
    offset: u32,
    lexical_mode: SemanticLexicalMode,
    fuzzy_distance: u8,
    retrieval_mode: SemanticRetrievalMode,
    grouping: Option<SemanticGroupingMode>,
) -> SemanticSearchResponse {
    engine
        .search_semantic(
            query.to_owned(),
            Vec::new(),
            limit,
            offset,
            lexical_mode,
            fuzzy_distance,
            retrieval_mode,
            grouping,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
}

fn exact(
    engine: &SearchEngine,
    query: &str,
    retrieval_mode: SemanticRetrievalMode,
) -> SemanticSearchResponse {
    search(
        engine,
        query,
        10,
        0,
        SemanticLexicalMode::Exact,
        0,
        retrieval_mode,
        None,
    )
}

fn one_line_corpus() -> Vec<Line> {
    vec![line(9_001, "בראשית א:א", "בראשית ברא אלהים", 2)]
}

// ── Retrieval modes through a configured sidecar ─────────────────────────────

#[test]
fn hybrid_fuses_real_bm25_candidates_with_semantic_ones() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);

    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::Hybrid);

    assert_eq!(response.executed_mode, SemanticExecutedMode::Hybrid);
    assert!(response.semantic_available);
    assert_eq!(response.fallback_kind, None);
    assert_eq!(engine.semantic_status().state, SemanticState::Ready);
    assert_eq!(response.results.len(), 1);
    let hit = &response.results[0];
    assert_eq!(hit.id, 9_001);
    // Proves the BM25 collector ran and its score survived fusion: only the
    // lexical half can populate this, and only `Hybrid`/`LexicalOnly` run it.
    assert!(
        hit.lexical_score.is_some(),
        "hybrid must carry the Tantivy BM25 score"
    );
    assert!(matches!(
        hit.source,
        SemanticResultSource::Lexical | SemanticResultSource::Both
    ));
    // The corpus count is Tantivy's, not a candidate-window artefact.
    assert_eq!(response.lexical_total_count, 1);
    assert!(!response.counts_are_exact);
}

#[test]
fn lexical_only_through_the_sidecar_is_a_choice_not_a_degradation() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);

    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::LexicalOnly);

    assert_eq!(response.executed_mode, SemanticExecutedMode::LexicalOnly);
    // The semantic path was never consulted, so it is unavailable *and* there is
    // nothing to explain — unlike the fallback path, which always states a
    // reason.
    assert!(!response.semantic_available);
    assert!(response.fallback_reason.is_none());
    assert_eq!(response.fallback_kind, None);
    assert_eq!(response.results.len(), 1);
    assert!(response.results[0].lexical_score.is_some());
    assert_eq!(response.results[0].source, SemanticResultSource::Lexical);
}

#[test]
fn fuzzy_lexical_mode_collects_candidates_within_edit_distance() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);

    // One deletion away from "בראשית".
    let response = search(
        &engine,
        "בראשי",
        10,
        0,
        SemanticLexicalMode::Fuzzy,
        1,
        SemanticRetrievalMode::LexicalOnly,
        None,
    );

    assert_eq!(response.executed_mode, SemanticExecutedMode::LexicalOnly);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].id, 9_001);
    assert!(response.results[0].lexical_score.is_some());
    assert_eq!(response.lexical_total_count, 1);
}

#[test]
fn semantic_only_hydrates_a_real_tantivy_document() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);

    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::SemanticOnly);

    assert!(response.semantic_available);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].id, 9_001);
    assert_eq!(response.results[0].snippet_html, "בראשית ברא אלהים");
    assert!(!response.results[0].needs_hydration);
    assert!(!response.counts_are_exact);
}

#[test]
fn an_oversized_page_reports_the_candidate_window_cap() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);

    let capped = search(
        &engine,
        "בראשית ברא",
        u32::MAX,
        0,
        SemanticLexicalMode::Exact,
        0,
        SemanticRetrievalMode::SemanticOnly,
        None,
    );

    assert!(capped.candidate_window_truncated);
    assert!(!capped.truncated);
    assert!(capped
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("candidate window capped")));
    // A note about a search that did run, not a fallback: it has no kind.
    assert!(capped.semantic_available);
    assert_eq!(capped.fallback_kind, None);
}

// ── The display contract ─────────────────────────────────────────────────────

/// A line comfortably past the snippet budget (Hebrew is two bytes per char),
/// with the query words at the front so a snippet can be anchored there.
fn long_line() -> Vec<Line> {
    let text = format!("בראשית ברא אלהים {}", "את השמים ואת הארץ ".repeat(60));
    assert!(
        text.len() > SNIPPET_BUDGET,
        "the fixture must exceed the snippet budget to be meaningful"
    );
    vec![line(9_100, "בראשית א:א", text.trim_end(), 1)]
}

#[test]
fn the_sidecar_path_returns_bounded_highlighted_markup_like_the_lexical_api() {
    let lines = long_line();
    let raw_len = lines[0].text.len();
    let (engine, _root) = fixture(&lines);

    let sidecar = exact(&engine, "בראשית ברא", SemanticRetrievalMode::Hybrid);
    assert_eq!(sidecar.executed_mode, SemanticExecutedMode::Hybrid);
    let hit = &sidecar.results[0];
    assert!(hit.is_highlighted, "a lexical match must be painted");
    assert!(hit.snippet_html.contains("<font color=red>"));
    assert!(
        hit.snippet_html.len() < raw_len,
        "the display string must be a snippet, not the whole line"
    );

    // The same query with the sidecar out of the picture: the fallback must
    // produce the same *kind* of value, so the app's snippet parser cannot tell
    // which path served the page.
    let (fallback_engine, _fallback_root) = lexical_engine(&lines);
    let fallback = exact(
        &fallback_engine,
        "בראשית ברא",
        SemanticRetrievalMode::Hybrid,
    );
    assert_eq!(fallback.executed_mode, SemanticExecutedMode::LexicalOnly);
    let fallback_hit = &fallback.results[0];
    assert!(fallback_hit.is_highlighted);
    assert!(fallback_hit.snippet_html.contains("<font color=red>"));
    assert!(fallback_hit.snippet_html.len() < raw_len);
}

#[test]
fn an_unpainted_line_is_bounded_and_flagged_rather_than_returned_whole() {
    let lines = long_line();
    let (engine, _root) = fixture(&lines);

    // `SemanticOnly` runs no lexical query, so there is nothing to paint with.
    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::SemanticOnly);
    let hit = &response.results[0];

    assert!(
        !hit.is_highlighted,
        "no lexical query ran, so nothing may claim to be highlighted"
    );
    assert!(!hit.snippet_html.contains("<font color=red>"));
    assert!(
        hit.snippet_html.ends_with('…'),
        "a cut line must say so: {}",
        hit.snippet_html
    );
    // Budget plus the ellipsis; never the unbounded line.
    assert!(
        hit.snippet_html.len() <= SNIPPET_BUDGET + '…'.len_utf8(),
        "snippet was {} bytes",
        hit.snippet_html.len()
    );
    // Cutting on a char boundary keeps it valid UTF-8 Hebrew.
    assert!(hit.snippet_html.starts_with("בראשית ברא אלהים"));
}

#[test]
fn a_semantic_hit_that_fails_the_phrase_is_not_painted_as_a_lexical_match() {
    // The query words are present but reversed and non-adjacent, so the exact
    // phrase query does not match this line: it can only reach the page through
    // vector similarity, with no BM25 score.
    let lines = vec![line(9_200, "שמות ד:כז", "אהרן הכהן משה רבנו", 1)];
    let (engine, _root) = fixture(&lines);

    let response = exact(&engine, "משה אהרן", SemanticRetrievalMode::Hybrid);

    assert_eq!(response.executed_mode, SemanticExecutedMode::Hybrid);
    assert_eq!(response.results.len(), 1);
    let hit = &response.results[0];
    assert_eq!(hit.source, SemanticResultSource::Semantic);
    assert!(
        hit.lexical_score.is_none(),
        "the phrase query must not have matched this line"
    );
    // Both words are in the line and the term highlighter would gladly paint
    // them, but no complete in-order occurrence exists and nothing lexical
    // vouched for this result — so claiming a highlight would assert a phrase
    // match that is not there.
    assert!(
        !hit.is_highlighted,
        "a phrase-failing semantic hit must not be painted: {}",
        hit.snippet_html
    );
    assert!(!hit.snippet_html.contains("<font color=red>"));
    assert_eq!(hit.snippet_html, "אהרן הכהן משה רבנו");
}

#[test]
fn a_lexical_phrase_match_is_still_painted() {
    // The same two words, now adjacent and in query order: Tantivy matches the
    // phrase, so painting is licensed and must still happen.
    let lines = vec![line(9_201, "שמות ד:כז", "וילך משה אהרן המדברה", 1)];
    let (engine, _root) = fixture(&lines);

    let response = exact(&engine, "משה אהרן", SemanticRetrievalMode::Hybrid);

    assert_eq!(response.results.len(), 1);
    let hit = &response.results[0];
    assert!(hit.lexical_score.is_some());
    assert!(hit.is_highlighted);
    assert!(hit.snippet_html.contains("<font color=red>"));
}

// ── Stale sidecar records ────────────────────────────────────────────────────

#[test]
fn stale_primaries_and_grouped_siblings_are_dropped_and_reported_apart() {
    // The second line is the query's words and more, so it scores below the first
    // and is the group's sibling by score: which line of a tie represents a group is
    // the semantic path's order, and not what this test is about.
    let lines = vec![
        line(9_001, "בראשית א:א", "בראשית ברא אלהים", 2),
        line(9_002, "בראשית א:ב", "בראשית ברא אלהים את השמים", 3),
    ];
    let (mut engine, _root) = fixture(&lines);

    // Keep the better-scored group representative live while making its grouped
    // sibling stale in the semantic sidecar.
    engine.delete_document_by_id(9_002).unwrap();
    engine.commit().unwrap();

    let first_page = search(
        &engine,
        "בראשית ברא",
        1,
        0,
        SemanticLexicalMode::Exact,
        0,
        SemanticRetrievalMode::SemanticOnly,
        None,
    );
    let second_page = search(
        &engine,
        "בראשית ברא",
        1,
        1,
        SemanticLexicalMode::Exact,
        0,
        SemanticRetrievalMode::SemanticOnly,
        None,
    );
    assert_eq!(first_page.results.len(), 1);
    assert!(second_page.results.is_empty());
    assert_eq!(first_page.total_count, second_page.total_count);
    assert!(!first_page.counts_are_exact);
    assert!(!second_page.counts_are_exact);
    // A stale *primary* is removed from the window, so it is reported as a
    // result — distinct from the sibling wording below.
    assert!(first_page
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("stale semantic result")));
    assert_eq!(first_page.fallback_kind, None);

    let grouped = search(
        &engine,
        "בראשית ברא",
        10,
        0,
        SemanticLexicalMode::Exact,
        0,
        SemanticRetrievalMode::SemanticOnly,
        Some(SemanticGroupingMode::SameSection),
    );
    assert_eq!(grouped.results.len(), 1);
    assert_eq!(grouped.results[0].id, 9_001);
    assert_eq!(grouped.results[0].merged_count, 1);
    assert!(grouped.results[0].merged.is_empty());
    // The sibling was dropped from a card on this page, not from the window, and
    // says so in its own words.
    let reason = grouped.fallback_reason.as_deref().unwrap();
    assert!(
        reason.contains("stale grouped sibling"),
        "expected a sibling-specific reason, got: {reason}"
    );

    // Once the representative also disappears, no stale semantic record may
    // resurrect either deleted Tantivy document.
    engine.delete_document_by_id(9_001).unwrap();
    engine.commit().unwrap();
    let deleted = exact(&engine, "בראשית ברא", SemanticRetrievalMode::SemanticOnly);
    assert!(deleted.results.is_empty());
    assert!(deleted
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("stale semantic result")));
}

#[test]
fn group_count_survives_the_materialized_sibling_cap() {
    const GROUP_SIZE: u64 = 25;

    let lines: Vec<Line> = (0..GROUP_SIZE)
        .map(|index| {
            line(
                10_000 + index,
                &format!("בראשית א:{}", index + 1),
                "בראשית ברא אלהים את השמים ואת הארץ",
                index + 1,
            )
        })
        .collect();
    let (engine, _root) = fixture(&lines);

    let response = search(
        &engine,
        "בראשית ברא",
        10,
        0,
        SemanticLexicalMode::Exact,
        0,
        SemanticRetrievalMode::SemanticOnly,
        Some(SemanticGroupingMode::SameSection),
    );

    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].merged_count, GROUP_SIZE as u32);
    assert_eq!(response.results[0].merged.len(), 10);
}

// ── Index lifecycle ──────────────────────────────────────────────────────────

#[test]
fn index_diff_surfaces_an_unverifiable_fingerprint_instead_of_calling_it_current() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);

    // `add_document` writes no content fingerprint, so the lexical hash is zero
    // — deliberately reported as unverifiable rather than up to date.
    let diff = engine.semantic_index_diff().unwrap();

    assert!(diff.enabled);
    assert_eq!(diff.unverifiable_books, vec![BOOK_KEY.to_owned()]);
    assert!(diff.new_books.is_empty());
    assert!(diff.removed_books.is_empty());
    assert!(!diff.model_mismatch);
    assert!(!diff.chunking_mismatch);
    assert!(!diff.normalization_mismatch);
}

#[test]
fn removing_a_book_drops_its_vectors_without_touching_tantivy() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);
    assert!(engine.semantic_status().vector_count > 0);

    let removed = engine
        .remove_semantic_books(vec![BOOK_KEY.to_owned()])
        .unwrap();

    assert!(removed.enabled);
    assert!(removed.vectors_removed > 0);
    assert_eq!(engine.semantic_status().vector_count, 0);
    // The lexical document is untouched, so a lexical retrieval still finds it.
    let lexical = exact(&engine, "בראשית ברא", SemanticRetrievalMode::LexicalOnly);
    assert_eq!(lexical.results.len(), 1);
    // ...while the semantic path now has nothing to serve.
    let semantic = exact(&engine, "בראשית ברא", SemanticRetrievalMode::SemanticOnly);
    assert!(semantic.results.is_empty());
}

#[test]
fn resetting_clears_every_book_and_leaves_the_lexical_index_intact() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);

    let reset = engine.reset_semantic_index().unwrap();

    assert!(reset.enabled);
    assert!(reset.vectors_removed > 0);
    let status = engine.semantic_status();
    assert_eq!(status.indexed_book_count, 0);
    assert_eq!(status.vector_count, 0);
    let lexical = exact(&engine, "בראשית ברא", SemanticRetrievalMode::LexicalOnly);
    assert_eq!(lexical.results.len(), 1);
}

#[test]
fn disabling_falls_back_to_ranked_lexical_results_with_a_reason() {
    let lines = one_line_corpus();
    let (mut engine, _root) = fixture(&lines);

    engine.disable_semantic();

    let status = engine.semantic_status();
    assert!(!status.enabled);
    assert!(!status.available);
    assert!(status.last_error.is_some());
    assert_eq!(status.state, SemanticState::NotConfigured);
    assert_eq!(status.error_kind, Some(SemanticErrorKind::NotConfigured));

    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::Hybrid);
    assert_eq!(response.executed_mode, SemanticExecutedMode::LexicalOnly);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].source, SemanticResultSource::Lexical);
    assert!(response.fallback_reason.is_some());
    assert_eq!(
        response.fallback_kind,
        Some(SemanticErrorKind::NotConfigured)
    );
}

// ── Cancellation ─────────────────────────────────────────────────────────────

/// Through the public API, with a session open: a search whose token is already cancelled
/// throws `Cancelled` in every mode, never its lexical results in its place, and the session
/// serves the next search, whose fresh token changes nothing. Where each look stops a search
/// is the unit tests' (`semantic_cancellation`), which can cancel at a chosen one.
#[test]
fn a_cancelled_search_is_cancelled_in_every_mode_and_the_next_one_is_served() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);
    let cancelled = SemanticCancellationToken::new();
    cancelled.cancel();

    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::SemanticOnly,
        SemanticRetrievalMode::LexicalOnly,
    ] {
        let error = refusal(
            engine.search_semantic(
                "בראשית ברא".to_owned(),
                Vec::new(),
                10,
                0,
                SemanticLexicalMode::Exact,
                0,
                mode,
                None,
                false,
                false,
                None,
                &cancelled,
            ),
            "a search whose token is cancelled must not be served",
        );
        assert_eq!(error.kind, SemanticErrorKind::Cancelled, "{mode:?}");
        assert_eq!(error.field, None, "{mode:?}");

        let served = exact(&engine, "בראשית ברא", mode);
        assert_eq!(served.results.len(), 1, "{mode:?}");
        assert_eq!(served.fallback_kind, None, "{mode:?}");
    }
    assert!(engine.semantic_status().available);
}

// ── Ranking options ──────────────────────────────────────────────────────────

/// An exact search for `query` in `mode`, ranked by `ranking`.
fn ranked(
    engine: &SearchEngine,
    query: &str,
    mode: SemanticRetrievalMode,
    ranking: Option<SemanticRankingOptions>,
) -> Result<SemanticSearchResponse, SemanticError> {
    engine.search_semantic(
        query.to_owned(),
        Vec::new(),
        10,
        0,
        SemanticLexicalMode::Exact,
        0,
        mode,
        None,
        false,
        false,
        ranking,
        &SemanticCancellationToken::new(),
    )
}

/// A page as bits: the ranking of two searches is the same only if this is.
fn page_bits(response: &SemanticSearchResponse) -> Vec<(u64, u32, Option<u32>, Option<u32>)> {
    response
        .results
        .iter()
        .map(|hit| {
            (
                hit.id,
                hit.fused_score.to_bits(),
                hit.lexical_score.map(f32::to_bits),
                hit.semantic_score.map(f32::to_bits),
            )
        })
        .collect()
}

/// Passing the defaults is passing nothing: every page, in every mode, is the same to the
/// bit. Two sessions over the same lines, so that neither answers from the other's cache.
#[test]
fn the_default_ranking_options_rank_exactly_as_none() {
    let lines = vec![
        line(9_001, "בראשית א:א", "בראשית ברא אלהים את השמים ואת הארץ", 2),
        line(
            9_002,
            "בראשית א:ב",
            "והארץ היתה תהו ובהו וחשך על פני תהום",
            2,
        ),
        line(9_003, "בראשית א:ג", "ויאמר אלהים יהי אור ויהי אור", 2),
    ];
    let (without, _without_root) = fixture(&lines);
    let (with_defaults, _defaults_root) = fixture(&lines);

    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::SemanticOnly,
        SemanticRetrievalMode::LexicalOnly,
    ] {
        for query in ["בראשית ברא", "ויאמר אלהים יהי אור", "\"יהי אור\"", "אור"]
        {
            let none = ranked(&without, query, mode, None).unwrap();
            let defaults = ranked(
                &with_defaults,
                query,
                mode,
                Some(SemanticRankingOptions::defaults()),
            )
            .unwrap();
            assert_eq!(page_bits(&none), page_bits(&defaults), "{mode:?} {query}");
            assert_eq!(
                none.executed_mode, defaults.executed_mode,
                "{mode:?} {query}"
            );
        }
    }
}

/// The options reach the ranking: with reciprocal rank fusion at `rrf_k` 30, the one line,
/// first on both sides, scores 1 / 31 from each, which no weighted fusion gives it.
#[test]
fn a_ranking_option_is_the_ranking_the_search_runs_on() {
    let lines = one_line_corpus();
    let (engine, _root) = fixture(&lines);
    let rrf = SemanticRankingOptions {
        fusion_strategy: SemanticFusionStrategy::Rrf,
        rrf_k: 30,
        ..SemanticRankingOptions::defaults()
    };

    let fused = ranked(
        &engine,
        "בראשית ברא",
        SemanticRetrievalMode::Hybrid,
        Some(rrf),
    )
    .unwrap();
    let hit = fused.results.first().expect("the line both sides found");
    assert_eq!(hit.source, SemanticResultSource::Both);
    let from_each_side = 1.0f32 / 31.0;
    assert_eq!(hit.fused_score, from_each_side + from_each_side);

    let weighted = ranked(&engine, "בראשית ברא", SemanticRetrievalMode::Hybrid, None).unwrap();
    assert_ne!(weighted.results[0].fused_score, hit.fused_score);
}

/// An option out of range is refused before the search runs, naming it, in every mode and
/// with no session open as with one: a lexical fallback would hide the mistake.
#[test]
fn an_option_out_of_range_is_refused_with_or_without_a_session() {
    let lines = one_line_corpus();
    let (mut engine, _root) = fixture(&lines);
    let spoiled = SemanticRankingOptions {
        alpha_by_query_type: SemanticQueryTypeAlphas {
            short: -0.2,
            ..SemanticRankingOptions::defaults().alpha_by_query_type
        },
        ..SemanticRankingOptions::defaults()
    };

    for open in [true, false] {
        if !open {
            engine.disable_semantic();
        }
        for mode in [
            SemanticRetrievalMode::Hybrid,
            SemanticRetrievalMode::SemanticOnly,
            SemanticRetrievalMode::LexicalOnly,
        ] {
            let error = refusal(
                ranked(&engine, "בראשית ברא", mode, Some(spoiled.clone())),
                "an alpha below 0 must be refused",
            );
            assert_eq!(error.kind, SemanticErrorKind::InvalidInput, "{mode:?}");
            assert_eq!(
                error.field.as_deref(),
                Some("alpha_by_query_type.short"),
                "{mode:?}"
            );
            assert!(
                error.message.contains("-0.2"),
                "{mode:?}: {}",
                error.message
            );
        }
    }
}

// ── Reconfiguration ──────────────────────────────────────────────────────────

#[test]
fn reconfiguring_with_the_same_inputs_keeps_the_indexed_vectors() {
    let lines = one_line_corpus();
    let (mut engine, root) = fixture(&lines);
    let before = engine.semantic_status();
    assert!(before.vector_count > 0);

    // A defensive caller may configure again on every app start. Re-opening the
    // engine would drop the manifest's vector-backed books, because the store is
    // in-memory — so this must be a no-op, not a silent wipe.
    configure(&mut engine, &root);

    let after = engine.semantic_status();
    assert_eq!(after.vector_count, before.vector_count);
    assert_eq!(after.indexed_book_count, before.indexed_book_count);
    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::SemanticOnly);
    assert_eq!(response.results.len(), 1);
}

#[test]
fn reconfiguring_with_different_inputs_is_refused_rather_than_destructive() {
    let lines = one_line_corpus();
    let (mut engine, root) = fixture(&lines);
    let before = engine.semantic_status();

    let attempt = engine.configure_semantic(mock_config(&root, "a-different-model"));
    let error = match attempt {
        Ok(_) => panic!("changing the model while a session is open must fail"),
        Err(error) => error,
    };
    assert!(
        error.message.contains("model_id"),
        "the refusal must name the input that changed: {}",
        error.message
    );
    assert_eq!(error.kind, SemanticErrorKind::SessionConflict);

    // A different recipe under the same model file asks for different vectors
    // just the same, and is refused the same way.
    let recipe_changes: [FieldEdit; 4] = [
        ("pooling", |config| config.pooling = "mean".to_owned()),
        ("max_tokens", |config| config.max_tokens = 256),
        ("model_quantization", |config| {
            config.model_quantization = "fp32".to_owned()
        }),
        ("embedding_text_version", |config| {
            config.embedding_text_version = 2
        }),
    ];
    for (field, change) in recipe_changes {
        let mut config = mock_config(&root, "test-mock");
        change(&mut config);
        let error = match engine.configure_semantic(config) {
            Ok(_) => panic!("changing {field} while a session is open must fail"),
            Err(error) => error,
        };
        assert!(
            error.message.contains(field),
            "the refusal must name the input that changed: {}",
            error.message
        );
        assert_eq!(error.kind, SemanticErrorKind::SessionConflict, "{field}");
    }

    // The refusals left the session untouched.
    let after = engine.semantic_status();
    assert_eq!(after.vector_count, before.vector_count);
    assert_eq!(after.indexed_book_count, before.indexed_book_count);

    // Disabling first is the explicit route to a different model.
    engine.disable_semantic();
    engine
        .configure_semantic(mock_config(&root, "a-different-model"))
        .expect("an explicit disable clears the way for a new configuration");
}

/// Where ONNX Runtime lives is not the index's identity: the stand-in loads nothing, so a
/// path that names no file serves, and the manifest never records it. It is an input of the
/// session all the same, since the process keeps the first runtime it loads: the same path
/// again is a repeat, and none, or another, is refused by name. An empty one is refused
/// before anything is opened.
#[test]
fn a_runtime_path_is_an_input_of_the_session_and_not_its_identity() {
    let lines = one_line_corpus();
    let (mut engine, root) = lexical_engine(&lines);
    write_stub_model(&root);
    let bundled = root
        .path()
        .join("Frameworks")
        .join("libonnxruntime.dylib")
        .to_string_lossy()
        .into_owned();
    let with_runtime = |path: Option<&str>| SemanticConfigInput {
        onnx_runtime_path: path.map(str::to_owned),
        ..mock_config(&root, "test-mock")
    };

    engine
        .configure_semantic(with_runtime(Some(&bundled)))
        .expect("configuring loads nothing");
    let indexed = index_books(&engine, &lines);
    assert_eq!(indexed.books_indexed, 1);
    assert!(engine.semantic_status().available);
    let manifest =
        std::fs::read_to_string(root.path().join("semantic").join("semantic_manifest.json"))
            .unwrap();
    assert!(
        !manifest.contains("Frameworks") && !manifest.contains("onnxruntime"),
        "the manifest records no deployment: {manifest}"
    );

    let before = engine.semantic_status();
    engine
        .configure_semantic(with_runtime(Some(&bundled)))
        .expect("the same runtime path is a repeat");
    assert_eq!(engine.semantic_status().vector_count, before.vector_count);

    let elsewhere = root
        .path()
        .join("libonnxruntime.dylib")
        .to_string_lossy()
        .into_owned();
    for changed in [None, Some(elsewhere.as_str())] {
        let error = refusal(
            engine.configure_semantic(with_runtime(changed)),
            "another runtime path while a session is open must be refused",
        );
        assert_eq!(
            error.kind,
            SemanticErrorKind::SessionConflict,
            "{changed:?}"
        );
        assert!(
            error.message.contains("and onnx_runtime_path changed"),
            "{}",
            error.message
        );
    }
    assert_eq!(engine.semantic_status().vector_count, before.vector_count);

    engine.disable_semantic();
    let error = refusal(
        engine.configure_semantic(with_runtime(Some(""))),
        "an empty runtime path must be refused",
    );
    assert_eq!(error.kind, SemanticErrorKind::InvalidInput);
    assert_eq!(error.field.as_deref(), Some("onnx_runtime_path"));
    assert_eq!(
        engine.semantic_status().state,
        SemanticState::NotConfigured,
        "a refused configuration leaves no session"
    );
}

// ── Concurrency ──────────────────────────────────────────────────────────────

/// Guards the *signatures*: `semantic_index_books` and `semantic_status` both
/// take `&self`, so a lexical search can run while a semantic index is being
/// built. Declaring either `&mut self` would fail to compile here — and would
/// make flutter_rust_bridge take a write lock on the whole engine, freezing
/// every concurrent lexical search and any status poll for the length of the
/// indexing run.
#[test]
fn lexical_search_and_status_stay_available_while_indexing_runs() {
    let lines: Vec<Line> = (0..80)
        .map(|index| {
            line(
                20_000 + index,
                &format!("בראשית א:{}", index + 1),
                "בראשית ברא אלהים את השמים ואת הארץ",
                index + 1,
            )
        })
        .collect();
    let (mut engine, root) = lexical_engine(&lines);
    configure(&mut engine, &root);

    let engine = &engine;
    std::thread::scope(|scope| {
        let indexer = scope.spawn(|| index_books(engine, &lines));

        for _ in 0..40 {
            let page = engine
                .search_and_count_exact(
                    "בראשית ברא".to_owned(),
                    Vec::new(),
                    10,
                    0,
                    ResultsOrder::Relevance,
                    false,
                    false,
                    None,
                )
                .unwrap();
            assert_eq!(page.total_count, lines.len() as u32);
            // Reading the status must not require exclusive access either.
            let _ = engine.semantic_status();
        }

        let summary = indexer.join().unwrap();
        assert!(summary.enabled);
        assert_eq!(summary.books_indexed, 1);
    });

    assert!(engine.semantic_status().vector_count > 0);
}

// ── Typed states and failures ────────────────────────────────────────────────

/// Configuring loads no model, so a session is open with nothing to serve until indexing
/// loads one: `Empty`, and each search's semantic half fails on its own, as `QueryFailed`,
/// with the lexical half served.
#[test]
fn a_session_with_nothing_indexed_is_empty_and_its_searches_fall_back() {
    let lines = one_line_corpus();
    let (mut engine, root) = lexical_engine(&lines);
    configure(&mut engine, &root);

    let status = engine.semantic_status();
    assert!(status.enabled && !status.available);
    assert_eq!(status.state, SemanticState::Empty);
    assert_eq!((status.last_error, status.error_kind), (None, None));

    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::Hybrid);
    assert_eq!(response.executed_mode, SemanticExecutedMode::LexicalOnly);
    assert!(
        response.fallback_reason.is_some(),
        "the coordinator says why the semantic half did not run"
    );
    assert_eq!(response.fallback_kind, Some(SemanticErrorKind::QueryFailed));
    assert_eq!(response.results.len(), 1);

    index_books(&engine, &lines);
    assert_eq!(engine.semantic_status().state, SemanticState::Ready);
}

/// The model loads at the first indexing, so that is where a missing or unusable one is
/// refused, each by its own kind; the status then reports the session as failed, with the
/// sidecar's text, which carries no type of its own.
#[test]
fn indexing_names_a_missing_or_unusable_model_by_kind() {
    let lines = one_line_corpus();
    type Plant = fn(&TempDir) -> SemanticConfigInput;
    let models: [(SemanticErrorKind, Plant); 3] = [
        (SemanticErrorKind::ModelMissing, |root| {
            mock_config(root, "test-mock")
        }),
        // A package whose graph is not a graph. Its tokenizer is there, since that is
        // checked first.
        (SemanticErrorKind::ModelInvalid, |root| {
            write_stub_model(root);
            std::fs::write(model_dir(root).join("model.onnx"), b"not a model").unwrap();
            mock_config(root, "test-mock")
        }),
        // An ONNX graph without the tokenizer its package needs beside it. The stand-in
        // serves the graph, so nothing but the package is at fault.
        (SemanticErrorKind::TokenizerMissing, |root| {
            write_stub_model(root);
            std::fs::remove_file(model_dir(root).join("tokenizer.json")).unwrap();
            mock_config(root, "test-mock")
        }),
    ];
    for (kind, plant) in models {
        let (mut engine, root) = lexical_engine(&lines);
        engine
            .configure_semantic(plant(&root))
            .expect("configuring loads no model");

        let error = refusal(try_index_books(&engine, &lines), "the model cannot load");
        assert_eq!(error.kind, kind, "{}", error.message);
        assert!(
            error.message.starts_with("semantic indexing failed: "),
            "{}",
            error.message
        );

        let status = engine.semantic_status();
        assert_eq!(status.state, SemanticState::Failed, "{kind:?}");
        assert!(status.last_error.is_some(), "{kind:?}");
        assert_eq!(
            status.error_kind,
            Some(SemanticErrorKind::Internal),
            "{kind:?}"
        );
    }
}

/// Configuring over a root built under another model: the sidecar keeps the old manifest
/// and refuses its vectors until a reset, which both the status and indexing say by type.
#[test]
fn a_root_built_under_another_configuration_needs_a_reindex() {
    let lines = one_line_corpus();
    let (mut engine, root) = fixture(&lines);
    engine.disable_semantic();
    engine
        .configure_semantic(mock_config(&root, "a-different-model"))
        .expect("an explicit disable clears the way for a new configuration");

    let status = engine.semantic_status();
    assert_eq!(status.state, SemanticState::NeedsReindex);
    assert!(status.needs_full_reindex.is_some());

    let error = refusal(
        try_index_books(&engine, &lines),
        "the old vectors are refused",
    );
    assert_eq!(
        error.kind,
        SemanticErrorKind::ReindexRequired,
        "{}",
        error.message
    );

    engine.reset_semantic_index().unwrap();
    index_books(&engine, &lines);
    assert_eq!(engine.semantic_status().state, SemanticState::Ready);
}

/// A configuration the sidecar cannot serve is the caller's input to fix, refused before
/// anything is opened; the field is named where the refusal says which.
#[test]
fn a_configuration_the_sidecar_cannot_serve_is_invalid_input() {
    let (mut engine, root) = lexical_engine(&one_line_corpus());
    type Spoil = fn(&mut SemanticConfigInput);
    let spoiled: [(Option<&str>, &str, Spoil); 4] = [
        (Some("model_quantization"), "model_quantization", |c| {
            c.model_quantization = " ".to_owned()
        }),
        (
            Some("embedding_text_version"),
            "embedding_text_version",
            |c| c.embedding_text_version = 99,
        ),
        (None, "last_token", |c| c.pooling = "last_token".to_owned()),
        (None, "embedding_max_tokens is 1", |c| c.max_tokens = 1),
    ];
    for (field, named, spoil) in spoiled {
        let mut config = mock_config(&root, "test-mock");
        spoil(&mut config);
        let error = refusal(
            engine.configure_semantic(config),
            "the configuration is refused",
        );
        assert!(error.message.contains(named), "{named}: {}", error.message);
        assert_eq!(error.kind, SemanticErrorKind::InvalidInput, "{named}");
        assert_eq!(error.field.as_deref(), field, "{named}");
        assert!(!engine.semantic_status().enabled, "{named}");
    }
}
