//! Schema 6 dropped the quote-free twin the indexing tokenizer emitted beside every
//! quoted word (see `quote_expansion`). These tests build the same books twice — with the
//! twins, as schema 5 indexed them, and without — and require every search to answer
//! alike: the documents, the counts, the snippets and their highlights.

use std::collections::HashMap;

use tempfile::TempDir;

use crate::api::search_engine::{
    ResultsOrder, SearchEngine, SearchPageResult, SearchScope, TextStorage,
};

const BOOKS: &[(&str, &str, &str)] = &[
    (
        "ספר א",
        "id:1",
        "<h1>ספר א</h1>\n\
         כתב הרמב\"ם בהלכות תשובה\n\
         וכן דעת רמב\"ם ורש\"י בזה\n\
         פירש רש\"י על התורה\n\
         ש\"ץ עובר לפני התיבה\n\
         שליח צבור עובר לפני התיבה\n\
         רש\"י והרמבם חלקו\n\
         תוס' ד\"ה אמר רבי\n\
         ועל זה פירש\n\
         רש\"י בפירושו הארוך\n\
         <h2>פרק ב</h2>\n\
         אמרו רז\"ל במדרש\n\
         כמו שכתב אדמו\"ר הזקן\n\
         כָּתַב הָרַמְבַּ\"ם בְּסִפְרוֹ",
    ),
    (
        "ספר ב",
        "id:2",
        "<h1>ספר ב</h1>\n\
         דברי רמבם בהלכות\n\
         רמ\"בם צורה משובשת\n\
         פירש רשי בקצרה\n\
         ר' יוחנן אמר\n\
         תוס ישנים\n\
         אדמורים רבים",
    ),
];

