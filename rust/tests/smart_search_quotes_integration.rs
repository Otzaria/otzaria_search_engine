#![cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]

use search_engine::api::search_engine::*;
use tempfile::TempDir;

const ROWS: &[(&str, &str, u64)] = &[
    ("בית שמע ישראל", "/qa/a", 101),
    ("בית שמע אישראל", "/qa/a", 102),
    ("בית ישראל שמע", "/qa/a", 103),
    ("בית שמע לכל ישראל", "/qa/a", 104),
    ("שמע ישראל ללא בית", "/qa/a", 105),
    ("שמע ישראל עולם", "/qa/a", 106),
    ("בית שמע ישראל", "/qa/a", 101),
    ("בית שמע ישראל", "/qa/b", 108),
];
fn setup(sidecar: bool) -> (SearchEngine, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("index");
    std::fs::create_dir(&index).unwrap();
    let mut e = SearchEngine::new(index.to_str().unwrap());
    for (n, (text, facet, section)) in ROWS.iter().enumerate() {
        e.add_document(
            n as u64 + 1,
            "QA",
            "ref",
            facet,
            text,
            n as u64,
            false,
            facet,
            Some(*section),
            None,
            None,
        )
        .unwrap();
    }
    e.commit().unwrap();
    if sidecar {
        use otzaria_semantic_search::semantic::embedding::mock::write_stub_onnx_package;
        let model = write_stub_onnx_package(&dir.path().join("model"));
        e.configure_semantic(SemanticConfigInput {
            root_dir: dir.path().join("semantic").to_string_lossy().into_owned(),
            model_path: model.to_string_lossy().into_owned(),
            model_id: "test-mock".into(),
            embedding_dim: 64,
            pooling: "in-graph".into(),
            max_tokens: 512,
            model_quantization: "int8".into(),
            embedding_text_version: 1,
            onnx_runtime_path: None,
        })
        .unwrap();
        let books = ["/qa/a", "/qa/b"]
            .iter()
            .map(|facet| SemanticBookInput {
                source_book_key: (*facet).into(),
                title: "QA".into(),
                content_fingerprint: 1,
                is_pdf: false,
                topics: (*facet).into(),
                extra_facets: vec![],
                lines: ROWS
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, f, _))| f == facet)
                    .map(|(n, (text, _, section))| SemanticBookLineInput {
                        line_id: n as u64 + 1,
                        section_id: *section,
                        text: (*text).into(),
                        line_hash: n as u64 + 1,
                        reference: "ref".into(),
                        segment: n as u64,
                    })
                    .collect(),
            })
            .collect();
        e.semantic_index_books(books).unwrap();
    }
    (e, dir)
}
fn page(
    e: &SearchEngine,
    q: &str,
    mode: SemanticRetrievalMode,
    group: Option<SemanticGroupingMode>,
    limit: u32,
    offset: u32,
) -> SemanticSearchResponse {
    e.search_semantic(
        q.into(),
        vec!["/qa/a".into()],
        limit,
        offset,
        SemanticLexicalMode::Fuzzy,
        1,
        mode,
        group,
        false,
        false,
        None,
        &SemanticCancellationToken::new(),
    )
    .unwrap()
}
fn ids(r: &SemanticSearchResponse) -> Vec<u64> {
    let mut out: Vec<_> = r.results.iter().map(|h| h.id).collect();
    out.sort_unstable();
    out
}
#[test]
fn partial_quotes_filter_order_gap_typo_and_allow_outside_edits_on_both_routes() {
    for configured in [false, true] {
        let (e, _dir) = setup(configured);
        for mode in [
            SemanticRetrievalMode::LexicalOnly,
            SemanticRetrievalMode::Hybrid,
        ] {
            for query in [
                "ביתת \"שמע ישראל\"",
                "\"שמע ישראל\" ביתת",
                "ביתת “שמע ישראל”",
            ] {
                let r = page(&e, query, mode, None, 100, 0);
                assert_eq!(ids(&r), [1, 5, 7], "sidecar={configured}, q={query}");
                assert_eq!(r.lexical_total_count, 3);
                assert_eq!(r.total_count, 3);
                assert!(!r.has_more);
                assert_eq!(r.counts_are_exact, !configured);
            }
        }
        let count = page(
            &e,
            "ביתת \"שמע ישראל\"",
            SemanticRetrievalMode::SemanticOnly,
            None,
            100,
            0,
        );
        assert_eq!(
            count.lexical_total_count, 3,
            "count-only sidecar={configured}"
        );
    }
}
#[test]
fn fully_quoted_and_separate_quotes_have_different_adjacency() {
    for configured in [false, true] {
        let (e, _dir) = setup(configured);
        let full = page(
            &e,
            "\"שמע ישראל\"",
            SemanticRetrievalMode::Hybrid,
            None,
            100,
            0,
        );
        assert_eq!(ids(&full), [1, 5, 6, 7]);
        let separate = page(
            &e,
            "ביתת \"שמע\" \"ישראל\"",
            SemanticRetrievalMode::Hybrid,
            None,
            100,
            0,
        );
        assert_eq!(ids(&separate), [1, 3, 4, 5, 7]);
        assert_eq!(separate.lexical_total_count, 5);
    }
}
#[test]
fn grouping_and_page_counts_do_not_reintroduce_quote_violations() {
    for configured in [false, true] {
        let (e, _dir) = setup(configured);
        let all = page(
            &e,
            "ביתת \"שמע ישראל\"",
            SemanticRetrievalMode::Hybrid,
            Some(SemanticGroupingMode::SameSection),
            100,
            0,
        );
        assert_eq!(all.total_count, 3);
        assert_eq!(all.group_count, Some(2));
        assert_eq!(all.results.len(), 2);
        assert!(!all.has_more);
        assert!(all.results.iter().any(|h| h.merged_count == 2));
        let first = page(
            &e,
            "ביתת \"שמע ישראל\"",
            SemanticRetrievalMode::Hybrid,
            Some(SemanticGroupingMode::SameSection),
            1,
            0,
        );
        let second = page(
            &e,
            "ביתת \"שמע ישראל\"",
            SemanticRetrievalMode::Hybrid,
            Some(SemanticGroupingMode::SameSection),
            1,
            1,
        );
        assert!(first.has_more);
        assert!(!second.has_more);
        assert_ne!(first.results[0].id, second.results[0].id);
    }
}
#[test]
fn vocalized_quotes_forbid_other_marks_order_gap_and_spelling() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("index");
    std::fs::create_dir(&p).unwrap();
    let mut e = SearchEngine::new(p.to_str().unwrap());
    for (n, s) in [
        "בית שָׁמַע ישראל",
        "בית שְׁמַע ישראל",
        "בית שָׁמַע לכל ישראל",
        "בית ישראל שָׁמַע",
        "בית שָׁמַע אישראל",
    ]
    .iter()
    .enumerate()
    {
        e.add_document(
            n as u64 + 1,
            "QA",
            "ref",
            "/qa",
            s,
            0,
            false,
            "/qa",
            None,
            None,
            None,
        )
        .unwrap();
    }
    e.commit().unwrap();
    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::LexicalOnly,
        SemanticRetrievalMode::SemanticOnly,
    ] {
        let r = e
            .search_semantic(
                "ביתת \"שָׁמַע ישראל\"".into(),
                vec![],
                100,
                0,
                SemanticLexicalMode::Fuzzy,
                1,
                mode,
                None,
                true,
                false,
                None,
                &SemanticCancellationToken::new(),
            )
            .unwrap();
        assert_eq!(r.lexical_total_count, 1);
        if mode != SemanticRetrievalMode::SemanticOnly {
            assert_eq!(ids(&r), [1]);
        }
    }
}
#[test]
fn partial_quote_highlight_expands_only_outside_and_paints_only_literal_spans() {
    for configured in [false, true] {
        let (mut e, _dir) = setup(configured);
        e.add_document(
            9,
            "QA",
            "ref",
            "/qa/a",
            "בית שמע ישראל אישראל שמע",
            9,
            false,
            "/qa/a",
            None,
            None,
            None,
        )
        .unwrap();
        e.commit().unwrap();
        let r = page(
            &e,
            "ביתת \"שמע ישראל\"",
            SemanticRetrievalMode::Hybrid,
            None,
            100,
            0,
        );
        let hit = r
            .results
            .iter()
            .find(|h| h.id == 9)
            .expect("outside typo must not remove literal hit");
        assert!(hit.is_highlighted);
        assert!(
            hit.snippet_html.contains("<font color=red>בית</font>"),
            "{}",
            hit.snippet_html
        );
        assert_eq!(
            hit.snippet_html
                .matches("<font color=red>שמע</font>")
                .count(),
            1,
            "{}",
            hit.snippet_html
        );
        assert_eq!(
            hit.snippet_html
                .matches("<font color=red>ישראל</font>")
                .count(),
            1,
            "{}",
            hit.snippet_html
        );
        assert!(
            !hit.snippet_html.contains("<font color=red>אישראל</font>"),
            "{}",
            hit.snippet_html
        );
    }
}
#[test]
fn teamim_inside_quote_are_literal_when_requested() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("index");
    std::fs::create_dir(&p).unwrap();
    let mut e = SearchEngine::new(p.to_str().unwrap());
    for (n, s) in ["בית ש֑מע ישראל", "בית ש֔מע ישראל", "בית ש֑מע לכל ישראל"]
        .iter()
        .enumerate()
    {
        e.add_document(
            n as u64 + 1,
            "QA",
            "ref",
            "/qa",
            s,
            0,
            false,
            "/qa",
            None,
            None,
            None,
        )
        .unwrap();
    }
    e.commit().unwrap();
    let r = e
        .search_semantic(
            "ביתת \"ש֑מע ישראל\"".into(),
            vec![],
            100,
            0,
            SemanticLexicalMode::Fuzzy,
            1,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            true,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(ids(&r), [1]);
    assert_eq!(r.lexical_total_count, 1);
}

