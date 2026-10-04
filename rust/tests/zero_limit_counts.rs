//! A zero-size page counts matches without materializing results or running highlights.
use search_engine::api::search_engine::{ResultGrouping, ResultsOrder, SearchEngine};
use tempfile::TempDir;

fn order(i: u8) -> ResultsOrder {
    match i {
        0 => ResultsOrder::Relevance,
        1 => ResultsOrder::Catalogue,
        _ => ResultsOrder::Generation,
    }
}

fn fixture(lines: &[&str]) -> (SearchEngine, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let index = dir.path().join("index");
    std::fs::create_dir(&index).unwrap();
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    for (i, text) in lines.iter().enumerate() {
        engine
            .add_document(
                i as u64 + 1,
                "QA",
                "ref",
                "/qa",
                text,
                i as u64,
                false,
                "/qa/book",
                Some((i / 2) as u64),
                Some(i as u32),
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    (engine, dir)
}

#[test]
fn zero_size_pages_preserve_counts_for_all_orders_groups_and_offsets() {
    let (engine, _dir) = fixture(&[
        "בית שָׁמַע ישראל וכל דברי התורה אשר נאמרו פה",
        "בית שָׁמַע ישראל וכל דברי התורה אשר נאמרו פה",
        "בית שָׁמַע ישראל וכל דברי התורה אשר נאמרו שם",
        "בית שְׁמַע ישראל וכל דברי התורה אשר נאמרו שם",
    ]);
    for sort in 0..3 {
        for grouping in [
            None,
            Some(ResultGrouping::SameSection),
            Some(ResultGrouping::IdenticalText),
        ] {
            for vocalized in [false, true] {
                for fuzzy in [false, true] {
                    let query = if vocalized {
                        "שָׁמַע ישראל"
                    } else {
                        "שמע ישראל"
                    };
                    let search = |limit, offset| {
                        if fuzzy {
                            engine
                                .search_and_count_fuzzy(
                                    query.into(),
                                    vec!["/qa".into()],
                                    limit,
                                    offset,
                                    0,
                                    order(sort),
                                    vocalized,
                                    false,
                                    grouping,
                                )
                                .unwrap()
                        } else {
                            engine
                                .search_and_count_exact(
                                    query.into(),
                                    vec!["/qa".into()],
                                    limit,
                                    offset,
                                    order(sort),
                                    vocalized,
                                    false,
                                    grouping,
                                )
                                .unwrap()
                        }
                    };
                    let full = search(100, 0);
                    assert_eq!(full.total_count, if vocalized { 3 } else { 4 });
                    for offset in [0, 1, 100, u32::MAX] {
                        let count = search(0, offset);
                        assert!(count.results.is_empty());
                        assert_eq!(count.total_count, full.total_count);
                        assert_eq!(count.group_count, full.group_count);
                        assert_eq!(count.truncated, full.truncated);
                    }
                }
            }
        }
    }
}

#[test]
fn zero_size_pages_count_empty_and_filtered_queries_without_panicking() {
    let (engine, _dir) = fixture(&["בית שמע ישראל"]);
    for query in ["", "אין התאמה"] {
        let exact = engine
            .search_and_count_exact(
                query.into(),
                vec![],
                0,
                0,
                ResultsOrder::Relevance,
                false,
                false,
                None,
            )
            .unwrap();
        let fuzzy = engine
            .search_and_count_fuzzy(
                query.into(),
                vec![],
                0,
                10,
                0,
                ResultsOrder::Relevance,
                false,
                false,
                None,
            )
            .unwrap();
        for count in [exact, fuzzy] {
            assert_eq!(count.total_count, 0);
            assert!(count.results.is_empty());
            assert!(!count.truncated);
            assert_eq!(count.group_count, None);
        }
    }
    let filtered = engine
        .search_and_count_exact(
            "שמע ישראל".into(),
            vec!["/other".into()],
            0,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            Some(ResultGrouping::SameSection),
        )
        .unwrap();
    assert_eq!(filtered.total_count, 0);
    assert_eq!(filtered.group_count, Some(0));
    assert!(filtered.results.is_empty());
}

#[test]
fn zero_size_pages_preserve_vocalized_expansion_truncation() {
    // One document contains more distinct vocalized terms than either collection cap.
    // Counting it must preserve truncation while skipping its large display text.
    let letters: Vec<char> = "בראשית".chars().collect();
    let marks = [
        "", "\u{05b0}", "\u{05b4}", "\u{05b7}", "\u{05b8}", "\u{05b9}",
    ];
    let words: Vec<String> = (0..25_000)
        .map(|mut n| {
            let mut word = String::new();
            for c in &letters {
                word.push(*c);
                word.push_str(marks[n % marks.len()]);
                n /= marks.len();
            }
            word
        })
        .collect();
    let text = words.join(" ");
    let (engine, _dir) = fixture(&[&text]);
    for sort in 0..3 {
        for grouping in [
            None,
            Some(ResultGrouping::SameSection),
            Some(ResultGrouping::IdenticalText),
        ] {
            for offset in [0, 11] {
                let exact = engine
                    .search_and_count_exact(
                        "בראשית".into(),
                        vec![],
                        0,
                        offset,
                        order(sort),
                        true,
                        false,
                        grouping,
                    )
                    .unwrap();
                let fuzzy = engine
                    .search_and_count_fuzzy(
                        "בראשית".into(),
                        vec![],
                        0,
                        offset,
                        0,
                        order(sort),
                        true,
                        false,
                        grouping,
                    )
                    .unwrap();
                for count in [exact, fuzzy] {
                    assert_eq!(count.total_count, 1);
                    assert!(count.truncated);
                    assert!(count.results.is_empty());
                    assert_eq!(count.group_count, grouping.map(|_| 1));
                }
            }
        }
    }
}
