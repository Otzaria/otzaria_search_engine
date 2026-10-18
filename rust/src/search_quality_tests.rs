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
