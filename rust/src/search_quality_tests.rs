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
            .add_document(
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
