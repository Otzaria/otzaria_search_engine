//! A PDF page is one document (#5237): its layout lines are where the page wrapped, so
//! every search mode matches words spread over them, and a hit is the page.

use crate::api::search_engine::*;
use std::collections::HashMap;
use std::time::Instant;
use tempfile::TempDir;

fn pdf_engine(pages: &[&[&str]]) -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut e = SearchEngine::new(dir.path().to_str().unwrap());
    add_pdf(&mut e, pages);
    e.commit().unwrap();
    (e, dir)
}

fn add_pdf(e: &mut SearchEngine, pages: &[&[&str]]) -> u32 {
    let pages = pages
        .iter()
        .enumerate()
        .map(|(i, lines)| PdfPageInput {
            reference: format!("עמוד {}", i + 1),
            text: lines.join("\n"),
            page_index: i as u32,
        })
        .collect();
    e.add_pdf_book_placed(
        "ספר".into(),
        "/t".into(),
        "/b/book.pdf".into(),
        0,
        5,
        pages,
        None,
    )
    .unwrap()
}

fn text_engine(lines: &[&str]) -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut e = SearchEngine::new(dir.path().to_str().unwrap());
    e.add_text_book_placed(
        "ספר".into(),
        "/t".into(),
        "/b/0.txt".into(),
        0,
        5,
        lines.join("\n"),
        None,
        TextStorage::InIndex,
        None,
    )
    .unwrap();
    e.commit().unwrap();
    (e, dir)
}

#[derive(Clone, Copy)]
enum Mode {
    All,
    AtLeast,
    Most,
}

impl Search {
    fn scope(&self) -> SearchScope {
        if self.same_paragraph {
            SearchScope::SameParagraph
        } else {
            SearchScope::WordDistance
        }
    }

    fn mode(&self) -> Option<WordMatchMode> {
        match self.mode {
            Mode::All => None,
            Mode::AtLeast => Some(WordMatchMode::AtLeast),
            Mode::Most => Some(WordMatchMode::MostWords),
        }
    }
}

#[derive(Clone)]
struct Search {
    distance: u32,
    any_order: bool,
    same_paragraph: bool,
    mode: Mode,
    count: Option<u32>,
    options: HashMap<String, HashMap<String, bool>>,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            distance: 0,
            any_order: false,
            same_paragraph: false,
            mode: Mode::All,
            count: None,
            options: HashMap::new(),
        }
    }
}

fn distance(distance: u32) -> Search {
    Search {
        distance,
        ..Search::default()
    }
}

fn any_order(distance: u32) -> Search {
    Search {
        distance,
        any_order: true,
        ..Search::default()
    }
}

fn with_options(mut search: Search, words: &[(&str, usize, &[&str])]) -> Search {
    for &(word, index, options) in words {
        search.options.insert(
            format!("{word}_{index}"),
            options.iter().map(|o| (o.to_string(), true)).collect(),
        );
    }
    search
}

