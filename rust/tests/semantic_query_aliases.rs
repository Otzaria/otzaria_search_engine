//! Smart lexical recall is tested without a configured model or any vectors.

use std::collections::BTreeSet;

use search_engine::api::search_engine::{
    SearchEngine, SemanticCancellationToken, SemanticErrorKind, SemanticExecutedMode,
    SemanticLexicalMode, SemanticResultSource, SemanticRetrievalMode, SemanticSearchResponse,
};
use tempfile::TempDir;

const NAME: &str = "\u{05d9}\u{05d4}\u{05d5}\u{05d4}";

fn index(lines: &[String]) -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    for (at, line) in lines.iter().enumerate() {
        engine
            .add_document(
                at as u32,
                "personal",
                "reference",
                "/personal",
                line,
                at as u64,
                false,
                "/books/personal.txt",
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    (engine, dir)
}

fn search(
    engine: &SearchEngine,
    query: &str,
    mode: SemanticLexicalMode,
    retrieval: SemanticRetrievalMode,
    marks: (bool, bool),
    limit: u32,
    offset: u32,
) -> SemanticSearchResponse {
    engine
        .search_semantic(
            query.to_string(),
            vec!["/personal".to_string()],
            limit,
            offset,
            mode,
            0,
            retrieval,
            None,
            marks.0,
            marks.1,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap()
}

fn segments(response: &SemanticSearchResponse) -> BTreeSet<u64> {
    response.results.iter().map(|hit| hit.segment).collect()
}

fn marked(snippet: &str) -> String {
    regex::Regex::new(r"<font color=red>(.*?)</font>")
        .unwrap()
        .captures_iter(snippet)
        .map(|part| part[1].to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn smart_placeholder_fallback_retains_original_and_canonical_words_per_position() {
    let (engine, _dir) = index(&[
        format!("{NAME} טוב {NAME}"),
        format!("יקוק טוב {NAME}"),
        format!("{NAME} טוב ידוד"),
        "יקוק טוב ידוד".to_string(),
        format!("{NAME} רע {NAME}"),
    ]);
    for mode in [SemanticLexicalMode::Exact, SemanticLexicalMode::Fuzzy] {
        for retrieval in [
            SemanticRetrievalMode::Hybrid,
            SemanticRetrievalMode::LexicalOnly,
        ] {
            for query in ["יקוק טוב ידוד", "\"יקוק טוב ידוד\""] {
                let first = search(&engine, query, mode, retrieval, (false, false), 2, 0);
                let second = search(&engine, query, mode, retrieval, (false, false), 2, 2);
                assert_eq!(
                    first.lexical_total_count, 4,
                    "every word keeps both spellings: {query}"
                );
                assert_eq!(first.total_count, 4);
                assert_eq!(second.total_count, 4);
                assert!(first.executed_mode == SemanticExecutedMode::LexicalOnly);
                assert_eq!(
                    first.fallback_kind,
                    Some(if cfg!(feature = "semantic-integration") {
                        SemanticErrorKind::NotConfigured
                    } else {
                        SemanticErrorKind::FeatureNotInBuild
                    })
                );
                assert!(first.counts_are_exact && second.counts_are_exact);
                assert!(first.has_more && !second.has_more);
                let mut found = segments(&first);
                assert!(
                    found.is_disjoint(&segments(&second)),
                    "pages must not repeat a hit"
                );
                found.extend(segments(&second));
                assert_eq!(found, BTreeSet::from([0, 1, 2, 3]));
                for hit in first.results.iter().chain(&second.results) {
                    assert!(hit.source == SemanticResultSource::Lexical);
                    assert!(hit.is_highlighted, "{}", hit.snippet_html);
                    let painted = marked(&hit.snippet_html);
                    let expected = match hit.segment {
                        0 => [NAME, NAME],
                        1 => ["יקוק", NAME],
                        2 => [NAME, "ידוד"],
                        3 => ["יקוק", "ידוד"],
                        _ => unreachable!(),
                    };
                    assert!(
                        expected.iter().all(|word| painted.contains(word)),
                        "{}",
                        hit.snippet_html
                    );
                }
                let count = search(
                    &engine,
                    query,
                    mode,
                    SemanticRetrievalMode::SemanticOnly,
                    (false, false),
                    10,
                    0,
                );
                assert!(count.results.is_empty());
                assert_eq!(count.lexical_total_count, 4);
            }
        }
    }
}

#[test]
fn smart_placeholder_partial_quotes_keep_adjacency_and_outside_occurrences() {
    let (engine, _dir) = index(&[
        format!("{NAME} טוב יקוק רחוק"),
        format!("יקוק טוב {NAME}"),
        format!("{NAME} רחוק טוב יקוק"),
        format!("{NAME} טוב אחר ידוד"),
    ]);
    let quoted = search(
        &engine,
        "\"יקוק טוב\" יקוק",
        SemanticLexicalMode::Fuzzy,
        SemanticRetrievalMode::Hybrid,
        (false, false),
        10,
        0,
    );
    assert_eq!(segments(&quoted), BTreeSet::from([0, 1, 3]));
    assert_eq!(quoted.lexical_total_count, 3);
    assert!(quoted.results.iter().all(|hit| hit.is_highlighted));
    let unquoted = search(
        &engine,
        "יקוק טוב",
        SemanticLexicalMode::Fuzzy,
        SemanticRetrievalMode::Hybrid,
        (false, false),
        10,
        0,
    );
    assert_eq!(segments(&unquoted), BTreeSet::from([0, 1, 2, 3]));
}

#[test]
fn smart_placeholder_mark_constraints_apply_to_both_spellings() {
    let (engine, _dir) = index(&[
        "יְקוק טוב".to_string(),
        "\u{05d9}ְ\u{05d4}\u{05d5}\u{05d4} טוב".to_string(),
        "יקוק טוב".to_string(),
        format!("{NAME} טוב"),
        "י֑קוק טוב".to_string(),
        "\u{05d9}֑\u{05d4}\u{05d5}\u{05d4} טוב".to_string(),
    ]);
    for mode in [SemanticLexicalMode::Exact, SemanticLexicalMode::Fuzzy] {
        for (query, marks, expected) in [
            ("יְקוק טוב", (true, false), BTreeSet::from([0, 1])),
            ("י֑קוק טוב", (false, true), BTreeSet::from([4, 5])),
            (
                "יְקוק טוב",
                (false, false),
                BTreeSet::from([0, 1, 2, 3, 4, 5]),
            ),
        ] {
            let response = search(
                &engine,
                query,
                mode,
                SemanticRetrievalMode::Hybrid,
                marks,
                10,
                0,
            );
            assert_eq!(segments(&response), expected, "{query}");
            assert_eq!(response.lexical_total_count as usize, expected.len());
            assert!(response.results.iter().all(|hit| hit.is_highlighted));
        }
    }
}

#[test]
fn many_placeholder_positions_use_term_alternatives_without_a_cartesian_limit() {
    let (engine, _dir) = index(&[
        "יקוק ידוד יקוק ידוד יקוק ידוד".to_string(),
        std::iter::repeat_n(NAME, 6).collect::<Vec<_>>().join(" "),
        format!("יקוק {NAME} יקוק {NAME} יקוק {NAME}"),
    ]);
    let response = search(
        &engine,
        "\"יקוק ידוד יקוק ידוד יקוק ידוד\"",
        SemanticLexicalMode::Fuzzy,
        SemanticRetrievalMode::Hybrid,
        (false, false),
        10,
        0,
    );
    assert_eq!(segments(&response), BTreeSet::from([0, 1, 2]));
    assert_eq!(response.lexical_total_count, 3);
    assert!(response.results.iter().all(|hit| hit.is_highlighted));
}

#[test]
fn smart_placeholder_expansion_respects_facets_and_leaves_regular_exact_unchanged() {
    let (mut engine, _dir) = index(&["יקוק".to_string(), NAME.to_string()]);
    engine
        .add_document(
            0,
            "other",
            "reference",
            "/elsewhere",
            NAME,
            10,
            false,
            "/books/other.txt",
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    let smart = search(
        &engine,
        "יקוק",
        SemanticLexicalMode::Exact,
        SemanticRetrievalMode::Hybrid,
        (false, false),
        10,
        0,
    );
    assert_eq!(segments(&smart), BTreeSet::from([0, 1]));
    let regular = engine
        .search_and_count_exact(
            "יקוק".to_string(),
            vec!["/personal".to_string()],
            10,
            0,
            search_engine::api::search_engine::ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(regular.total_count, 1);
    assert_eq!(regular.results[0].segment, 0);
}

#[test]
fn smart_placeholder_fuzzy_keeps_dictionary_forms_and_literal_quotes() {
    let (mut engine, dir) = index(&[
        format!("{NAME} הלכתי"),
        format!("{NAME} הולך"),
        "יקוק הולך".to_string(),
        format!("{NAME} רחוק הלכתי"),
    ]);
    let path = dir.path().join("lexical.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "CREATE TABLE base (id INTEGER PRIMARY KEY, value TEXT UNIQUE);
         CREATE TABLE surface (id INTEGER PRIMARY KEY, value TEXT UNIQUE, base_id INTEGER, notes TEXT);
         CREATE TABLE variant (id INTEGER PRIMARY KEY, value TEXT UNIQUE);
         CREATE TABLE surface_variant (surface_id INTEGER, variant_id INTEGER, PRIMARY KEY (surface_id, variant_id));
         INSERT INTO base VALUES (1, 'הלכ');
         INSERT INTO surface VALUES (1, 'הלכתי', 1, NULL), (2, 'הולכ', 1, NULL);",
    ).unwrap();
    drop(db);
    assert!(engine.set_magic_dictionary_path(path.to_str().unwrap().to_string()));
    let fuzzy = search(
        &engine,
        "יקוק הלכתי",
        SemanticLexicalMode::Fuzzy,
        SemanticRetrievalMode::Hybrid,
        (false, false),
        10,
        0,
    );
    assert_eq!(segments(&fuzzy), BTreeSet::from([0, 1, 2, 3]));
    for hit in fuzzy
        .results
        .iter()
        .filter(|hit| matches!(hit.segment, 1 | 2))
    {
        assert!(
            marked(&hit.snippet_html).contains("הולך"),
            "{}",
            hit.snippet_html
        );
    }
    let quoted = search(
        &engine,
        "\"יקוק הלכתי\"",
        SemanticLexicalMode::Fuzzy,
        SemanticRetrievalMode::Hybrid,
        (false, false),
        10,
        0,
    );
    assert_eq!(segments(&quoted), BTreeSet::from([0]));
    let partial = search(
        &engine,
        "יקוק \"הלכתי\"",
        SemanticLexicalMode::Fuzzy,
        SemanticRetrievalMode::Hybrid,
        (false, false),
        10,
        0,
    );
    assert_eq!(segments(&partial), BTreeSet::from([0, 3]));
}
