#![cfg(feature = "semantic-mock")]

use otzaria_semantic_search::semantic::embedding::mock::write_stub_gguf;
use search_engine::api::search_engine::{
    SearchEngine, SemanticBookInput, SemanticBookLineInput, SemanticGroupingMode,
    SemanticLexicalMode, SemanticRetrievalMode,
};
use tempfile::TempDir;

/// Exercises the live SearchEngine → Tantivy → sidecar route without GGUF/FFI.
/// The sidecar's explicitly test-only deterministic backend is selected by the
/// `semantic-mock` feature; production builds use `semantic`/`semantic-real`.
#[test]
fn semantic_mock_search_hydrates_a_real_tantivy_document() {
    let root = TempDir::new().unwrap();
    let index_dir = root.path().join("tantivy");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut engine = SearchEngine::new(index_dir.to_str().unwrap());

    engine
        .add_document(
            9_001,
            "בראשית",
            "בראשית א:א",
            "/תורה",
            "בראשית ברא אלהים",
            2,
            false,
            "/library/bereshit.json",
            Some(42),
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();

    let model_path = root.path().join("mock.gguf");
    write_stub_gguf(&model_path, 3).unwrap();
    let status = engine
        .configure_semantic(search_engine::api::search_engine::SemanticConfigInput {
            root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
            model_path: model_path.to_string_lossy().into_owned(),
            model_id: "test-mock".to_owned(),
            embedding_dim: 64,
        })
        .unwrap();
    assert!(status.enabled);

    let indexed = engine
        .semantic_index_books(vec![SemanticBookInput {
            source_book_key: "/library/bereshit.json".to_owned(),
            title: "בראשית".to_owned(),
            content_fingerprint: 123,
            is_pdf: false,
            topics: "/תורה".to_owned(),
            extra_facets: Vec::new(),
            lines: vec![SemanticBookLineInput {
                line_id: 9_001,
                section_id: 42,
                text: "בראשית ברא אלהים".to_owned(),
                line_hash: 77,
                reference: "בראשית א:א".to_owned(),
                segment: 2,
            }],
        }])
        .unwrap();
    assert!(indexed.enabled);
    assert_eq!(indexed.books_indexed, 1);

    let response = engine
        .search_semantic(
            "בראשית ברא".to_owned(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
        )
        .unwrap();

    assert!(response.semantic_available);
    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].id, 9_001);
    assert_eq!(response.results[0].text, "בראשית ברא אלהים");
    assert!(!response.results[0].needs_hydration);
    assert!(!response.counts_are_exact);

    let capped = engine
        .search_semantic(
            "בראשית ברא".to_owned(),
            Vec::new(),
            u32::MAX,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
        )
        .unwrap();
    assert!(capped.candidate_window_truncated);
    assert!(!capped.truncated);
    assert!(capped
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("candidate window capped")));
}

