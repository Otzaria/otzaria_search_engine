//! Phrases that continue from one line onto the next (`cross_line`), through the public
//! search API: matching, anchoring, what breaks a seam, counting and display.

use crate::api::search_engine::*;
use crate::cross_line::{self, LineEdges};
use std::collections::HashMap;
use tantivy::tokenizer::TextAnalyzer;
use tempfile::TempDir;

fn engine() -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(dir.path().to_str().unwrap());
    (engine, dir)
}

fn add_book(engine: &mut SearchEngine, order: u32, path: &str, lines: &[&str]) {
    engine
        .add_text_book(
            format!("ספר {order}"),
            "/t".to_string(),
            path.to_string(),
            order,
            5,
            lines.join("\n"),
            None,
            TextStorage::InIndex,
            None,
        )
        .unwrap();
}

fn book_engine(lines: &[&str]) -> (SearchEngine, TempDir) {
    let (mut e, dir) = engine();
    add_book(&mut e, 0, "/b/0.txt", lines);
    e.commit().unwrap();
    (e, dir)
}

fn exact(e: &SearchEngine, query: &str) -> Vec<SearchResult> {
    exact_ordered(e, query, ResultsOrder::Catalogue)
}

fn exact_ordered(e: &SearchEngine, query: &str, order: ResultsOrder) -> Vec<SearchResult> {
    e.search_exact(query.to_string(), vec![], 100, 0, order, false, false, None)
        .unwrap()
}

fn advanced(e: &SearchEngine, query: &str, distance: u32) -> Vec<SearchResult> {
    advanced_with(e, query, distance, HashMap::new(), "")
}

fn advanced_with(
    e: &SearchEngine,
    query: &str,
    distance: u32,
    options: HashMap<String, HashMap<String, bool>>,
    negative: &str,
) -> Vec<SearchResult> {
    e.search_advanced(
        query.to_string(),
        negative.to_string(),
        vec![],
        100,
        0,
        distance,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        options,
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
        SearchScope::WordDistance,
        None,
        None,
        None,
    )
    .unwrap()
}

fn count_advanced(e: &SearchEngine, query: &str, distance: u32) -> u32 {
    e.count_advanced(
        query.to_string(),
        String::new(),
        vec![],
        distance,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        false,
        false,
        SearchScope::WordDistance,
        SearchScope::WordDistance,
        None,
        None,
    )
    .unwrap()
}

/// `(segment, continues_to_next_line)` of each hit.
fn hits(results: &[SearchResult]) -> Vec<(u64, bool)> {
    results
        .iter()
        .map(|r| (r.segment, r.continues_to_next_line))
        .collect()
}

fn analyzer(e: &SearchEngine) -> TextAnalyzer {
    e.text_analyzer_for_tests()
}

#[test]
fn a_phrase_continues_onto_the_next_line() {
    let (e, _dir) = book_engine(&[
        "ויבדל בין המים אשר מתחת לרקיע ובין המים",
        "ויאמר אלהים יקוו המים",
    ]);
    let results = exact(&e, "ובין המים ויאמר אלהים");
    assert_eq!(hits(&results), [(0, true)]);
    let text = &results[0].text;
    assert_eq!(text.matches("<br>").count(), 1, "{text}");
    assert!(!text.contains('\n'), "{text}");
    for word in ["ובין", "המים", "ויאמר", "אלהים"] {
        assert!(
            text.contains(&format!("<font color=red>{word}</font>")),
            "{word} not painted: {text}"
        );
    }
    // The line break sits between the painted words of the two lines.
    assert!(text.find("ויאמר").unwrap() > text.find("<br>").unwrap());
    assert!(text.find("ובין").unwrap() < text.find("<br>").unwrap());
    assert_eq!(
        e.count_exact("ובין המים ויאמר אלהים".into(), vec![], false, false)
            .unwrap(),
        1
    );
}

#[test]
fn every_split_of_a_long_phrase_is_found() {
    let first = "אחת שתים שלש ארבע חמש שש";
    let second = "שבע שמונה תשע עשר אחת עשרה";
    let (e, _dir) = book_engine(&[first, second]);
    let words: Vec<&str> = first.split(' ').chain(second.split(' ')).collect();
    for split in 1..words.len() - 1 {
        for len in [2, 4, 8, words.len()] {
            let start = split.saturating_sub(len / 2);
            let end = (start + len).min(words.len());
            if start >= 6 || end <= 6 {
                continue;
            }
            let phrase = words[start..end].join(" ");
            assert_eq!(hits(&exact(&e, &phrase)), [(0, true)], "{phrase}");
        }
    }
    // Twelve words, six on each side: no window limit.
    assert_eq!(hits(&exact(&e, &words.join(" "))), [(0, true)]);
}

