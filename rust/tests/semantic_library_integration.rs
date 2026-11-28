//! The library source is process-global. Keep its mutations in a separate test binary.

#![cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]

use search_engine::api::search_engine::{
    SearchEngine, SemanticBookInput, SemanticBookLineInput, SemanticRetrievalMode,
    SemanticSearchResponse,
};
use tempfile::TempDir;

#[path = "support/semantic_mock.rs"]
mod semantic_mock;
use semantic_mock::*;

const TOPICS: &str = "/תורה";

/// External and stored text must agree; suspending the source must report unavailable text.
#[test]
fn library_text_reaches_the_sidecar_path_like_stored_text() {
    use search_engine::api::search_engine::{
        configure_line_source, resume_line_source, suspend_line_source, TextStatus, TextStorage,
    };
    let rows = [
        "<h1>בראשית</h1>",
        "בראשית ברא אלהים את השמים ואת הארץ",
        "והארץ היתה תהו ובהו וחשך על פני תהום",
        "ויאמר אלהים יהי אור ויהי אור",
    ];
    let root = TempDir::new().unwrap();
    let db = root.path().join("seforim.db");
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER NOT NULL,
                                lineIndex INTEGER NOT NULL);
             CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
             CREATE TABLE line_content (id INTEGER PRIMARY KEY, content TEXT NOT NULL);",
        )
        .unwrap();
        for (index, row) in rows.iter().enumerate() {
            conn.execute(
                "INSERT INTO line (id, bookId, lineIndex) VALUES (?1, 1, ?2)",
                rusqlite::params![index as i64 + 10, index as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                rusqlite::params![index as i64 + 10, row],
            )
            .unwrap();
        }
    }
    configure_line_source(db.to_string_lossy().into_owned()).unwrap();

    let build = |name: &str, storage: TextStorage, stored_lines: Option<Vec<u32>>| {
        let dir = root.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("tantivy")).unwrap();
        let mut engine = SearchEngine::new(dir.join("tantivy").to_str().unwrap());
        // Only a whole book can keep its text in the library database.
        let added = engine
            .add_text_book(
                "בראשית".to_string(),
                TOPICS.to_string(),
                "id:1".to_string(),
                rows.join("\n"),
                None,
                storage,
                stored_lines,
                None,
            )
            .unwrap();
        assert_eq!(added as usize, rows.len());
        engine.commit().unwrap();
        let semantic_root = TempDir::new_in(&dir).unwrap();
        configure(&mut engine, &semantic_root);
        engine
            .semantic_index_books(vec![SemanticBookInput {
                source_book_key: "id:1".to_owned(),
                title: "בראשית".to_owned(),
                content_fingerprint: 1,
                is_pdf: false,
                topics: TOPICS.to_owned(),
                extra_facets: Vec::new(),
                lines: rows
                    .iter()
                    .enumerate()
                    .map(|(segment, row)| SemanticBookLineInput {
                        line_id: (1u64 << 32) + segment as u64 + 1,
                        // What add_text_book gives every line under the book's heading.
                        section_id: (1u64 << 32) + 1,
                        text: row.to_string(),
                        line_hash: 7,
                        reference: "בראשית".to_owned(),
                        segment: segment as u64,
                    })
                    .collect(),
            }])
            .unwrap();
        (engine, semantic_root)
    };
    let (stored, _a) = build("stored", TextStorage::InIndex, None);
    let (external, _b) = build("external", TextStorage::LibraryDb, None);
    // Row 1 keeps its text in the index, as an image row of an official book does.
    let kept_id = (1u64 << 32) + 2;
    let (mixed, _c) = build("mixed", TextStorage::LibraryDb, Some(vec![1]));

    let flatten = |response: &SemanticSearchResponse| -> Vec<String> {
        let mut out: Vec<String> = response
            .results
            .iter()
            .map(|r| {
                format!(
                    "{}|{}|{}|{:?}",
                    r.id, r.snippet_html, r.is_highlighted, r.text_status
                )
            })
            .collect();
        out.sort();
        out
    };
    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::SemanticOnly,
        SemanticRetrievalMode::LexicalOnly,
    ] {
        for query in ["בראשית ברא", "אלהים", "אור"] {
            let (a, b) = (exact(&stored, query, mode), exact(&external, query, mode));
            assert_eq!(flatten(&a), flatten(&b), "{query} {mode:?}");
            let c = exact(&mixed, query, mode);
            assert_eq!(flatten(&a), flatten(&c), "{query} {mode:?}");
            assert!(!b.results.is_empty(), "{query} {mode:?}");
            assert!(b.results.iter().all(|r| r.text_status == TextStatus::Ok));
        }
    }

    suspend_line_source().unwrap();
    for mode in [
        SemanticRetrievalMode::Hybrid,
        SemanticRetrievalMode::SemanticOnly,
    ] {
        let response = exact(&external, "אלהים", mode);
        assert!(!response.results.is_empty());
        for r in &response.results {
            assert_eq!(r.text_status, TextStatus::Unavailable, "{mode:?}");
            assert!(r.snippet_html.is_empty());
            assert!(!r.is_highlighted);
        }
        // The stored row never touches the suspended source; the others still need it.
        let response = exact(&mixed, "אלהים", mode);
        let kept = response.results.iter().find(|r| r.id == kept_id).unwrap();
        assert_eq!(kept.text_status, TextStatus::Ok, "{mode:?}");
        assert!(kept.snippet_html.contains("אלהים"), "{mode:?}");
        assert!(response
            .results
            .iter()
            .any(|r| r.id != kept_id && r.text_status == TextStatus::Unavailable));
    }
    resume_line_source().unwrap();
}