#[test]
fn semantic_mock_search_drops_deleted_primary_and_grouped_sibling_records() {
    let root = TempDir::new().unwrap();
    let index_dir = root.path().join("tantivy");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut engine = SearchEngine::new(index_dir.to_str().unwrap());

    for id in [9_001, 9_002] {
        engine
            .add_document(
                id,
                "בראשית",
                &format!("בראשית א:{}", id - 9_000),
                "/תורה",
                "בראשית ברא אלהים",
                id - 8_999,
                false,
                "/library/bereshit.json",
                Some(42),
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();

    let model_path = root.path().join("mock.gguf");
    write_stub_gguf(&model_path, 3).unwrap();
    engine
        .configure_semantic(search_engine::api::search_engine::SemanticConfigInput {
            root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
            model_path: model_path.to_string_lossy().into_owned(),
            model_id: "test-mock".to_owned(),
            embedding_dim: 64,
        })
        .unwrap();
    engine
        .semantic_index_books(vec![SemanticBookInput {
            source_book_key: "/library/bereshit.json".to_owned(),
            title: "בראשית".to_owned(),
            content_fingerprint: 123,
            is_pdf: false,
            topics: "/תורה".to_owned(),
            extra_facets: Vec::new(),
            lines: vec![
                SemanticBookLineInput {
                    line_id: 9_001,
                    section_id: 42,
                    text: "בראשית ברא אלהים".to_owned(),
                    line_hash: 77,
                    reference: "בראשית א:א".to_owned(),
                    segment: 2,
                },
                SemanticBookLineInput {
                    line_id: 9_002,
                    section_id: 42,
                    text: "בראשית ברא אלהים".to_owned(),
                    line_hash: 78,
                    reference: "בראשית א:ב".to_owned(),
                    segment: 3,
                },
            ],
        }])
        .unwrap();

    // Keep the lower-id group representative live while making its grouped
    // sibling stale in the semantic sidecar.
    engine.delete_document_by_id(9_002).unwrap();
    engine.commit().unwrap();

    let first_page = engine
        .search_semantic(
            "בראשית ברא".to_owned(),
            Vec::new(),
            1,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
        )
        .unwrap();
    let second_page = engine
        .search_semantic(
            "בראשית ברא".to_owned(),
            Vec::new(),
            1,
            1,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
        )
        .unwrap();
    assert_eq!(first_page.results.len(), 1);
    assert!(second_page.results.is_empty());
    assert_eq!(first_page.total_count, second_page.total_count);
    assert!(!first_page.counts_are_exact);
    assert!(!second_page.counts_are_exact);

    let grouped = engine
        .search_semantic(
            "בראשית ברא".to_owned(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            Some(SemanticGroupingMode::SameSection),
            false,
            false,
        )
        .unwrap();
    assert_eq!(grouped.results.len(), 1);
    assert_eq!(grouped.results[0].id, 9_001);
    assert_eq!(grouped.results[0].merged_count, 1);
    assert!(grouped.results[0].merged.is_empty());
    assert!(grouped
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("stale semantic result")));

    // Once the representative also disappears, no stale semantic record may
    // resurrect either deleted Tantivy document.
    engine.delete_document_by_id(9_001).unwrap();
    engine.commit().unwrap();
    let deleted = engine
        .search_semantic(
            "בראשית ברא".to_owned(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            None,
            false,
            false,
        )
        .unwrap();
    assert!(deleted.results.is_empty());
    assert!(deleted
        .fallback_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("stale semantic result")));
}

#[test]
fn semantic_group_count_survives_the_materialized_sibling_cap() {
    const GROUP_SIZE: u64 = 25;

    let root = TempDir::new().unwrap();
    let index_dir = root.path().join("tantivy");
    std::fs::create_dir_all(&index_dir).unwrap();
    let mut engine = SearchEngine::new(index_dir.to_str().unwrap());
    let mut lines = Vec::new();

    for index in 0..GROUP_SIZE {
        let id = 10_000 + index;
        engine
            .add_document(
                id,
                "בראשית",
                &format!("בראשית א:{}", index + 1),
                "/תורה",
                "בראשית ברא אלהים את השמים ואת הארץ",
                index + 1,
                false,
                "/library/bereshit.json",
                Some(42),
                None,
                None,
            )
            .unwrap();
        lines.push(SemanticBookLineInput {
            line_id: id,
            section_id: 42,
            text: "בראשית ברא אלהים את השמים ואת הארץ".to_owned(),
            line_hash: 100 + index,
            reference: format!("בראשית א:{}", index + 1),
            segment: index + 1,
        });
    }
    engine.commit().unwrap();

    let model_path = root.path().join("mock.gguf");
    write_stub_gguf(&model_path, 3).unwrap();
    engine
        .configure_semantic(search_engine::api::search_engine::SemanticConfigInput {
            root_dir: root.path().join("semantic").to_string_lossy().into_owned(),
            model_path: model_path.to_string_lossy().into_owned(),
            model_id: "test-mock".to_owned(),
            embedding_dim: 64,
        })
        .unwrap();
    engine
        .semantic_index_books(vec![SemanticBookInput {
            source_book_key: "/library/bereshit.json".to_owned(),
            title: "בראשית".to_owned(),
            content_fingerprint: 123,
            is_pdf: false,
            topics: "/תורה".to_owned(),
            extra_facets: Vec::new(),
            lines,
        }])
        .unwrap();

    let response = engine
        .search_semantic(
            "בראשית ברא".to_owned(),
            Vec::new(),
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::SemanticOnly,
            Some(SemanticGroupingMode::SameSection),
            false,
            false,
        )
        .unwrap();

    assert_eq!(response.results.len(), 1);
    assert_eq!(response.results[0].merged_count, GROUP_SIZE as u32);
    assert_eq!(response.results[0].merged.len(), 10);
}