#[test]
fn numbering_markers_are_skipped_at_both_edges() {
    let (e, _dir) = book_engine(&[
        "(א) ויהי ערב ויהי בקר יום אחד {פ}",
        "(ב) ויאמר אלהים יהי רקיע",
        "[ג] וירא אלהים כי טוב:",
        "{ד} ויקרא אלהים לרקיע שמים",
    ]);
    assert_eq!(hits(&exact(&e, "יום אחד ויאמר אלהים")), [(0, true)]);
    assert_eq!(hits(&exact(&e, "יהי רקיע וירא")), [(1, true)]);
    assert_eq!(hits(&exact(&e, "כי טוב ויקרא")), [(2, true)]);
    // A marker is not a word of the text: the phrase may not run through it.
    assert!(exact(&e, "אחד פ ויאמר").is_empty());
    assert!(exact(&e, "אחד ב ויאמר").is_empty());
}

#[test]
fn headings_and_empty_lines_break_a_phrase() {
    let (e, _dir) = book_engine(&[
        "בראשית ברא אלהים",
        "<h2>פרק ב</h2>",
        "ויכלו השמים",
        "",
        "והארץ היתה תהו",
        "<h3>הלכה</h3>",
    ]);
    assert!(exact(&e, "אלהים ויכלו").is_empty());
    assert!(exact(&e, "אלהים פרק").is_empty());
    assert!(exact(&e, "ב ויכלו").is_empty());
    assert!(exact(&e, "השמים והארץ").is_empty());
    assert!(exact(&e, "תהו הלכה").is_empty());
}

#[test]
fn a_phrase_never_continues_into_another_book() {
    let (mut e, _dir) = engine();
    add_book(&mut e, 0, "/b/0.txt", &["סוף הספר הראשון"]);
    add_book(&mut e, 1, "/b/1.txt", &["תחילת הספר השני"]);
    add_book(
        &mut e,
        2,
        "/b/2.txt",
        &["עוד שורה ובה הראשון", "תחילת השורה"],
    );
    e.commit().unwrap();
    let results = exact(&e, "הראשון תחילת");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file_path, "/b/2.txt");
    assert!(results[0].continues_to_next_line);
}