#[test]
fn independent_quote_highlighting_keeps_each_quote_literal_and_handles_repeated_outside_words() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("index");
    std::fs::create_dir(&p).unwrap();
    let mut e = SearchEngine::new(p.to_str().unwrap());
    e.add_document(
        1,
        "QA",
        "ref",
        "/qa",
        "בית שמע ישראל ביתת ישראל שמע אישראל",
        0,
        false,
        "/qa",
        None,
        None,
        None,
    )
    .unwrap();
    e.commit().unwrap();
    let r = e
        .search_semantic(
            "ביתת \"שמע ישראל\"".into(),
            vec![],
            100,
            0,
            SemanticLexicalMode::Fuzzy,
            1,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(ids(&r), [1]);
    let html = &r.results[0].snippet_html;
    assert!(html.contains("<font color=red>בית</font>"), "{html}");
    assert!(html.contains("<font color=red>ביתת</font>"), "{html}");
    assert_eq!(
        html.matches("<font color=red>שמע</font>").count(),
        1,
        "{html}"
    );
    assert_eq!(
        html.matches("<font color=red>ישראל</font>").count(),
        1,
        "{html}"
    );
    assert!(!html.contains("<font color=red>אישראל</font>"), "{html}");
    let repeated = e
        .search_semantic(
            "שמע \"שמע ישראל\"".into(),
            vec![],
            100,
            0,
            SemanticLexicalMode::Fuzzy,
            1,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(ids(&repeated), [1]);
    assert_eq!(
        repeated.results[0]
            .snippet_html
            .matches("<font color=red>שמע</font>")
            .count(),
        2
    );
}

#[test]
fn vocalized_partial_quote_highlights_literal_marks_and_outside_typo() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("index");
    std::fs::create_dir(&p).unwrap();
    let mut e = SearchEngine::new(p.to_str().unwrap());
    e.add_document(
        1,
        "QA",
        "ref",
        "/qa",
        "בית שָׁמַע ישראל שְׁמַע אישראל",
        0,
        false,
        "/qa",
        None,
        None,
        None,
    )
    .unwrap();
    e.commit().unwrap();
    let r = e
        .search_semantic(
            "ביתת \"שָׁמַע ישראל\"".into(),
            vec![],
            100,
            0,
            SemanticLexicalMode::Fuzzy,
            1,
            SemanticRetrievalMode::Hybrid,
            None,
            true,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(ids(&r), [1]);
    let html = &r.results[0].snippet_html;
    assert!(html.contains("<font color=red>בית</font>"), "{html}");
    assert!(html.contains("<font color=red>שָׁמַע</font>"), "{html}");
    assert!(!html.contains("<font color=red>שְׁמַע</font>"), "{html}");
    assert!(!html.contains("<font color=red>אישראל</font>"), "{html}");
}

#[test]
fn zero_limit_counts_keep_quote_constraints_with_facets_on_all_routes() {
    for configured in [false, true] {
        let (e, _dir) = setup(configured);
        for mode in [
            SemanticRetrievalMode::Hybrid,
            SemanticRetrievalMode::LexicalOnly,
            SemanticRetrievalMode::SemanticOnly,
        ] {
            let count = page(&e, "ביתת \"שמע ישראל\"", mode, None, 0, 0);
            assert!(count.results.is_empty());
            assert_eq!(
                count.lexical_total_count, 3,
                "configured={configured}, mode={mode:?}"
            );
        }
    }
}