fn advanced(e: &SearchEngine, query: &str, s: &Search) -> Vec<SearchResult> {
    e.search_advanced(
        query.to_string(),
        String::new(),
        vec![],
        100,
        0,
        s.distance,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        s.options.clone(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        s.scope(),
        SearchScope::WordDistance,
        None,
        s.mode(),
        s.count,
        Some(s.any_order),
        None,
    )
    .unwrap()
}

fn count(e: &SearchEngine, query: &str, s: &Search) -> u32 {
    e.count_advanced(
        query.to_string(),
        String::new(),
        vec![],
        s.distance,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        s.options.clone(),
        HashMap::new(),
        false,
        false,
        s.scope(),
        SearchScope::WordDistance,
        s.mode(),
        s.count,
        Some(s.any_order),
        None,
    )
    .unwrap()
}

fn exact(e: &SearchEngine, query: &str) -> Vec<SearchResult> {
    e.search_exact(
        query.to_string(),
        vec![],
        100,
        0,
        ResultsOrder::Catalogue,
        false,
        false,
        None,
        None,
    )
    .unwrap()
}

/// `(page, continues_to_next_line)` of each hit.
fn pages(results: &[SearchResult]) -> Vec<(u64, bool)> {
    results
        .iter()
        .map(|r| (r.segment, r.continues_to_next_line))
        .collect()
}

fn assert_painted(result: &SearchResult, words: &[&str]) {
    let text = &result.text;
    assert!(!text.contains('\n'), "{text}");
    for word in words {
        assert!(
            text.contains(&format!("<font color=red>{word}</font>")),
            "{word} not painted: {text}"
        );
    }
}

const PAGE: &[&str] = &[
    "ויאמר משה אל העם",
    "אל תיראו התיצבו",
    "וראו את ישועת",
    "יהוה אשר יעשה לכם היום",
    "כי אשר ראיתם את",
    "מצרים היום לא תספו",
];

#[test]
fn a_page_is_one_document() {
    let (mut e, _dir) = {
        let dir = TempDir::new().unwrap();
        let e = SearchEngine::new(dir.path().to_str().unwrap());
        (e, dir)
    };
    assert_eq!(add_pdf(&mut e, &[PAGE, &["!@#$%^&*()"], &["סוף"]]), 2);
    e.commit().unwrap();
    let stamps = e.get_book_stamps().unwrap();
    assert_eq!(stamps.len(), 1);
    // A PDF an older engine indexed line by line has none.
    assert_ne!(stamps[0].text_hash, 0);
    let results = advanced(&e, "סוף", &Search::default());
    assert_eq!(pages(&results), [(2, false)]);
    assert_eq!(results[0].reference, "עמוד 3");
}

#[test]
fn any_order_matches_across_the_lines_of_a_page() {
    let (e, _dir) = pdf_engine(&[&["כתוב בראש"], PAGE]);
    let results = advanced(&e, "תיראו העם", &any_order(2));
    assert_eq!(pages(&results), [(1, false)]);
    assert_painted(&results[0], &["העם", "תיראו"]);
    // משה (line 1) .. יהוה (line 4): eight words between.
    assert_eq!(
        pages(&advanced(&e, "יהוה משה", &any_order(8))),
        [(1, false)]
    );
    assert!(advanced(&e, "יהוה משה", &any_order(7)).is_empty());
    assert_eq!(count(&e, "יהוה משה", &any_order(8)), 1);
}

#[test]
fn word_distance_matches_across_the_lines_of_a_page() {
    let (e, _dir) = pdf_engine(&[PAGE]);
    // העם (line 1) .. וראו (line 3): three words between.
    let results = advanced(&e, "העם וראו", &distance(3));
    assert_eq!(pages(&results), [(0, false)]);
    assert_painted(&results[0], &["העם", "וראו"]);
    assert!(advanced(&e, "העם וראו", &distance(2)).is_empty());
    assert!(advanced(&e, "וראו העם", &distance(3)).is_empty());
}

#[test]
fn an_exact_phrase_matches_across_the_lines_of_a_page() {
    let (e, _dir) = pdf_engine(&[&["שמע ישראל", "יהוה", "אלהינו יהוה אחד"]]);
    let results = exact(&e, "ישראל יהוה אלהינו");
    assert_eq!(pages(&results), [(0, false)]);
    assert_painted(&results[0], &["ישראל", "יהוה", "אלהינו"]);
}

#[test]
fn the_reported_search_matches_across_several_lines() {
    // Three words, prefixes and suffixes, distance 30 (#5237).
    let (e, _dir) = pdf_engine(&[&["שורה אחרת"], PAGE, &["עוד עמוד"]]);
    let search = with_options(
        distance(30),
        &[
            ("ראו", 0, &["קידומות"]),
            ("ישוע", 1, &["סיומות"]),
            ("מצרי", 2, &["סיומות"]),
        ],
    );
    let results = advanced(&e, "ראו ישוע מצרי", &search);
    assert_eq!(pages(&results), [(1, false)]);
    // With its prefixes, ראו is also the ראו of תיראו, the first occurrence on the page.
    assert_painted(&results[0], &["תיראו", "ישועת", "מצרים"]);
    assert_eq!(count(&e, "ראו ישוע מצרי", &search), 1);
    // Options are keyed by the word's place in the query.
    let any = with_options(
        any_order(30),
        &[
            ("מצרי", 0, &["סיומות"]),
            ("ראו", 1, &["קידומות"]),
            ("ישוע", 2, &["סיומות"]),
        ],
    );
    assert_eq!(pages(&advanced(&e, "מצרי ראו ישוע", &any)), [(1, false)]);
    // Without the options the words are not these.
    assert!(advanced(&e, "ראו ישוע מצרי", &distance(30)).is_empty());
}

#[test]
fn full_and_defective_spelling_match_across_lines() {
    let (e, _dir) = pdf_engine(&[PAGE]);
    let search = with_options(distance(5), &[("התייצבו", 1, &["כתיב מלא/חסר"])]);
    let results = advanced(&e, "משה התייצבו", &search);
    assert_eq!(pages(&results), [(0, false)]);
    assert_painted(&results[0], &["משה", "התיצבו"]);
}

#[test]
fn same_paragraph_is_the_page() {
    let (e, _dir) = pdf_engine(&[PAGE, &["משה לבדו"]]);
    let search = Search {
        same_paragraph: true,
        ..Search::default()
    };
    assert_eq!(pages(&advanced(&e, "מצרים משה", &search)), [(0, false)]);
    assert_eq!(count(&e, "מצרים משה", &search), 1);
    assert!(advanced(&e, "מצרים לבדו", &search).is_empty());
}

#[test]
fn at_least_n_and_most_words_count_the_whole_page() {
    let (e, _dir) = pdf_engine(&[PAGE, &["משה לבדו"]]);
    let at_least = |n| Search {
        mode: Mode::AtLeast,
        count: Some(n),
        ..distance(0)
    };
    // One word on line 1, one on line 6, the third nowhere.
    assert_eq!(
        pages(&advanced(&e, "משה מצרים גולית", &at_least(2))),
        [(0, false)]
    );
    assert!(advanced(&e, "משה מצרים גולית", &at_least(3)).is_empty());
    let most = Search {
        mode: Mode::Most,
        ..distance(0)
    };
    assert_eq!(pages(&advanced(&e, "משה מצרים גולית", &most)), [(0, false)]);
}

#[test]
fn one_result_per_page() {
    let (e, _dir) = pdf_engine(&[&["העם אל", "תיראו העם תיראו"], &["העם תיראו"]]);
    let search = any_order(1);
    assert_eq!(
        pages(&advanced(&e, "תיראו העם", &search)),
        [(0, false), (1, false)]
    );
    assert_eq!(count(&e, "תיראו העם", &search), 2);
}

#[test]
fn pages_join_only_through_their_edges() {
    let (e, _dir) = pdf_engine(&[&["ויאמר משה", "אל העם"], &["אל תיראו"], &["התיצבו"]]);
    // An ordered phrase continues onto the next page, attributed to the first.
    let results = exact(&e, "העם אל תיראו");
    assert_eq!(pages(&results), [(0, true)]);
    assert!(results[0].text.contains("<br>"), "{}", results[0].text);
    // Any order stays within a page; a phrase crosses one page edge at most.
    assert!(advanced(&e, "תיראו העם", &any_order(3)).is_empty());
    assert!(exact(&e, "העם אל תיראו התיצבו").is_empty());
}

#[test]
fn a_page_without_text_breaks_the_join() {
    let (e, _dir) = pdf_engine(&[&["סוף העמוד"], &["!@#$%^&*()"], &["המשך כאן"]]);
    assert!(exact(&e, "העמוד המשך").is_empty());
}

#[test]
fn text_books_keep_their_lines() {
    let (e, _dir) = text_engine(PAGE);
    assert!(advanced(&e, "תיראו העם", &any_order(2)).is_empty());
    assert!(advanced(&e, "העם וראו", &distance(3)).is_empty());
    let same_paragraph = Search {
        same_paragraph: true,
        ..Search::default()
    };
    assert!(advanced(&e, "מצרים משה", &same_paragraph).is_empty());
}

/// `cargo test --release --lib pdf_page_tests::bench -- --ignored --nocapture`
#[test]
#[ignore = "benchmark"]
fn bench_searches_over_100k_pdf_lines() {
    let words = [
        "ויאמר",
        "משה",
        "אל",
        "העם",
        "תיראו",
        "התיצבו",
        "וראו",
        "את",
        "ישועת",
        "יהוה",
        "אשר",
        "יעשה",
        "לכם",
        "היום",
        "כי",
        "ראיתם",
        "מצרים",
        "לא",
        "תספו",
        "לראתם",
        "עוד",
        "עד",
        "עולם",
        "ילחם",
        "ואתם",
        "תחרשון",
        "ויסע",
        "מלאך",
        "האלהים",
        "ההלך",
    ];
    let dir = TempDir::new().unwrap();
    let mut e = SearchEngine::new(dir.path().to_str().unwrap());
    let mut seed = 7u64;
    let mut next = || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (seed >> 33) as usize
    };
    let (books, pages_per_book, lines_per_page) = (25, 100, 40);
    let started = Instant::now();
    for book in 0..books {
        let pages: Vec<PdfPageInput> = (0..pages_per_book)
            .map(|page| PdfPageInput {
                reference: format!("עמוד {}", page + 1),
                text: (0..lines_per_page)
                    .map(|_| {
                        (0..5)
                            .map(|_| words[next() % words.len()])
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                page_index: page as u32,
            })
            .collect();
        e.add_pdf_book_placed(
            format!("ספר {book}"),
            "/t".into(),
            format!("/b/{book}.pdf"),
            book as u32,
            5,
            pages,
            None,
        )
        .unwrap();
    }
    e.commit().unwrap();
    let index_time = started.elapsed();
    fn dir_size(path: &std::path::Path) -> u64 {
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| match entry.metadata() {
                Ok(m) if m.is_dir() => dir_size(&entry.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    }
    let size = dir_size(dir.path());
    println!(
        "indexed {} lines in {index_time:?}, index {:.1} MB",
        books * pages_per_book * lines_per_page,
        size as f64 / 1e6
    );
    let cases: [(&str, Search); 5] = [
        ("משה ישועת", distance(30)),
        ("ישועת משה", any_order(10)),
        ("את אשר", any_order(30)),
        ("משה מצרים ישועת", distance(30)),
        (
            "מצרים משה",
            Search {
                same_paragraph: true,
                ..Search::default()
            },
        ),
    ];
    for (query, search) in cases {
        let _ = count(&e, query, &search);
        let runs = 5;
        let started = Instant::now();
        let mut hits = 0;
        for _ in 0..runs {
            hits = count(&e, query, &search);
            let _ = advanced(&e, query, &search);
        }
        println!(
            "{query:?} any_order={} distance={}: {hits} hits, {:?} per count+search",
            search.any_order,
            search.distance,
            started.elapsed() / runs
        );
    }
}

#[test]
fn a_long_page_s_snippet_shows_every_matched_word() {
    let mut lines = vec!["ויאמר משה אל העם"];
    lines.extend(std::iter::repeat_n("שורה של מילוי בלבד בעמוד הזה", 58));
    lines.push("וראו את ישועת יהוה");
    let (e, _dir) = pdf_engine(&[&lines]);
    let same_paragraph = Search {
        same_paragraph: true,
        ..Search::default()
    };
    let results = advanced(&e, "משה ישועת", &same_paragraph);
    assert_eq!(pages(&results), [(0, false)]);
    assert_painted(&results[0], &["משה", "ישועת"]);
    assert!(results[0].text.contains(" … "), "{}", results[0].text);
    // So does an ordered match whose words are far apart.
    let results = advanced(&e, "משה ישועת", &distance(400));
    assert_eq!(pages(&results), [(0, false)]);
    assert_painted(&results[0], &["משה", "ישועת"]);
}

#[test]
fn the_last_page_index_is_followed_by_no_page() {
    let dir = TempDir::new().unwrap();
    let mut e = SearchEngine::new(dir.path().to_str().unwrap());
    let input = vec![
        PdfPageInput {
            reference: "אחרון".into(),
            text: "סוף העמוד".into(),
            page_index: u32::MAX,
        },
        PdfPageInput {
            reference: "ראשון".into(),
            text: "המשך כאן".into(),
            page_index: 0,
        },
    ];
    assert_eq!(
        e.add_pdf_book_placed(
            "ספר".into(),
            "/t".into(),
            "/b/x.pdf".into(),
            0,
            5,
            input,
            None
        )
        .unwrap(),
        2
    );
    e.commit().unwrap();
    assert!(exact(&e, "העמוד המשך").is_empty());
    assert_eq!(
        pages(&exact(&e, "סוף העמוד")),
        [(u64::from(u32::MAX), false)]
    );
    assert_eq!(pages(&exact(&e, "המשך כאן")), [(0, false)]);
}

#[test]
fn a_pdf_indexed_by_lines_beside_one_indexed_by_pages() {
    let dir = TempDir::new().unwrap();
    let mut e = SearchEngine::new(dir.path().to_str().unwrap());
    // As an older engine wrote a PDF: one document per layout line, no `textHash`.
    for (ordinal, line) in ["ויאמר משה אל העם", "אל תיראו התיצבו"].iter().enumerate()
    {
        e.add_document(
            ordinal as u32,
            "ישן",
            "עמוד 1",
            "/t",
            line,
            0,
            true,
            "/b/old.pdf",
            None,
            None,
        )
        .unwrap();
    }
    add_pdf(&mut e, &[PAGE]);
    e.commit().unwrap();
    let stamps: HashMap<String, u64> = e
        .get_book_stamps()
        .unwrap()
        .into_iter()
        .map(|s| (s.file_path, s.text_hash))
        .collect();
    assert_eq!(stamps["/b/old.pdf"], 0);
    assert_ne!(stamps["/b/book.pdf"], 0);
    let found = |results: Vec<SearchResult>| {
        let mut paths: Vec<String> = results.into_iter().map(|r| r.file_path).collect();
        paths.sort();
        paths
    };
    // Both books, each in its own way: the old one by its lines.
    assert_eq!(
        found(advanced(&e, "משה העם", &distance(1))),
        ["/b/book.pdf", "/b/old.pdf"]
    );
    assert_eq!(
        found(advanced(&e, "תיראו העם", &any_order(2))),
        ["/b/book.pdf"]
    );
}

#[test]
fn a_page_of_one_long_line_shows_every_matched_word() {
    // An OCR page without layout lines.
    let filler = "שורה של מילוי בלבד בעמוד הזה ".repeat(60);
    let page = format!("ויאמר משה אל העם {filler}וראו את ישועת יהוה");
    let (e, _dir) = pdf_engine(&[&[page.as_str()]]);
    let same_paragraph = Search {
        same_paragraph: true,
        ..Search::default()
    };
    let results = advanced(&e, "משה ישועת", &same_paragraph);
    assert_eq!(pages(&results), [(0, false)]);
    assert_painted(&results[0], &["משה", "ישועת"]);
    assert!(results[0].text.contains(" … "), "{}", results[0].text);
}