fn build(dir: &TempDir, twins: bool) -> SearchEngine {
    let path = dir.path().to_str().unwrap();
    let mut engine = if twins {
        SearchEngine::with_quote_free_twins(path)
    } else {
        SearchEngine::new(path)
    };
    for (order, (title, file_path, text)) in BOOKS.iter().enumerate() {
        engine
            .add_text_book_placed(
                title.to_string(),
                "/root".to_string(),
                file_path.to_string(),
                order as u32,
                5,
                text.to_string(),
                None,
                TextStorage::InIndex,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    engine
}

fn lexical_db(dir: &TempDir) -> String {
    let path = dir.path().join("lexical.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE base (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
        CREATE TABLE surface (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE, base_id INTEGER NOT NULL REFERENCES base(id), notes TEXT);
        CREATE TABLE variant (id INTEGER PRIMARY KEY AUTOINCREMENT, value TEXT NOT NULL UNIQUE);
        CREATE TABLE surface_variant (surface_id INTEGER NOT NULL REFERENCES surface(id), variant_id INTEGER NOT NULL REFERENCES variant(id), PRIMARY KEY (surface_id, variant_id));
        INSERT INTO base (id, value) VALUES (1, 'אדמור'), (2, 'רשי');
        INSERT INTO surface (id, value, base_id) VALUES
            (1, 'אדמור', 1), (2, 'אדמורימ', 1), (3, 'רשי', 2), (4, 'כרשי', 2);
        "#,
    )
    .unwrap();
    path.to_str().unwrap().to_string()
}

type Options = HashMap<String, HashMap<String, bool>>;

fn options(word: &str, index: usize, names: &[&str]) -> Options {
    HashMap::from([(
        format!("{word}_{index}"),
        names.iter().map(|n| (n.to_string(), true)).collect(),
    )])
}

/// What a page shows, in a form two indexes can be compared by.
fn page(result: SearchPageResult, ranked: bool) -> String {
    let mut rows: Vec<String> = result
        .results
        .iter()
        .map(|r| {
            format!(
                "{} {} {:?} {}",
                r.id, r.continues_to_next_line, r.text_status, r.text
            )
        })
        .collect();
    if ranked {
        // BM25 statistics count the twins: the order of equally relevant hits may move.
        rows.sort();
    }
    format!(
        "total={} truncated={}\n{}",
        result.total_count,
        result.truncated,
        rows.join("\n")
    )
}

fn sorted_counts(counts: HashMap<String, u32>) -> Vec<(String, u32)> {
    let mut counts: Vec<_> = counts.into_iter().collect();
    counts.sort();
    counts
}

const EXACT: &[&str] = &[
    "רמבם",
    "רמב\"ם",
    "הרמבם",
    "רשי",
    "רש\"י",
    "שצ",
    "ש\"ץ",
    "תוס",
    "תוס'",
    "רזל",
    "דה",
    "ר",
    "אדמור",
    "פירש רשי",
    "כתב הרמבם",
    "דעת רמבם ורשי",
    "רשי והרמבם",
    "דה אמר רבי",
    "ר יוחנן",
    "שליח צבור",
];

/// Every search the application runs, over `engine`, as text.
fn answers(engine: &SearchEngine) -> Vec<String> {
    let mut out = Vec::new();
    let facets = || vec!["/root".to_string()];
    for &query in EXACT {
        for (order, ranked) in [
            (ResultsOrder::Catalogue, false),
            (ResultsOrder::Relevance, true),
        ] {
            let result = engine
                .search_and_count_exact(query.into(), facets(), 50, 0, order, false, false, None)
                .unwrap();
            out.push(format!("exact {query}: {}", page(result, ranked)));
        }
        let counts = engine
            .count_by_book_exact(query.into(), facets(), false, false, None)
            .unwrap();
        out.push(format!(
            "exact by book {query}: {:?}",
            sorted_counts(counts)
        ));
        for distance in [0u8, 1] {
            let result = engine
                .search_and_count_fuzzy(
                    query.into(),
                    facets(),
                    50,
                    0,
                    distance,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    None,
                )
                .unwrap();
            out.push(format!("fuzzy{distance} {query}: {}", page(result, false)));
        }
    }
    let advanced: Vec<(&str, u32, Options, SearchScope)> = vec![
        ("רמבם", 0, Options::new(), SearchScope::WordDistance),
        (
            "רמבם",
            0,
            options("רמבם", 0, &["קידומות"]),
            SearchScope::WordDistance,
        ),
        (
            "רמבם",
            0,
            options("רמבם", 0, &["קידומות", "סיומות"]),
            SearchScope::WordDistance,
        ),
        ("רמב\"ם", 0, Options::new(), SearchScope::WordDistance),
        (
            "רמב\"ם",
            0,
            options("רמב\"ם", 0, &["התעלם מגרשיים"]),
            SearchScope::WordDistance,
        ),
        (
            "רמב\"ם",
            0,
            options("רמב\"ם", 0, &["התעלם מגרשיים", "קידומות"]),
            SearchScope::WordDistance,
        ),
        (
            "רמב",
            0,
            options("רמב", 0, &["חלק ממילה"]),
            SearchScope::WordDistance,
        ),
        (
            "רמבם",
            0,
            options("רמבם", 0, &["שגיאות כתיב"]),
            SearchScope::WordDistance,
        ),
        (
            "רשי",
            0,
            options("רשי", 0, &["כתיב מלא/חסר"]),
            SearchScope::WordDistance,
        ),
        ("פירש רשי", 0, Options::new(), SearchScope::WordDistance),
        ("פירש רשי", 2, Options::new(), SearchScope::WordDistance),
        ("רשי פירש", 3, Options::new(), SearchScope::WordDistance),
        (
            "כתב רמבם",
            0,
            options("רמבם", 1, &["קידומות"]),
            SearchScope::WordDistance,
        ),
        ("רמבם רשי", 0, Options::new(), SearchScope::SameParagraph),
        ("רמבם רזל", 0, Options::new(), SearchScope::SameSection),
        (
            "ש\"ץ",
            0,
            options("ש\"ץ", 0, &["התעלם מגרשיים"]),
            SearchScope::WordDistance,
        ),
    ];
    for (query, distance, opts, scope) in advanced {
        let result = engine
            .search_and_count_advanced(
                query.into(),
                String::new(),
                facets(),
                50,
                0,
                distance,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                opts.clone(),
                HashMap::new(),
                ResultsOrder::Catalogue,
                false,
                false,
                scope,
                SearchScope::WordDistance,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let label: std::collections::BTreeMap<_, std::collections::BTreeMap<_, _>> = opts
            .iter()
            .map(|(word, names)| (word, names.iter().collect()))
            .collect();
        out.push(format!(
            "advanced {query} d{distance} {label:?}: {}",
            page(result, false)
        ));
    }
    // An opened book paints what the index terms of the query match.
    let lines: Vec<&str> = BOOKS.iter().flat_map(|(_, _, text)| text.lines()).collect();
    let mut paint =
        |label: String, pattern: Option<crate::api::search_engine::HighlightPattern>| {
            let Some(pattern) = pattern else {
                out.push(format!("{label}: none"));
                return;
            };
            let matcher = pattern
                .matcher
                .expect("an index pattern carries its matcher");
            let painted: Vec<String> = lines
                .iter()
                .map(|line| {
                    let matches = matcher
                        .find_matches(line.to_string(), pattern.word_boundary_eligible.clone());
                    format!(
                        "{:?}",
                        matches.iter().map(|m| (m.start, m.end)).collect::<Vec<_>>()
                    )
                })
                .collect();
            out.push(format!("{label}: {}", painted.join(" ")));
        };
    for (query, opts) in [
        ("רמבם", Options::new()),
        ("רמבם", options("רמבם", 0, &["קידומות"])),
        ("רמב\"ם", options("רמב\"ם", 0, &["התעלם מגרשיים"])),
        ("פירש רשי", Options::new()),
    ] {
        let pattern = engine
            .generate_index_highlight_pattern(
                query.into(),
                0,
                HashMap::new(),
                HashMap::new(),
                opts,
                None,
            )
            .unwrap();
        paint(format!("highlight {query}"), pattern);
    }
    for query in ["רמבם", "אדמור", "שצ"] {
        for distance in [0u8, 1] {
            let pattern = engine
                .generate_index_fuzzy_highlight_pattern(query.into(), distance)
                .unwrap();
            paint(format!("fuzzy highlight{distance} {query}"), pattern);
        }
    }
    // Vocalized: the `textVocalized` field had twins too.
    for query in ["כָּתַב הָרַמְבַּם", "הָרַמְבַּ\"ם"] {
        let result = engine
            .search_and_count_exact(
                query.into(),
                facets(),
                50,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
                None,
            )
            .unwrap();
        out.push(format!("vocalized {query}: {}", page(result, false)));
    }
    out
}

#[test]
fn every_search_answers_as_with_the_quote_free_twins() {
    let (twins_dir, plain_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let mut with_twins = build(&twins_dir, true);
    let mut without = build(&plain_dir, false);
    let compare = |with_twins: &SearchEngine, without: &SearchEngine| {
        let (expected, got) = (answers(with_twins), answers(without));
        assert_eq!(expected.len(), got.len());
        for (expected, got) in expected.iter().zip(&got) {
            assert_eq!(got, expected);
        }
        expected
    };
    let expected = compare(&with_twins, &without);
    // The fixture exercises what it is for: quote-free queries reach quoted words.
    let rambam = expected
        .iter()
        .find(|a| a.starts_with("exact רמבם:"))
        .unwrap();
    assert!(rambam.contains("רמב&quot;ם</font>"), "{rambam}");
    for label in ["highlight רמבם:", "fuzzy highlight0 אדמור:"] {
        let painted = expected.iter().find(|a| a.starts_with(label)).unwrap();
        assert!(painted.contains("[("), "{painted}");
    }

    let lexical_dir = TempDir::new().unwrap();
    let path = lexical_db(&lexical_dir);
    assert!(with_twins.set_magic_dictionary_path(path.clone()));
    assert!(without.set_magic_dictionary_path(path));
    compare(&with_twins, &without);
}

#[test]
fn the_index_holds_no_quote_free_twin() {
    let (twins_dir, plain_dir) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let with_twins = build(&twins_dir, true);
    let without = build(&plain_dir, false);
    let terms = |engine: &SearchEngine| {
        let searcher = engine.searcher_for_tests();
        let field = searcher.schema().get_field("text").unwrap();
        let mut terms = Vec::new();
        for reader in searcher.segment_readers() {
            let inverted = reader.inverted_index(field).unwrap();
            let mut stream = inverted.terms().stream().unwrap();
            while stream.advance() {
                terms.push(String::from_utf8(stream.key().to_vec()).unwrap());
            }
        }
        terms.sort();
        terms.dedup();
        terms
    };
    let (twinned, plain) = (terms(&with_twins), terms(&without));
    assert!(twinned.contains(&"רשי".to_string()));
    assert!(
        plain.contains(&"רשי".to_string()),
        "a quote-free word is indexed as written"
    );
    assert!(twinned.contains(&"הרמבם".to_string()));
    assert!(!plain.contains(&"שצ".to_string()), "no twin of ש\"ץ");
    assert!(!plain.contains(&"אדמור".to_string()), "no twin of אדמו\"ר");
    assert!(twinned.contains(&"אדמור".to_string()));
}
