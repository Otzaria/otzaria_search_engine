//! Independent QA of the hybrid query-shape / matched-term highlighting path.
use search_engine::api::search_engine::{ResultsOrder, SearchEngine, SearchScope};
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tempfile::TempDir;

type Options = HashMap<String, HashMap<String, bool>>;

fn options(word: &str, position: usize, flags: &[&str]) -> Options {
    HashMap::from([(
        format!("{word}_{position}"),
        flags.iter().map(|flag| ((*flag).into(), true)).collect(),
    )])
}

fn engine(lines: &[&str]) -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    for (id, line) in lines.iter().enumerate() {
        engine
            .add_document(
                id as u64,
                "title",
                "ref",
                "/root",
                line,
                0,
                false,
                "/books/qa.txt",
                None,
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    (engine, dir)
}

fn assert_search_hit_highlights(
    lines: &[&str],
    query: &str,
    options: Options,
    alternatives: HashMap<u32, Vec<String>>,
    require_every_fixture: bool,
) {
    let (engine, _dir) = engine(lines);
    let hits = engine
        .search_advanced(
            query.into(),
            String::new(),
            vec![],
            100,
            0,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            alternatives.clone(),
            HashMap::new(),
            options.clone(),
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
        .unwrap();
    let pattern = engine
        .generate_index_highlight_pattern(query.into(), 0, HashMap::new(), alternatives, options)
        .unwrap()
        .unwrap();
    let ids: HashSet<_> = hits.iter().map(|hit| hit.id).collect();
    assert!(!ids.is_empty(), "fixture has no search hits: query={query}");
    if require_every_fixture {
        assert_eq!(
            ids.len(),
            lines.len(),
            "fixture did not search-match every line: query={query}, ids={ids:?}"
        );
    }
    let matcher = pattern.matcher.unwrap();
    for (id, line) in lines
        .iter()
        .enumerate()
        .filter(|(id, _)| ids.contains(&(*id as u64)))
    {
        assert_eq!(
            matcher.find_matches((*line).into(), vec![]).len(),
            1,
            "search hit lost phrase highlight: query={query}, id={id}, line={line}"
        );
        assert!(
            !matcher.find_word_matches((*line).into(), vec![]).is_empty(),
            "search hit lost independent highlight: query={query}, line={line}"
        );
    }
}

#[test]
#[ignore = "manual exhaustive hybrid morphology QA; run with --include-ignored"]
fn hybrid_morphology_matrix_preserves_index_hits() {
    for flags in [
        vec!["קידומות"],
        vec!["סיומות"],
        vec!["קידומות", "סיומות"],
        vec!["קידומות דקדוקיות"],
        vec!["סיומות דקדוקיות"],
        vec!["קידומות דקדוקיות", "סיומות דקדוקיות"],
        vec!["חלק ממילה"],
        vec!["קידומות ארמיות"],
    ] {
        let mut flags = flags;
        flags.extend(["שגיאות כתיב", "כתיב מלא/חסר"]);
        for query in ["קונטרסים", "קונטרסים תורה", "אמר קונטרסים תורה"]
        {
            let position = usize::from(query.starts_with("אמר"));
            let lines: Vec<_> = ["כונטרסים", "קונטרשים"]
                .iter()
                .map(|word| query.replace("קונטרסים", word))
                .collect();
            let refs: Vec<_> = lines.iter().map(String::as_str).collect();
            assert_search_hit_highlights(
                &refs,
                query,
                options("קונטרסים", position, &flags),
                HashMap::new(),
                false,
            );
        }
    }
}

#[test]
fn hybrid_interior_spelling_uses_branch_specific_affix_window() {
    assert_search_hit_highlights(
        &["אמר אאבתרה משה", "אמר אאבתורה משה"],
        "אמר תורה משה",
        options(
            "תורה",
            1,
            &["קידומות", "סיומות", "שגיאות כתיב", "כתיב מלא/חסר"],
        ),
        HashMap::new(),
        true,
    );
}

#[test]
fn hybrid_alternatives_and_quoted_nikud_forms_keep_offsets() {
    assert_search_hit_highlights(
        &["אמר הַרַמְבָּ״ם תורה", "אמר הַרַמְוָ״ם תורה", "אמר הַחָכָם תורה"],
        "אמר רמבם תורה",
        options(
            "רמבם",
            1,
            &["קידומות", "שגיאות כתיב", "כתיב מלא/חסר", "התעלם מגרשיים"],
        ),
        HashMap::from([(1, vec!["חכם".into()])]),
        true,
    );
    let (engine, _dir) = engine(&["אמר הַרַמְבָּ״ם תורה"]);
    let pattern = engine
        .generate_index_highlight_pattern(
            "אמר רמבם תורה".into(),
            0,
            HashMap::new(),
            HashMap::new(),
            options("רמבם", 1, &["קידומות", "שגיאות כתיב", "כתיב מלא/חסר"]),
        )
        .unwrap()
        .unwrap();
    let matches = pattern
        .matcher
        .unwrap()
        .find_matches("אמר הַרַמְבָּ״ם תורה".into(), vec![]);
    assert_eq!(matches.len(), 1);
    let units: Vec<u16> = "אמר הַרַמְבָּ״ם תורה".encode_utf16().collect();
    let word = &matches[0].word_ranges[1];
    assert_eq!(
        String::from_utf16(
            &units
                [(matches[0].start + word.start) as usize..(matches[0].start + word.end) as usize]
        )
        .unwrap(),
        "רַמְבָּ״ם"
    );
}

#[test]
fn typo_without_morphology_keeps_token_boundaries() {
    let (engine, _dir) = engine(&["כונטרסים תורה", "קונטרשים תורה"]);
    let pattern = engine
        .generate_index_highlight_pattern(
            "קונטרסים תורה".into(),
            0,
            HashMap::new(),
            HashMap::new(),
            options("קונטרסים", 0, &["שגיאות כתיב", "כתיב מלא/חסר"]),
        )
        .unwrap()
        .unwrap();
    assert_eq!(pattern.word_boundary_eligible, vec![true, true]);
    let matcher = pattern.matcher.unwrap();
    assert!(matcher
        .find_matches("אכונטרסים תורה".into(), vec![])
        .is_empty());
    assert!(matcher
        .find_matches("כונטרסים התורה".into(), vec![])
        .is_empty());
}

#[test]
fn hybrid_large_dictionary_compiles_and_scans_with_bounded_pattern() {
    let letters: Vec<char> = "אבגדהוזחטיכלמנסעפצקרשת".chars().collect();
    let words: String = (0..2_000)
        .map(|i| {
            format!(
                "{}{}כונטרסים{} ",
                letters[i % letters.len()],
                letters[(i / letters.len()) % letters.len()],
                letters[(i / letters.len().pow(2)) % letters.len()]
            )
        })
        .collect();
    let (engine, _dir) = engine(&[&words, "כונטרסים", "קונטרשים"]);
    let started = Instant::now();
    let pattern = engine
        .generate_index_highlight_pattern(
            "קונטרסים".into(),
            0,
            HashMap::new(),
            HashMap::new(),
            options(
                "קונטרסים",
                0,
                &["קידומות", "סיומות", "שגיאות כתיב", "כתיב מלא/חסר"],
            ),
        )
        .unwrap()
        .unwrap();
    let preparation = started.elapsed();
    assert!(pattern.word_patterns[0].chars().count() <= 24_128);
    let matcher = pattern.matcher.unwrap();
    for word in ["כונטרסים", "קונטרשים", "הקונטרסיםא"] {
        assert_eq!(matcher.find_matches(word.into(), vec![]).len(), 1);
    }
    let started = Instant::now();
    let scanned = matcher.find_matches("נאמר כאן דבר שאינו מתאים ".repeat(2_000), vec![]);
    assert!(scanned.is_empty());
    eprintln!(
        "QA debug timing: prepare={preparation:?}, scan_10000_tokens={:?}",
        started.elapsed()
    );
}
