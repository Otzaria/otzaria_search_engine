//! Partial quotations constrain their own literal words, leaving outside words approximate.
//! Both the configured sidecar route and the lexical fallback use the same query and counts.

use search_engine::api::search_engine::{
    ResultsOrder, SearchEngine, SemanticCancellationToken, SemanticLexicalMode,
    SemanticRetrievalMode, SemanticSearchResponse,
};
use tempfile::TempDir;

const BOOK: &str = "/quotes/book";

fn engine(lines: &[&str], dictionary: bool) -> (SearchEngine, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("index");
    std::fs::create_dir(&index).unwrap();
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    for (i, text) in lines.iter().enumerate() {
        engine
            .add_document(
                i as u64 + 1,
                "book",
                "ref",
                "/quotes",
                text,
                0,
                false,
                BOOK,
                None,
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    if dictionary {
        let path = dir.path().join("lexical.db");
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE base(id INTEGER PRIMARY KEY, value TEXT UNIQUE);
             CREATE TABLE surface(id INTEGER PRIMARY KEY, value TEXT UNIQUE, base_id INTEGER);
             CREATE TABLE variant(id INTEGER PRIMARY KEY, value TEXT UNIQUE);
             CREATE TABLE surface_variant(surface_id INTEGER, variant_id INTEGER);
             INSERT INTO base VALUES(1, 'הלך');
             INSERT INTO surface VALUES(1, 'הלכתי', 1);",
        )
        .unwrap();
        drop(db);
        assert!(engine.set_magic_dictionary_path(path.to_string_lossy().into_owned()));
    }
    (engine, dir)
}

fn search(
    engine: &SearchEngine,
    query: &str,
    distance: u8,
    mode: SemanticRetrievalMode,
    nikud: bool,
) -> SemanticSearchResponse {
    engine
        .search_semantic(
            query.into(),
            vec![],
            100,
            0,
            SemanticLexicalMode::Fuzzy,
            distance,
            mode,
            None,
            nikud,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
}

fn ids(response: &SemanticSearchResponse) -> Vec<u64> {
    let mut ids: Vec<_> = response.results.iter().map(|hit| hit.id).collect();
    ids.sort_unstable();
    ids
}

fn assert_partial_quotations(engine: &SearchEngine, dictionary: bool) {
    for query in ["הלך \"שמע ישראל\"", "\"שמע ישראל\" הלך", "הלך “שמע ישראל”"]
    {
        let expected = if dictionary { vec![1, 5] } else { vec![5] };
        let result = search(engine, query, 0, SemanticRetrievalMode::Hybrid, false);
        assert_eq!(ids(&result), expected, "{query}, dictionary={dictionary}");
        assert_eq!(result.lexical_total_count, expected.len() as u32);
        let count_only = search(engine, query, 0, SemanticRetrievalMode::SemanticOnly, false);
        assert_eq!(count_only.lexical_total_count, expected.len() as u32);
    }
    // Edits remain permitted outside the quotation, while the typo, reversed words and
    // extra word inside it (rows 2–4) never match, even with a dictionary loaded.
    let result = search(
        engine,
        "הלכתי \"שמע ישראל\"",
        1,
        SemanticRetrievalMode::Hybrid,
        false,
    );
    let expected = if dictionary { vec![1, 5] } else { vec![1] };
    assert_eq!(ids(&result), expected);
    assert_eq!(result.lexical_total_count, expected.len() as u32);
    let result = search(
        engine,
        "הלכת \"שמע ישראל\"",
        1,
        SemanticRetrievalMode::Hybrid,
        false,
    );
    let expected = vec![1, 6];
    assert_eq!(ids(&result), expected);
    assert_eq!(result.lexical_total_count, expected.len() as u32);
}

const PARTIAL: &[&str] = &[
    "הלכתי אתמול שמע ישראל",
    "הלכתי אתמול שמע אישראל",
    "הלכתי אתמול ישראל שמע",
    "הלכתי אתמול שמע לכל ישראל",
    "הלך אתמול שמע ישראל",
    "הלכה אתמול שמע ישראל",
];

#[test]
fn fallback_partial_quotes_keep_literal_words_and_loose_outside_words() {
    for dictionary in [false, true] {
        let (engine, _dir) = engine(PARTIAL, dictionary);
        assert_partial_quotations(&engine, dictionary);
    }
}

#[test]
fn single_word_quotes_are_literal_and_separate_quotes_are_independent() {
    for dictionary in [false, true] {
        let (engine, _dir) = engine(
            &[
                "הלך האיש שמע",
                "הלכתי האיש שמע",
                "הלך האיש שמא",
                "שמע האיש הלך",
            ],
            dictionary,
        );
        let result = search(
            &engine,
            "הלך \"שמע\"",
            1,
            SemanticRetrievalMode::Hybrid,
            false,
        );
        let expected = if dictionary {
            vec![1, 2, 4]
        } else {
            vec![1, 4]
        };
        assert_eq!(ids(&result), expected);
        assert_eq!(result.lexical_total_count, expected.len() as u32);
        let result = search(
            &engine,
            "\"הלך\" \"שמע\"",
            1,
            SemanticRetrievalMode::Hybrid,
            false,
        );
        assert_eq!(ids(&result), [1, 4]);
    }
}

#[test]
fn fully_quoted_query_is_verbatim_and_an_unpaired_quote_does_not_constrain() {
    let (engine, _dir) = engine(&["שמע ישראל", "שמע לכל ישראל", "שמע אישראל"], true);
    assert_eq!(
        ids(&search(
            &engine,
            "\"שמע ישראל\"",
            1,
            SemanticRetrievalMode::Hybrid,
            false
        )),
        [1]
    );
    assert_eq!(
        ids(&search(
            &engine,
            "\"שמע ישראל",
            1,
            SemanticRetrievalMode::Hybrid,
            false
        )),
        [1, 2, 3]
    );
}

#[test]
fn quoted_acronyms_and_vocalization_keep_their_literal_contents() {
    let (engine, _dir) = engine(
        &[
            "הלך אתמול רמב״ם",
            "הלכתי אתמול רמב״ם",
            "הלך אתמול רמב״ן",
            "הלך אתמול שָׁמַע ישראל",
            "הלך אתמול שְׁמַע ישראל",
        ],
        true,
    );
    assert_eq!(
        ids(&search(
            &engine,
            "הלך \"רַמְבַּ״ם\"",
            0,
            SemanticRetrievalMode::Hybrid,
            false
        )),
        [1, 2]
    );
    assert_eq!(
        ids(&search(
            &engine,
            "הלך \"שָׁמַע ישראל\"",
            1,
            SemanticRetrievalMode::Hybrid,
            true
        )),
        [4]
    );
    // A bare acronym remains approximate; its internal gershayim is not a quotation.
    assert_eq!(
        ids(&search(
            &engine,
            "הלך רמב״ם",
            1,
            SemanticRetrievalMode::LexicalOnly,
            false
        )),
        [1, 2, 3]
    );
}

#[test]
fn ordinary_approximate_quoted_phrases_keep_dictionary_expansion() {
    let (engine, _dir) = engine(&["הלכתי שמע", "הלך שמע"], true);
    let results = engine
        .search_fuzzy(
            "\"הלך שמע\"".into(),
            vec![],
            100,
            0,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
        )
        .unwrap();
    let mut got: Vec<_> = results.iter().map(|hit| hit.id).collect();
    got.sort_unstable();
    assert_eq!(got, [1, 2]);
    assert_eq!(
        engine
            .count_fuzzy("\"הלך שמע\"".into(), vec![], 0, false, false)
            .unwrap(),
        2
    );
}

#[cfg(all(feature = "semantic-mock", not(feature = "semantic-onnx")))]
#[test]
fn configured_sidecar_partial_quotes_use_the_same_matching_and_counts() {
    use otzaria_semantic_search::semantic::embedding::mock::write_stub_onnx_package;
    use search_engine::api::search_engine::{
        SemanticBookInput, SemanticBookLineInput, SemanticConfigInput,
    };
    for dictionary in [false, true] {
        let (mut engine, dir) = engine(PARTIAL, dictionary);
        let model = write_stub_onnx_package(&dir.path().join("model"));
        engine
            .configure_semantic(SemanticConfigInput {
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
        engine
            .semantic_index_books(vec![SemanticBookInput {
                source_book_key: BOOK.into(),
                title: "book".into(),
                content_fingerprint: 1,
                is_pdf: false,
                topics: "/quotes".into(),
                extra_facets: vec![],
                lines: PARTIAL
                    .iter()
                    .enumerate()
                    .map(|(i, text)| SemanticBookLineInput {
                        line_id: i as u64 + 1,
                        section_id: i as u64 + 1,
                        text: (*text).into(),
                        line_hash: i as u64 + 1,
                        reference: "ref".into(),
                        segment: 0,
                    })
                    .collect(),
            }])
            .unwrap();
        assert_partial_quotations(&engine, dictionary);
    }
}
