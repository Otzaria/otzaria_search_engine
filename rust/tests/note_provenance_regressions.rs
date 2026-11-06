use search_engine::api::search_engine::{
    ResultsOrder, SearchEngine, SemanticLexicalMode, TextStorage,
};

#[test]
fn oversized_quoted_snippet_keeps_note_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    engine.open_index_writer().unwrap();
    let phrase = "מילה ".repeat(100).trim().to_string();
    let raw = format!("<i class='footnote'>{phrase} סוף</i>");
    engine
        .add_text_book(
            "ספר".into(),
            "/notes".into(),
            "/review.txt".into(),
            raw,
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    let short = engine
        .search_smart_lexical(
            "\"מילה מילה\" \"מילה מילה\"".into(),
            vec![],
            10,
            0,
            SemanticLexicalMode::Fuzzy,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(short.len(), 1);
    assert!(
        short[0].text.contains("<otzaria-note>"),
        "control: {}",
        short[0].text
    );
    let query = format!("\"{phrase}\" \"{phrase}\"");
    let hits = engine
        .search_smart_lexical(
            query,
            vec![],
            10,
            0,
            SemanticLexicalMode::Fuzzy,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].text.contains("<otzaria-note>"),
        "note provenance disappeared in cropped fallback: {}",
        hits[0].text
    );
}

#[test]
fn note_spanning_lines_and_preparation_windows_keeps_note_provenance() {
    let raw = format!("גוף<sup class='footnote-marker'>א\nב</sup><i class=footnote>הערהראשונה\n{}הערהשנייה</i> סוף\n<i class=footnote>לאסגורה\nאףהיא", "פסקה\n".repeat(300));
    let mut snapshots = Vec::new();
    for mode in 0..3 {
        let dir = tempfile::tempdir().unwrap();
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        engine.set_economy_indexing(mode == 1).unwrap();
        engine.open_index_writer().unwrap();
        if mode == 2 {
            engine
                .add_text_book_bytes(
                    "ספר".into(),
                    "/notes".into(),
                    "/review.txt".into(),
                    raw.as_bytes().to_vec(),
                    None,
                    TextStorage::InIndex,
                    None,
                    None,
                )
                .unwrap();
        } else {
            engine
                .add_text_book(
                    "ספר".into(),
                    "/notes".into(),
                    "/review.txt".into(),
                    raw.clone(),
                    None,
                    TextStorage::InIndex,
                    None,
                    None,
                )
                .unwrap();
        }
        engine.commit().unwrap();
        let mut snapshot = Vec::new();
        for query in ["הערהראשונה", "הערהשנייה", "לאסגורה", "אףהיא"]
        {
            let hits = engine
                .search_exact(
                    query.into(),
                    vec![],
                    10,
                    0,
                    ResultsOrder::Relevance,
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap();
            assert_eq!(hits.len(), 1);
            let hit = &hits[0];
            assert_eq!(
                hit.text.contains("<otzaria-note>"),
                query.starts_with("הערה"),
                "mode {mode}, {query}: {}",
                hit.text
            );
            assert_eq!(
                hit.text.contains("</otzaria-note>"),
                query.starts_with("הערה")
            );
            let plain = engine.get_document_by_id(hit.id).unwrap().unwrap().text;
            assert!(!plain.contains("otzaria-note"));
            snapshot.push((hit.id, hit.segment, hit.text.clone(), plain));
        }
        assert!(snapshot[0]
            .2
            .contains("<otzaria-note-marker>ב</otzaria-note-marker>"));
        snapshots.push(snapshot);
    }
    assert!(snapshots.windows(2).all(|pair| pair[0] == pair[1]));
}
