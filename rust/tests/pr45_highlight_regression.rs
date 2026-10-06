// Review of PR #45, commit 062bb8236d7d58efe1f74a8adc5b60033982782c.
// Spelling shapes must not consume the budget needed by indexed typo hits.
use anyhow::Result;
use search_engine::api::search_engine::{ResultsOrder, SearchEngine, SearchResult, SearchScope};
use std::collections::HashMap;
use tempfile::TempDir;

fn make_engine() -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    let engine = SearchEngine::new(dir.path().to_str().unwrap());
    (engine, dir)
}

fn add(engine: &mut SearchEngine, id: u64, text: &str, file_path: &str) {
    engine
        .add_document(
            id, "title", "ref", "/root", text, 0, false, file_path, None, None, None,
        )
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn search_advanced_default(
    engine: &SearchEngine,
    query: String,
    facets: Vec<String>,
    limit: u32,
    offset: u32,
    distance: u32,
    custom_spacing: HashMap<String, String>,
    alternative_words: HashMap<u32, Vec<String>>,
    search_options: HashMap<String, HashMap<String, bool>>,
    order: ResultsOrder,
    match_nikud: bool,
    match_taamim: bool,
    scope: SearchScope,
) -> Result<Vec<SearchResult>> {
    let negative_scope = SearchScope::WordDistance;
    engine.search_advanced(
        query,
        String::new(),
        facets,
        limit,
        offset,
        distance,
        distance,
        custom_spacing,
        HashMap::new(),
        alternative_words,
        HashMap::new(),
        search_options,
        HashMap::new(),
        order,
        match_nikud,
        match_taamim,
        scope,
        negative_scope,
        None,
        None,
        None,
    )
}

#[test]
fn spelling_budget_keeps_affix_typo_hits() {
    let (mut engine, _dir) = make_engine();
    for (id, line) in [(1, "כונטרסים תורה"), (2, "קונטרשים תורה")] {
        add(&mut engine, id, line, "/books/a.txt");
    }
    engine.commit().unwrap();
    let options = HashMap::from([(
        "קונטרסים_0".to_string(),
        ["קידומות", "סיומות", "שגיאות כתיב", "כתיב מלא/חסר"]
            .iter()
            .map(|o| (o.to_string(), true))
            .collect(),
    )]);
    let hits = search_advanced_default(
        &engine,
        "קונטרסים תורה".into(),
        vec![],
        10,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        options.clone(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
    )
    .unwrap();
    let ids: Vec<_> = hits.iter().map(|h| h.id).collect();
    assert_eq!(ids.len(), 2);
    let hl = engine
        .generate_index_highlight_pattern(
            "קונטרסים תורה".into(),
            0,
            HashMap::new(),
            HashMap::new(),
            options,
        )
        .unwrap()
        .unwrap();
    let matcher = hl.matcher.unwrap();
    for line in ["כונטרסים תורה", "קונטרשים תורה"] {
        assert_eq!(
            matcher.find_matches(line.into(), vec![]).len(),
            1,
            "found search hit has no highlight: {line}"
        );
    }
}