#[test]
fn a_hit_in_line_and_across_the_break_counts_once() {
    let (e, _dir) = book_engine(&["אמר רבי יוחנן דבר אמר", "רבי יוחנן אמר", "שורה אחרת"]);
    let results = exact(&e, "אמר רבי");
    // Line 0 holds the phrase in line and across the break; it is one hit, shown in line.
    assert_eq!(hits(&results), [(0, false)]);
    assert_eq!(
        e.count_exact("אמר רבי".into(), vec![], false, false)
            .unwrap(),
        1
    );
    let by_book = e
        .count_by_book_exact("אמר רבי".into(), vec![], false, false)
        .unwrap();
    assert_eq!(by_book.values().sum::<u32>(), 1);
    let page = e
        .search_and_count_exact(
            "אמר רבי".into(),
            vec![],
            10,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
        )
        .unwrap();
    assert_eq!(page.total_count, 1);
    let grouped = e
        .search_exact(
            "דבר אמר רבי".into(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            Some(ResultGrouping::SameSection),
        )
        .unwrap();
    assert_eq!(hits(&grouped), [(0, true)]);
    assert_eq!(grouped[0].merged_count, 1);
}

#[test]
fn in_line_hits_rank_above_cross_line_hits() {
    let (mut e, _dir) = engine();
    let mut lines = vec!["שורה ובה קריאת שמע ערבית"; 3];
    lines.push("ואחר כך קריאת");
    lines.push("שמע של שחרית");
    lines.extend(["מילים אחרות לגמרי בשורה"; 40]);
    add_book(&mut e, 0, "/b/0.txt", &lines);
    e.commit().unwrap();
    let results = exact_ordered(&e, "קריאת שמע", ResultsOrder::Relevance);
    assert_eq!(results.len(), 4);
    assert!(results[..3].iter().all(|r| !r.continues_to_next_line));
    assert_eq!(
        (results[3].segment, results[3].continues_to_next_line),
        (3, true)
    );
}

#[test]
fn word_distance_counts_the_words_of_both_lines() {
    let (e, _dir) = book_engine(&["השמים ואת הארץ", "והארץ היתה תהו ובהו"]);
    // Between השמים and היתה: ואת הארץ | והארץ — three words.
    assert!(advanced(&e, "השמים היתה", 2).is_empty());
    assert_eq!(hits(&advanced(&e, "השמים היתה", 3)), [(0, true)]);
    assert_eq!(count_advanced(&e, "השמים היתה", 3), 1);
    assert_eq!(hits(&advanced(&e, "הארץ היתה ובהו", 1)), [(0, true)]);
    assert_eq!(hits(&advanced(&e, "ואת הארץ והארץ היתה", 0)), [(0, true)]);
    // Order still matters across the break.
    assert!(advanced(&e, "היתה השמים", 5).is_empty());
}

#[test]
fn advanced_word_options_apply_across_the_break() {
    let (e, _dir) = book_engine(&["ראה כי טוב השמים", "והארץ היתה"]);
    let prefixes: HashMap<String, HashMap<String, bool>> = [(
        "ארץ_1".to_string(),
        [("קידומות".to_string(), true)].into_iter().collect(),
    )]
    .into_iter()
    .collect();
    assert!(advanced(&e, "השמים ארץ", 0).is_empty());
    assert_eq!(
        hits(&advanced_with(&e, "השמים ארץ", 0, prefixes, "")),
        [(0, true)]
    );
}

#[test]
fn a_negative_phrase_stays_inside_its_line() {
    let (e, _dir) = book_engine(&["שלום עליכם מלאכי", "השרת שלום"]);
    // The negative phrase only crosses the break: it does not exclude line 0.
    let results = advanced_with(&e, "שלום", 0, HashMap::new(), "מלאכי השרת");
    assert_eq!(results.len(), 2);
}

#[test]
fn vocalized_searches_stay_inside_their_line() {
    let (e, _dir) = book_engine(&["בָּרָא אֱלֹהִים אֵת", "הַשָּׁמַיִם וְאֵת"]);
    assert_eq!(hits(&exact(&e, "את השמים")), [(0, true)]);
    let vocalized = e
        .search_exact(
            "אֵת הַשָּׁמַיִם".to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Catalogue,
            true,
            false,
            None,
        )
        .unwrap();
    assert!(vocalized.is_empty());
}

#[test]
fn pdf_lines_join_across_pages_but_not_across_a_dropped_line() {
    let (mut e, _dir) = engine();
    let pages = vec![
        PdfPageInput {
            reference: "עמוד 1".into(),
            text: "שורה ראשונה בעמוד\nוזה סוף העמוד הראשון".into(),
            page_index: 0,
        },
        PdfPageInput {
            reference: "עמוד 2".into(),
            text: "המשך בעמוד השני\n!@#$%^&*()\nאחרי הזבל באה שורה".into(),
            page_index: 1,
        },
    ];
    let added = e
        .add_pdf_book(
            "ספר".into(),
            "/t".into(),
            "/b/book.pdf".into(),
            3,
            5,
            pages,
            None,
        )
        .unwrap();
    assert_eq!(added, 4);
    e.commit().unwrap();
    let results = exact(&e, "הראשון המשך");
    assert_eq!(results.len(), 1);
    assert!(results[0].is_pdf && results[0].continues_to_next_line);
    assert_eq!(results[0].segment, 0);
    assert!(exact(&e, "השני אחרי").is_empty());
}

#[test]
fn a_reindexed_book_joins_its_new_lines() {
    let (mut e, _dir) = engine();
    add_book(&mut e, 0, "/b/0.txt", &["קול דודי", "דופק פתחי"]);
    add_book(&mut e, 1, "/b/1.txt", &["קול דודי דופק"]);
    e.commit().unwrap();
    assert_eq!(exact(&e, "דודי דופק").len(), 2);
    e.delete_documents_by_file_path("/b/0.txt").unwrap();
    add_book(
        &mut e,
        0,
        "/b/0.txt",
        &["קול דודי", "<h2>הפסקה</h2>", "דופק פתחי"],
    );
    e.commit().unwrap();
    let results = exact(&e, "דודי דופק");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file_path, "/b/1.txt");
}

