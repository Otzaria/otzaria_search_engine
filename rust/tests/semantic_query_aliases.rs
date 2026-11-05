//! Smart lexical recall is tested without a configured model or any vectors.

use std::collections::BTreeSet;

use search_engine::api::search_engine::{
    generate_smart_highlight_pattern, BookOrderEntry, ResultsOrder, SearchEngine, SearchResult,
    SemanticCancellationToken, SemanticErrorKind, SemanticExecutedMode, SemanticLexicalMode,
    SemanticResultSource, SemanticRetrievalMode, SemanticSearchResponse, TextStorage,
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

fn catalogue(
    engine: &SearchEngine,
    query: &str,
    mode: SemanticLexicalMode,
    marks: (bool, bool),
    limit: u32,
    offset: u32,
) -> Vec<SearchResult> {
    engine
        .search_smart_lexical(
            query.to_string(),
            vec!["/personal".to_string()],
            limit,
            offset,
            mode,
            0,
            ResultsOrder::Catalogue,
            marks.0,
            marks.1,
            None,
        )
        .unwrap()
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
            let in_book = catalogue(&engine, query, mode, marks, 10, 0);
            assert_eq!(
                in_book
                    .iter()
                    .map(|hit| hit.segment)
                    .collect::<BTreeSet<_>>(),
                expected
            );
            assert!(in_book
                .iter()
                .all(|hit| hit.text.contains("<font color=red>")));
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
    let regular_fuzzy = engine
        .search_fuzzy(
            "יקוק".to_string(),
            vec!["/personal".to_string()],
            10,
            0,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(
        regular_fuzzy
            .iter()
            .map(|hit| hit.segment)
            .collect::<Vec<_>>(),
        [0]
    );
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
    for (query, expected) in [
        ("יקוק הלכתי", BTreeSet::from([0, 1, 2, 3])),
        ("\"יקוק הלכתי\"", BTreeSet::from([0])),
        ("יקוק \"הלכתי\"", BTreeSet::from([0, 3])),
    ] {
        let in_book = catalogue(
            &engine,
            query,
            SemanticLexicalMode::Fuzzy,
            (false, false),
            10,
            0,
        );
        assert_eq!(
            in_book
                .iter()
                .map(|hit| hit.segment)
                .collect::<BTreeSet<_>>(),
            expected
        );
    }
}

#[test]
fn smart_catalogue_matches_global_recall_and_keeps_every_spelling_in_order() {
    let (engine, _dir) = index(&[
        format!("{NAME} טוב {NAME}"),
        format!("יקוק טוב {NAME}"),
        format!("{NAME} טוב ידוד"),
        "יקוק טוב ידוד".to_string(),
        format!("{NAME} רע {NAME}"),
    ]);
    for mode in [SemanticLexicalMode::Exact, SemanticLexicalMode::Fuzzy] {
        for query in ["יקוק טוב ידוד", "\"יקוק טוב ידוד\"", "\"יקוק טוב\" ידוד"]
        {
            let first = catalogue(&engine, query, mode, (false, false), 2, 0);
            let second = catalogue(&engine, query, mode, (false, false), 2, 2);
            let all = first.iter().chain(&second).collect::<Vec<_>>();
            assert_eq!(
                all.iter().map(|hit| hit.segment).collect::<Vec<_>>(),
                [0, 1, 2, 3]
            );
            let global = search(
                &engine,
                query,
                mode,
                SemanticRetrievalMode::LexicalOnly,
                (false, false),
                10,
                0,
            );
            assert_eq!(
                all.iter().map(|hit| hit.id).collect::<BTreeSet<_>>(),
                global.results.iter().map(|hit| hit.id).collect()
            );
            assert!(all.iter().all(|hit| hit.text.contains("<font color=red>")));
            assert!(catalogue(&engine, query, mode, (false, false), 0, 0).is_empty());
        }
    }
}

#[test]
fn smart_catalogue_paginates_after_literal_constraints_beyond_five_thousand() {
    let lines = (0..5105)
        .map(|at| {
            let left = if at % 2 == 0 { NAME } else { "יקוק" };
            let right = if at % 4 < 2 { NAME } else { "ידוד" };
            let middle = if at < 50 { "רחוק טוב" } else { "טוב" };
            format!("{left} {middle} {right}")
        })
        .collect::<Vec<_>>();
    let (engine, _dir) = index(&lines);
    let query = "\"יקוק טוב\" ידוד";
    let first = catalogue(
        &engine,
        query,
        SemanticLexicalMode::Fuzzy,
        (false, false),
        5000,
        0,
    );
    assert_eq!(first.len(), 5000);
    assert_eq!(
        first.iter().map(|hit| hit.segment).collect::<Vec<_>>(),
        (50..5050).collect::<Vec<_>>()
    );
    let tail = catalogue(
        &engine,
        query,
        SemanticLexicalMode::Fuzzy,
        (false, false),
        100,
        5000,
    );
    assert_eq!(
        tail.iter().map(|hit| hit.segment).collect::<Vec<_>>(),
        (5050..5105).collect::<Vec<_>>()
    );
    assert!(catalogue(
        &engine,
        query,
        SemanticLexicalMode::Fuzzy,
        (false, false),
        10,
        5055
    )
    .is_empty());
}

#[test]
fn smart_catalogue_uses_book_ranks_and_the_existing_cancellation_handle() {
    let (mut engine, _dir) = index(&[NAME.to_string()]);
    for (book, segment) in [("/books/early.txt", 11), ("/books/unmapped.txt", 12)] {
        engine
            .add_document(
                0,
                "book",
                "line",
                "/personal",
                "יקוק",
                segment,
                false,
                book,
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    engine
        .set_book_order(vec![
            BookOrderEntry {
                file_path: "/books/early.txt".to_string(),
                catalogue_order: 0,
                generation_order: 0,
            },
            BookOrderEntry {
                file_path: "/books/personal.txt".to_string(),
                catalogue_order: 9,
                generation_order: 9,
            },
        ])
        .unwrap();
    let first = catalogue(
        &engine,
        "יקוק",
        SemanticLexicalMode::Fuzzy,
        (false, false),
        1,
        0,
    );
    let rest = catalogue(
        &engine,
        "יקוק",
        SemanticLexicalMode::Fuzzy,
        (false, false),
        5,
        1,
    );
    assert_eq!(
        first
            .iter()
            .chain(&rest)
            .map(|hit| hit.file_path.as_str())
            .collect::<Vec<_>>(),
        [
            "/books/early.txt",
            "/books/personal.txt",
            "/books/unmapped.txt"
        ]
    );
    engine.lexical_cancellation_handle().cancel(765);
    let cancelled = engine.search_smart_lexical(
        "יקוק".to_string(),
        vec![],
        10,
        0,
        SemanticLexicalMode::Fuzzy,
        0,
        ResultsOrder::Catalogue,
        false,
        false,
        Some(765),
    );
    assert!(cancelled
        .err()
        .unwrap()
        .to_string()
        .contains("lexical search cancelled"));
}

#[test]
fn smart_catalogue_only_allows_a_complete_quotation_to_cross_lines() {
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    // The book importer writes the line-edge fields used by cross-line search.
    // Independent add_document calls deliberately do not infer adjacent lines.
    engine
        .add_text_book(
            "personal".to_string(),
            "/personal".to_string(),
            "/books/personal.txt".to_string(),
            format!("התחלה {NAME}\nטוב וסוף\nיקוק טוב וסוף"),
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    let complete = catalogue(
        &engine,
        "\"יקוק טוב\"",
        SemanticLexicalMode::Fuzzy,
        (false, false),
        10,
        0,
    );
    assert_eq!(
        complete.iter().map(|hit| hit.segment).collect::<Vec<_>>(),
        [0, 2]
    );
    assert!(complete[0].continues_to_next_line);
    for query in ["\"יקוק טוב\" וסוף", "יקוק טוב וסוף"] {
        let partial = catalogue(
            &engine,
            query,
            SemanticLexicalMode::Fuzzy,
            (false, false),
            10,
            0,
        );
        assert_eq!(
            partial.iter().map(|hit| hit.segment).collect::<Vec<_>>(),
            [2]
        );
        assert!(!partial[0].continues_to_next_line);
    }
}

#[test]
fn smart_display_quotes_remain_groups_and_overlaps_preserve_every_word() {
    let (engine, _dir) = index(&["אב גד הו אבב טוב".to_string(), format!("{NAME} טוב ידוד")]);
    let pattern = |query: &str, mode, distance| {
        engine
            .generate_index_smart_highlight_pattern(query.to_string(), mode, distance, None)
            .unwrap()
            .unwrap()
            .matcher
            .unwrap()
    };
    let partial = pattern("\"אב גד\" טוב", SemanticLexicalMode::Fuzzy, 1);
    assert!(partial
        .find_word_matches("אב רחוק גד".to_string(), vec![])
        .is_empty());
    assert!(partial
        .find_word_matches("אבב גד".to_string(), vec![])
        .is_empty());
    assert!(!partial.allows_cross_line());
    let repeated = pattern("\"אב גד\" אב", SemanticLexicalMode::Fuzzy, 1);
    let matches = repeated.find_word_matches("אב גד אבב".to_string(), vec![]);
    assert_eq!(
        matches
            .iter()
            .map(|hit| (hit.start, hit.end))
            .collect::<Vec<_>>(),
        [(0, 5), (6, 9)]
    );
    let overlapping = pattern("\"אב גד\" \"גד הו\"", SemanticLexicalMode::Fuzzy, 0);
    let matches = overlapping.find_word_matches("😀 <b>אב גד הו</b>".to_string(), vec![]);
    assert_eq!(matches.len(), 1);
    assert_eq!((matches[0].start, matches[0].end), (6, 14));
    assert_eq!(
        matches[0]
            .word_ranges
            .iter()
            .map(|range| (range.start, range.end))
            .collect::<Vec<_>>(),
        [(0, 2), (3, 5), (6, 8)]
    );
    let exact = pattern("אב גד", SemanticLexicalMode::Exact, 1);
    assert!(exact
        .find_word_matches("אבב גד".to_string(), vec![])
        .is_empty());
    assert!(exact.allows_cross_line());
    assert!(pattern("\"אב גד\"", SemanticLexicalMode::Fuzzy, 1).allows_cross_line());
}

#[test]
fn smart_display_retains_canonical_matches_without_polluting_regular_patterns() {
    let (engine, _dir) = index(&[NAME.to_string(), "יקוק".to_string()]);
    for smart_first in [false, true] {
        for smart in [smart_first, !smart_first, smart_first] {
            let pattern = if smart {
                engine.generate_index_smart_highlight_pattern(
                    "יקוק".to_string(),
                    SemanticLexicalMode::Fuzzy,
                    0,
                    None,
                )
            } else {
                engine.generate_index_fuzzy_highlight_pattern("יקוק".to_string(), 0, None)
            }
            .unwrap()
            .unwrap()
            .matcher
            .unwrap();
            let canonical = "😀 <b>יְהֹוָה</b>".to_string();
            assert_eq!(!pattern.find_matches(canonical, vec![]).is_empty(), smart);
            assert_eq!(pattern.find_matches("יקוק".to_string(), vec![]).len(), 1);
        }
    }
}

#[test]
fn smart_display_observes_the_same_placeholder_gate_as_retrieval() {
    let (engine, _dir) = index(&["יקוק".to_string(), NAME.to_string()]);
    for mode in [SemanticLexicalMode::Exact, SemanticLexicalMode::Fuzzy] {
        for query in ["\u{fb1d}קוק", "\"\u{fb1d}קוק\""] {
            let hits = catalogue(&engine, query, mode, (false, false), 10, 0);
            assert_eq!(hits.iter().map(|hit| hit.segment).collect::<Vec<_>>(), [0]);
            let matcher = engine
                .generate_index_smart_highlight_pattern(query.to_string(), mode, 0, None)
                .unwrap()
                .unwrap()
                .matcher
                .unwrap();
            assert!(matcher.find_matches(NAME.to_string(), vec![]).is_empty());
            assert_eq!(matcher.find_matches("יקוק".to_string(), vec![]).len(), 1);
            let fallback = generate_smart_highlight_pattern(query.to_string(), mode)
                .unwrap()
                .matcher
                .unwrap();
            assert!(fallback.find_matches(NAME.to_string(), vec![]).is_empty());
            assert_eq!(fallback.find_matches("יקוק".to_string(), vec![]).len(), 1);
        }
    }
}

#[test]
fn smart_display_fallback_preserves_groups_without_an_index() {
    let fallback = generate_smart_highlight_pattern(
        "\"יקוק טוב\" סוף".to_string(),
        SemanticLexicalMode::Fuzzy,
    )
    .unwrap()
    .matcher
    .unwrap();
    assert!(!fallback.allows_cross_line());
    assert!(fallback
        .find_matches(format!("{NAME} רחוק טוב סוף"), vec![])
        .is_empty());
    let matches = fallback.find_matches(format!("{NAME} טוב סוף"), vec![]);
    assert_eq!(
        matches
            .iter()
            .map(|hit| hit.word_ranges.len())
            .sum::<usize>(),
        3
    );
    let incomplete = fallback.find_word_matches(format!("{NAME} רחוק טוב סוף"), vec![]);
    assert_eq!(incomplete.len(), 1);
    assert_eq!((incomplete[0].start, incomplete[0].end), (14, 17));
    let complete =
        generate_smart_highlight_pattern("\"יקוק טוב\"".to_string(), SemanticLexicalMode::Fuzzy)
            .unwrap()
            .matcher
            .unwrap();
    assert!(complete.allows_cross_line());
    assert!(!complete
        .find_matches(format!("{NAME} טוב"), vec![])
        .is_empty());
}

#[test]
fn smart_display_uses_the_query_analyzer_for_qere_ketiv_and_long_tokens() {
    let (engine, _dir) = index(&[
        "אב גד הו".to_string(),
        "גד הו".to_string(),
        "אב הו".to_string(),
    ]);
    let garbage = "x".repeat(128);
    for (query, expected, text, rejected, complete_quote) in [
        ("\"(אב) [גד] הו\"".to_string(), 0, "אב גד הו", "גד הו", true),
        (
            "\"(אב) [גד]\" הו".to_string(),
            0,
            "אב גד הו",
            "גד הו",
            false,
        ),
        (format!("\"אב {garbage} הו\""), 2, "אב הו", "גד הו", true),
        (format!("\"אב הו\" {garbage}"), 2, "אב הו", "גד הו", true),
    ] {
        for mode in [SemanticLexicalMode::Exact, SemanticLexicalMode::Fuzzy] {
            let hits = catalogue(&engine, &query, mode, (false, false), 10, 0);
            assert_eq!(
                hits.iter().map(|hit| hit.segment).collect::<Vec<_>>(),
                [expected],
                "{query} {mode:?}"
            );
            for pattern in [
                engine
                    .generate_index_smart_highlight_pattern(query.clone(), mode, 0, None)
                    .unwrap(),
                generate_smart_highlight_pattern(query.clone(), mode),
            ] {
                let matcher = pattern.unwrap().matcher.unwrap();
                assert_eq!(
                    matcher.allows_cross_line(),
                    mode == SemanticLexicalMode::Exact || complete_quote
                );
                assert!(!matcher.find_matches(text.to_string(), vec![]).is_empty());
                assert!(matcher
                    .find_matches(rejected.to_string(), vec![])
                    .is_empty());
            }
        }
    }
}
