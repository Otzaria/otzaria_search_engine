//! Engine-level tests of advanced-search match quality: which index terms a
//! query word selects, in which order a budget keeps them, how results rank,
//! and what an opened book highlights.

use std::collections::HashMap;

use tempfile::TempDir;

use super::*;
use crate::term_automaton;

fn engine_with(lines: &[&str]) -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    for (id, text) in lines.iter().enumerate() {
        engine
            .add_document_at(
                id as u64,
                "title",
                "ref",
                "/root",
                text,
                0,
                false,
                "/books/a.txt",
                None,
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    (engine, dir)
}

fn options(query: &str, flags: &[&str]) -> HashMap<String, HashMap<String, bool>> {
    hebrew_query::split_query_words(&hebrew_query::normalize_for_index(query))
        .iter()
        .enumerate()
        .map(|(i, w)| {
            (
                format!("{w}_{i}"),
                flags.iter().map(|f| (f.to_string(), true)).collect(),
            )
        })
        .collect()
}

fn advanced_ids(
    engine: &SearchEngine,
    query: &str,
    flags: &[&str],
    order: ResultsOrder,
) -> Vec<u64> {
    engine
        .search_advanced(
            query.to_string(),
            String::new(),
            Vec::new(),
            100,
            0,
            0,
            0,
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            options(query, flags),
            HashMap::new(),
            order,
            false,
            false,
            SearchScope::WordDistance,
            SearchScope::WordDistance,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

fn index_highlight(engine: &SearchEngine, query: &str, flags: &[&str]) -> HighlightPattern {
    engine
        .generate_index_highlight_pattern(
            query.to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            options(query, flags),
            None,
            None,
        )
        .unwrap()
        .expect("a highlight pattern")
}

#[test]
fn search_and_highlight_share_compiled_automata() {
    let (engine, _dir) = engine_with(&["בשבתות ושבתות", "שבת שלום"]);
    let flags = ["קידומות דקדוקיות", "סיומות דקדוקיות"];
    assert!(!advanced_ids(&engine, "שבת", &flags, ResultsOrder::Catalogue).is_empty());
    let after_search = term_automaton::builds_on_this_thread();
    index_highlight(&engine, "שבת", &flags);
    assert_eq!(
        term_automaton::builds_on_this_thread(),
        after_search,
        "the highlight must reuse the search's DFA"
    );
}

/// otzaria#3673: adding "קידומות ארמיות" to any set of the regular options never drops
/// a match that set found, typo variants of a phrase word included.
#[test]
fn aramaic_prefixes_never_drop_matches_of_the_other_options() {
    let (engine, _dir) = engine_with(&[
        "לבבות דקל",
        "ולבבות דקל",
        "דלבבות דקל",
        "לבוות דקל",
        "ללבבת דקל",
        "דקל ללבבת",
        "דקל דללבות",
    ]);
    let regular = [
        "קידומות דקדוקיות",
        "סיומות דקדוקיות",
        "כתיב מלא/חסר",
        "חלק ממילה",
        "שגיאות כתיב",
    ];
    let mut bases: Vec<Vec<&str>> = vec![Vec::new(), regular.to_vec()];
    bases.extend(regular.iter().map(|o| vec![*o]));
    for query in ["לבבות דקל", "ללבות דקל", "דקל ללבות"] {
        for base in &bases {
            let before = advanced_ids(&engine, query, base, ResultsOrder::Catalogue);
            let with: Vec<&str> = base
                .iter()
                .chain(["קידומות ארמיות"].iter())
                .copied()
                .collect();
            let after = advanced_ids(&engine, query, &with, ResultsOrder::Catalogue);
            let lost: Vec<u64> = before
                .iter()
                .filter(|id| !after.contains(id))
                .copied()
                .collect();
            assert!(lost.is_empty(), "{query} {base:?}: lost {lost:?}");
        }
    }
    let mut all = regular.to_vec();
    all.push("קידומות ארמיות");
    let ids = advanced_ids(&engine, "לבבות דקל", &all, ResultsOrder::Catalogue);
    for id in [0, 1, 2] {
        assert!(ids.contains(&id), "{id} missing from {ids:?}");
    }
}

#[test]
fn vocalized_prefixes_survive_phrase_budget_trimming() {
    let (engine, _dir) = engine_with(&["וּבְרֵאשִׁית שָׁלוֹם", "בְרֵאשִׁית שָׁלוֹם"]);
    let ids = advanced_ids(
        &engine,
        "בראשית שלום",
        &["ניקוד", "שגיאות כתיב", "קידומות דקדוקיות", "קידומות ארמיות"],
        ResultsOrder::Catalogue,
    );
    assert_eq!(ids, [0, 1]);
}

#[test]
fn final_letter_suffix_windows_keep_phrase_typo_results() {
    let (engine, _dir) = engine_with(&["אברעם שלום", "ואברעם שלום", "אברהם שלום"]);
    let ids = advanced_ids(
        &engine,
        "אברהם שלום",
        &["שגיאות כתיב", "כתיב מלא/חסר", "סיומות", "קידומות ארמיות"],
        ResultsOrder::Catalogue,
    );
    assert_eq!(ids, [0, 1, 2]);
}

fn kept_terms(engine: &SearchEngine, branches: &[&str], max_terms: u32) -> (Vec<String>, bool) {
    let field = engine.schema.get_field("text").unwrap();
    let branches: Vec<String> = branches.iter().map(|b| b.to_string()).collect();
    let entry = engine
        .materialize_term_set(&branches, &[], field, max_terms, u64::MAX)
        .unwrap();
    let mut terms: Vec<String> = entry
        .terms
        .iter()
        .map(|t| t.value().as_str().unwrap().to_string())
        .collect();
    terms.sort();
    (terms, entry.truncated)
}

#[test]
fn budget_keeps_the_closest_and_most_frequent_forms() {
    // Dictionary order reaches באמר/האמר first; the frequent ואמר/שאמר must
    // win the budget instead.
    let mut lines = vec!["אמר", "באמר", "האמר"];
    lines.extend(["ואמר"; 5]);
    lines.extend(["שאמר"; 4]);
    let (engine, _dir) = engine_with(&lines);
    let (terms, truncated) = kept_terms(&engine, &["(?:ו|ב|ה|ש)?אמר"], 3);
    assert_eq!(terms, ["אמר", "ואמר", "שאמר"]);
    assert!(truncated);
}

#[test]
fn budget_ranks_by_branch_before_frequency() {
    let mut lines = vec!["אמרו"];
    lines.extend(["ואמר"; 9]);
    let (engine, _dir) = engine_with(&lines);
    let (terms, truncated) = kept_terms(&engine, &["אמר.", ".אמר"], 1);
    assert_eq!(terms, ["אמרו"]);
    assert!(truncated);
    let (terms, truncated) = kept_terms(&engine, &["אמר.", ".אמר"], 5);
    assert_eq!(terms, ["אמרו", "ואמר"]);
    assert!(!truncated);
}

#[test]
fn ranking_does_not_depend_on_segments() {
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    engine.set_bulk_indexing(true).unwrap();
    for (id, text) in ["שאמר", "שאמר באמר", "ואמר שאמר", "ואמר האמר", "אמר"]
        .iter()
        .enumerate()
    {
        engine
            .add_document_at(
                id as u64,
                "title",
                "ref",
                "/root",
                text,
                0,
                false,
                "/books/a.txt",
                None,
                None,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
    }
    assert!(engine.get_segment_count().unwrap() > 1);
    let (terms, truncated) = kept_terms(&engine, &["(?:ו|ב|ה|ש)?אמר"], 3);
    assert_eq!(terms, ["אמר", "ואמר", "שאמר"]);
    assert!(truncated);
}

#[test]
fn relevance_ranks_exact_then_affixed_then_spelling_then_typo() {
    // Catalogue order is the reverse of the tiers.
    let (engine, _dir) = engine_with(&["ברקה", "בריכה", "ובברכה", "ברכה"]);
    let flags = ["קידומות דקדוקיות", "כתיב מלא/חסר", "שגיאות כתיב"];
    let catalogue = advanced_ids(&engine, "ברכה", &flags, ResultsOrder::Catalogue);
    let relevance = advanced_ids(&engine, "ברכה", &flags, ResultsOrder::Relevance);
    assert_eq!(catalogue, [0, 1, 2, 3]);
    assert_eq!(relevance, [3, 2, 1, 0]);
}

fn word_options(query: &str, per_word: &[&[&str]]) -> HashMap<String, HashMap<String, bool>> {
    hebrew_query::split_query_words(&hebrew_query::normalize_for_index(query))
        .iter()
        .zip(per_word)
        .enumerate()
        .map(|(i, (w, flags))| {
            (
                format!("{w}_{i}"),
                flags.iter().map(|f| (f.to_string(), true)).collect(),
            )
        })
        .collect()
}

fn phrase_ids(
    engine: &SearchEngine,
    query: &str,
    options: HashMap<String, HashMap<String, bool>>,
) -> Vec<u64> {
    engine
        .search_advanced(
            query.to_string(),
            String::new(),
            Vec::new(),
            100,
            0,
            0,
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
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
}

/// Otzaria/otzaria#3673: within a phrase, a typo-tolerant word of four letters
/// or more takes any single-letter edit — not only the common confusions.
#[test]
fn phrase_typo_takes_any_single_letter_edit() {
    let (engine, _dir) = engine_with(&["פאלמיטו: לבבות דקל (hearts of palm)", "לבות דקל"]);
    const TYPO: &str = "שגיאות כתיב";
    const GRAM_PREFIX: &str = "קידומות דקדוקיות";

    // ל→ב at the first word, typo on that word only.
    let ids = phrase_ids(
        &engine,
        "ללבות דקל",
        word_options("ללבות דקל", &[&[TYPO], &[]]),
    );
    assert!(ids.contains(&0), "{ids:?}");
    // The edit on the second word.
    let ids = phrase_ids(
        &engine,
        "פאלמיטו ללבות",
        word_options("פאלמיטו ללבות", &[&[], &[TYPO]]),
    );
    assert_eq!(ids, [0]);
    // Insertion, deletion and transposition within a phrase.
    for query in ["לבבבות דקל", "לבות דקל", "לבבתו דקל"] {
        let ids = phrase_ids(&engine, query, word_options(query, &[&[TYPO], &[]]));
        assert!(ids.contains(&0), "{query}: {ids:?}");
    }
    // Typo composed with a morphological option.
    let both: &[&str] = &[TYPO, GRAM_PREFIX];
    let ids = phrase_ids(
        &engine,
        "ללבות דקל",
        word_options("ללבות דקל", &[both, both]),
    );
    assert!(ids.contains(&0), "{ids:?}");
    // A three-letter word still takes only the common confusions (ל→ר is none).
    let ids = phrase_ids(
        &engine,
        "לבבות דקר",
        word_options("לבבות דקר", &[&[TYPO], &[TYPO]]),
    );
    assert!(ids.is_empty(), "{ids:?}");

    // The opened book highlights the form the phrase matched.
    let hl = engine
        .generate_index_highlight_pattern(
            "ללבות דקל".to_string(),
            0,
            HashMap::new(),
            HashMap::new(),
            word_options("ללבות דקל", &[&[TYPO], &[]]),
            None,
            None,
        )
        .unwrap()
        .expect("a highlight pattern");
    let word = regex::Regex::new(&hl.word_patterns[0]).unwrap();
    assert!(word.is_match("לבבות"), "{}", hl.word_patterns[0]);
}

fn sorted(mut ids: Vec<u64>) -> Vec<u64> {
    ids.sort_unstable();
    ids
}

/// A typo substitution stands for another letter, never the typed one: with
/// "ניקוד" on the word, the unchanged letters keep their typed marks.
#[test]
fn phrase_typo_keeps_the_typed_nikud_of_the_unchanged_word() {
    let (engine, _dir) = engine_with(&["בָּרכה שלום", "בְּרכה שלום", "בְּרקה שלום", "בְּרגה שלום"]);
    let options = HashMap::from([(
        "בָּרכה_0".to_string(),
        HashMap::from([
            ("שגיאות כתיב".to_string(), true),
            (hebrew_query::OPT_MATCH_NIKUD.to_string(), true),
        ]),
    )]);
    let ids = phrase_ids(&engine, "בָּרכה שלום", options);
    assert_eq!(sorted(ids), [0, 2, 3]);
}

/// The full edits come after every sampled typo variant and in a budget of
/// their own: they never push one out of the phrase char budget.
#[test]
fn phrase_typo_edits_keep_every_sampled_variant() {
    let (engine, _dir) = engine_with(&["ספרם שלום"]);
    let affixes: &[&str] = &[
        "שגיאות כתיב",
        "קידומות דקדוקיות",
        "סיומות דקדוקיות",
        "קידומות ארמיות",
        "סיומות ארמיות",
    ];
    let ids = phrase_ids(
        &engine,
        "ספרים שלום",
        word_options("ספרים שלום", &[affixes, &[]]),
    );
    assert_eq!(ids, [0]);
}

/// The same on the vocalized path: the deletion ללות of לְלבות stays.
#[test]
fn vocalized_phrase_typo_edits_keep_every_sampled_variant() {
    let (engine, _dir) = engine_with(&["ללִות שלום"]);
    let options = HashMap::from([(
        "לְלבות_0".to_string(),
        HashMap::from(
            [
                "שגיאות כתיב",
                "קידומות דקדוקיות",
                "סיומות דקדוקיות",
                hebrew_query::OPT_MATCH_NIKUD,
            ]
            .map(|f| (f.to_string(), true)),
        ),
    )]);
    assert_eq!(phrase_ids(&engine, "לְלבות שלום", options), [0]);
}

/// A sampled typo variant still composes with כתיב מלא/חסר.
#[test]
fn phrase_typo_composes_with_spelling() {
    let (engine, _dir) = engine_with(&["בירקה שלום", "בריקה שלום", "בורקה שלום"]);
    let flags: &[&str] = &["שגיאות כתיב", "כתיב מלא/חסר"];
    let ids = phrase_ids(
        &engine,
        "ברכה שלום",
        word_options("ברכה שלום", &[flags, &[]]),
    );
    assert_eq!(sorted(ids), [0, 1, 2]);
}

/// Otzaria/otzaria#3673 with each option that composes with typo tolerance,
/// and with all of them.
#[test]
fn phrase_typo_edit_survives_the_other_options() {
    let (engine, _dir) = engine_with(&["פאלמיטו: לבבות דקל (hearts of palm)"]);
    let extra = [
        "קידומות דקדוקיות",
        "סיומות דקדוקיות",
        "כתיב מלא/חסר",
        "קידומות ארמיות",
        "סיומות ארמיות",
        "חלק ממילה",
    ];
    let all = (1u32 << extra.len()) - 1;
    for mask in (0..extra.len())
        .map(|i| 1u32 << i)
        .chain([all & !(1 << 5), all])
    {
        let flags: Vec<&str> = std::iter::once("שגיאות כתיב")
            .chain(
                (0..extra.len())
                    .filter(|i| mask >> i & 1 == 1)
                    .map(|i| extra[i]),
            )
            .collect();
        let ids = phrase_ids(
            &engine,
            "ללבות דקל",
            word_options("ללבות דקל", &[&flags, &[]]),
        );
        assert_eq!(ids, [0], "{flags:?}");
    }
}

/// The full typo edits must not crowd an exact branch out of the dictionary
/// scan cap before branch ranking gets a chance to keep it.
#[test]
fn phrase_typo_scan_cap_keeps_the_exact_match() {
    let letters: Vec<char> = "אבגדהוזחטיכלמנסעפ".chars().collect();
    let mut noise = String::new();
    let mut count = 0;
    'terms: for a in &letters {
        for b in &letters {
            for c in &letters {
                for d in &letters {
                    // ב→ג is outside the sampled confusions; all these terms
                    // match the new typo+partial branches and sort before לבבות.
                    noise.push_str(&format!("{a}{b}לגבות{c}{d} "));
                    count += 1;
                    if count > term_automaton::COLLECT_CAP {
                        break 'terms;
                    }
                }
            }
        }
    }
    noise.push_str("עולם");
    let (mut engine, _dir) = engine_with(&["לבבות שלום", &noise]);
    engine.merge_into_one_segment();
    let flags: &[&str] = &["שגיאות כתיב", "חלק ממילה"];
    assert_eq!(
        phrase_ids(
            &engine,
            "לבבות שלום",
            word_options("לבבות שלום", &[flags, &[]])
        ),
        [0]
    );
}