#[test]
fn line_edges_agree_with_the_indexing_analyzer() {
    let (e, _dir) = engine();
    let mut analyzer = analyzer(&e);
    let cases = [
        ("(ג) ויאמר אלהים {פ}", Some((1, 2))),
        ("ויאמר אלהים", Some((0, 1))),
        ("[יא] רמב\"ם אמר (א)", Some((1, 2))),
        ("הארץ (הוצא) [היצא]", Some((0, 1))),
        ("(א)", None),
        ("", None),
        ("  ...  ", None),
    ];
    for (line, expected) in cases {
        let edges: Option<LineEdges> = cross_line::line_edges(&mut analyzer, line);
        assert_eq!(
            edges.as_ref().map(|e| (e.first, e.last)),
            expected,
            "{line:?}"
        );
    }
    // Random text: the positions match what the analyzer emits for the same words.
    let alphabet: Vec<char> = "אבגדהו זחט'\"()[]{}.,: -־abc1".chars().collect();
    let mut seed = 0x1703u64;
    for _ in 0..3000 {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        let len = (seed >> 40) as usize % 40;
        let line: String = (0..len)
            .map(|_| {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                alphabet[(seed >> 33) as usize % alphabet.len()]
            })
            .collect();
        let content = cross_line::content_range(&line);
        let mut positions = Vec::new();
        {
            let mut stream = analyzer.token_stream(&line);
            while stream.advance() {
                let t = stream.token();
                if t.offset_from >= content.start && t.offset_to <= content.end {
                    positions.push(t.position as u32);
                }
            }
        }
        let expected = positions
            .first()
            .map(|&first| (first, *positions.iter().max().unwrap()));
        let edges = cross_line::line_edges(&mut analyzer, &line);
        assert_eq!(edges.map(|e| (e.first, e.last)), expected, "{line:?}");
    }
}

