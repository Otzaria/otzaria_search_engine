//! Integration coverage for the live `SearchEngine` → Tantivy → sidecar route,
//! without GGUF/FFI. The sidecar's explicitly test-only deterministic backend is
//! selected by the `semantic-mock` feature; production builds use
//! `semantic`/`semantic-real`.
//!
//! Every retrieval mode is exercised *through a configured sidecar*, not only
//! the lexical fallback: `Hybrid` and `LexicalOnly` are the only paths that run
//! the BM25 candidate collector, so a fallback-only suite would leave it
//! untested.

#![cfg(feature = "semantic-mock")]

use otzaria_semantic_search::semantic::embedding::mock::write_stub_gguf;
use search_engine::api::search_engine::{
    ResultsOrder, SearchEngine, SemanticBookInput, SemanticBookLineInput, SemanticConfigInput,
    SemanticExecutedMode, SemanticGroupingMode, SemanticIndexingSummary, SemanticLexicalMode,
    SemanticResultSource, SemanticRetrievalMode, SemanticSearchResponse,
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

/// Open the sidecar against `root`, writing the stub model it needs.
fn configure(engine: &mut SearchEngine, root: &TempDir) {
    let model_path = root.path().join("mock.gguf");
    write_stub_gguf(&model_path, 3).unwrap();
    let status = engine
        .configure_semantic(SemanticConfigInput {
            root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
            model_path: model_path.to_string_lossy().into_owned(),
            model_id: "test-mock".to_owned(),
            embedding_dim: 64,
        })
        .unwrap();
    assert!(
        status.enabled,
        "the sidecar should report itself configured"
    );
}

fn index_books(engine: &SearchEngine, lines: &[Line]) -> SemanticIndexingSummary {
    engine
        .semantic_index_books(vec![SemanticBookInput {
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
        .unwrap()
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
    let lines = vec![
        line(9_001, "בראשית א:א", "בראשית ברא אלהים", 2),
        line(9_002, "בראשית א:ב", "בראשית ברא אלהים", 3),
    ];
    let (mut engine, _root) = fixture(&lines);

    // Keep the lower-id group representative live while making its grouped
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

    let response = exact(&engine, "בראשית ברא", SemanticRetrievalMode::Hybrid);
    assert_eq!(response.executed_mode, SemanticExecutedMode::LexicalOnly);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].source, SemanticResultSource::Lexical);
    assert!(response.fallback_reason.is_some());
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

    let model_path = root.path().join("mock.gguf");
    let attempt = engine.configure_semantic(SemanticConfigInput {
        root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
        model_path: model_path.to_string_lossy().into_owned(),
        model_id: "a-different-model".to_owned(),
        embedding_dim: 64,
    });
    let message = match attempt {
        Ok(_) => panic!("changing the model while a session is open must fail"),
        Err(error) => error.to_string(),
    };
    assert!(
        message.contains("model_id"),
        "the refusal must name the input that changed: {message}"
    );

    // The refusal left the session untouched.
    let after = engine.semantic_status();
    assert_eq!(after.vector_count, before.vector_count);
    assert_eq!(after.indexed_book_count, before.indexed_book_count);

    // Disabling first is the explicit route to a different model.
    engine.disable_semantic();
    engine
        .configure_semantic(SemanticConfigInput {
            root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
            model_path: model_path.to_string_lossy().into_owned(),
            model_id: "a-different-model".to_owned(),
            embedding_dim: 64,
        })
        .expect("an explicit disable clears the way for a new configuration");
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