/// What phrases across line breaks cost on the real library: index space and
/// query time, with and without the cross-line part. Indexes every
/// `OTZARIA_XLINE_STEP`-th book of `OTZARIA_SEFORIM_DB` (default: all) into
/// `OTZARIA_XLINE_DIR`; `OTZARIA_XLINE_REUSE` keeps a built index.
#[test]
#[ignore]
fn real_library_cross_line_cost() {
    use rusqlite::{Connection, OpenFlags};
    use std::time::Instant;
    use tantivy::collector::{Count, TopDocs};
    use tantivy::Order;

    let Ok(db) = std::env::var("OTZARIA_SEFORIM_DB") else {
        return;
    };
    let step: usize = std::env::var("OTZARIA_XLINE_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let dir = std::path::PathBuf::from(std::env::var("OTZARIA_XLINE_DIR").unwrap_or_else(|_| {
        std::env::temp_dir()
            .join("otzaria_xline")
            .display()
            .to_string()
    }));
    let reuse = std::env::var("OTZARIA_XLINE_REUSE").is_ok()
        && check_index_compatibility(dir.display().to_string()).compatible;
    let mut engine;
    if reuse {
        engine = SearchEngine::new(dir.to_str().unwrap());
    } else {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        engine = SearchEngine::new(dir.to_str().unwrap());
        let conn = Connection::open_with_flags(
            format!("file:{}?mode=ro", db.replace('\\', "/")),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        conn.execute_batch("PRAGMA query_only=1").unwrap();
        let books: Vec<i64> = conn
            .prepare("SELECT DISTINCT bookId FROM line ORDER BY bookId")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .step_by(step)
            .collect();
        let started = Instant::now();
        let mut lines = 0usize;
        for (order, book) in books.iter().enumerate() {
            let rows: Vec<String> = conn
                .prepare_cached(
                    "SELECT CAST(lc.content AS TEXT) FROM line l LEFT JOIN line_content lc \
                     ON lc.id = l.id WHERE l.bookId = ?1 ORDER BY l.lineIndex, l.id",
                )
                .unwrap()
                .query_map([book], |r| r.get::<_, Option<String>>(0))
                .unwrap()
                .map(|r| r.unwrap().unwrap_or_default().replace('\n', " "))
                .collect();
            lines += rows.len();
            engine
                .add_text_book(
                    format!("ספר {book}"),
                    "/x".into(),
                    format!("/x/{book}"),
                    order as u32,
                    5,
                    rows.join("\n"),
                    None,
                    TextStorage::InIndex,
                    None,
                )
                .unwrap();
        }
        engine.commit().unwrap();
        let indexed = started.elapsed();
        engine.optimize().unwrap();
        println!(
            "indexed {} books, {lines} lines in {:.1}s (+ optimize {:.1}s)",
            books.len(),
            indexed.as_secs_f64(),
            started.elapsed().as_secs_f64() - indexed.as_secs_f64()
        );
    }
    let searcher = engine.searcher_for_tests();
    let usage = searcher.space_usage().unwrap();
    let mut by_field: HashMap<String, u64> = HashMap::new();
    for segment in usage.segments() {
        for per_field in [
            segment.termdict(),
            segment.postings(),
            segment.positions(),
            segment.fast_fields(),
            segment.fieldnorms(),
        ] {
            for field in per_field.fields() {
                *by_field.entry(field.field_name().to_string()).or_default() +=
                    field.total().get_bytes();
            }
        }
    }
    let mb = |name: &str| by_field.get(name).copied().unwrap_or(0) as f64 / 1e6;
    println!(
        "docs {}, segments {}, index {:.1} MB; text {:.1} MB; lineEdge {:.1} MB, lineFirst {:.1} MB, lineLast {:.1} MB",
        searcher.num_docs(),
        searcher.segment_readers().len(),
        usage.total().get_bytes() as f64 / 1e6,
        mb("text"),
        mb(cross_line::LINE_EDGE_FIELD),
        mb(cross_line::LINE_FIRST_FIELD),
        mb(cross_line::LINE_LAST_FIELD),
    );

    let best_ms = |f: &dyn Fn() -> usize| {
        let mut best = f64::MAX;
        let mut value = 0;
        for _ in 0..5 {
            let t = Instant::now();
            value = f();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        (value, best)
    };
    let phrases: &[(&str, Option<u32>)] = &[
        ("המים ויאמר", None),
        ("יום אחד ויאמר", None),
        ("ואת הארץ והארץ היתה", None),
        ("אמר רבי", None),
        ("אמר לו", None),
        ("של תורה", None),
        ("את הארץ", None),
        ("כי לא", None),
        ("על פי", None),
        ("רבי יהודה אומר", None),
        ("אמר רב יהודה אמר שמואל", None),
        ("וכו' אמר", None),
        ("ע\"ש ועיין", None),
        ("אמר אמר", None),
        ("ויהי ערב ויהי בקר יום", None),
        ("אמר רבי", Some(2)),
        ("כי לא", Some(2)),
        ("השמים היתה", Some(3)),
        ("רבי יהודה אומר", Some(1)),
    ];
    println!("phrase | distance | in-line hits, count ms, page ms | with cross-line hits, count ms, page ms | API page ms");
    for &(phrase, distance) in phrases {
        let inline = engine.phrase_query_for_tests(phrase, distance, false);
        let union = engine.phrase_query_for_tests(phrase, distance, true);
        let page = |q: &dyn tantivy::query::Query| {
            let top = TopDocs::with_limit(100).order_by_fast_field::<u64>("id", Order::Asc);
            searcher.search(q, &(top, Count)).unwrap().1
        };
        let (c_in, t_in) = best_ms(&|| searcher.search(inline.as_ref(), &Count).unwrap());
        let (_, p_in) = best_ms(&|| page(inline.as_ref()));
        let (c_un, t_un) = best_ms(&|| searcher.search(union.as_ref(), &Count).unwrap());
        let (_, p_un) = best_ms(&|| page(union.as_ref()));
        let (_, api) = best_ms(&|| match distance {
            None => engine
                .search_and_count_exact(
                    phrase.into(),
                    vec![],
                    100,
                    0,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    None,
                )
                .unwrap()
                .results
                .len(),
            Some(d) => engine
                .search_and_count_advanced(
                    phrase.into(),
                    String::new(),
                    vec![],
                    100,
                    0,
                    d,
                    0,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    SearchScope::WordDistance,
                    SearchScope::WordDistance,
                    None,
                    None,
                    None,
                )
                .unwrap()
                .results
                .len(),
        });
        println!(
            "{phrase} | {distance:?} | {c_in} {t_in:.1} {p_in:.1} | {c_un} (+{}) {t_un:.1} {p_un:.1} | {api:.1}",
            c_un - c_in
        );
    }
}
