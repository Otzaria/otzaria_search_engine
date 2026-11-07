use search_engine::api::search_engine::{
    ResultGrouping, ResultsOrder, SearchEngine, SearchPageResult, SearchScope,
};
use serde_json::{json, Value};

fn order(name: &str) -> ResultsOrder {
    match name {
        "catalogue" => ResultsOrder::Catalogue,
        "generation" => ResultsOrder::Generation,
        "relevance" => ResultsOrder::Relevance,
        _ => unreachable!(),
    }
}

fn page(name: &str, page: SearchPageResult) -> Value {
    json!({
        "case": name,
        "total_count": page.total_count,
        "truncated": page.truncated,
        "group_count": page.group_count,
        "results": page.results.iter().map(|r| json!({
            "title": r.title,
            "reference": r.reference,
            "text": r.text,
            "id": r.id,
            "segment": r.segment,
            "is_pdf": r.is_pdf,
            "file_path": r.file_path,
            "merged_count": r.merged_count,
            "merged": r.merged.iter().map(|s| json!({
                "title": s.title, "reference": s.reference,
                "id": s.id, "segment": s.segment, "is_pdf": s.is_pdf,
                "file_path": s.file_path,
            })).collect::<Vec<_>>(),
            "text_status": format!("{:?}", r.text_status),
            "continues_to_next_line": r.continues_to_next_line,
        })).collect::<Vec<_>>(),
    })
}

pub fn contract(engine: &SearchEngine) -> Value {
    let mut pages = Vec::new();
    for ordering in ["catalogue", "generation", "relevance"] {
        pages.push(page(
            &format!("exact-{ordering}"),
            engine
                .search_and_count_exact(
                    "מחשב".into(),
                    vec![],
                    20,
                    0,
                    order(ordering),
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap(),
        ));
        pages.push(page(
            &format!("phrase-{ordering}"),
            engine
                .search_and_count_exact(
                    "מחשב מהיר".into(),
                    vec![],
                    20,
                    0,
                    order(ordering),
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap(),
        ));
        pages.push(page(
            &format!("fuzzy-{ordering}"),
            engine
                .search_and_count_fuzzy(
                    "מחשף".into(),
                    vec![],
                    20,
                    0,
                    1,
                    order(ordering),
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap(),
        ));
        pages.push(page(
            &format!("grouped-{ordering}"),
            engine
                .search_and_count_exact(
                    "מחשב".into(),
                    vec![],
                    20,
                    0,
                    order(ordering),
                    false,
                    false,
                    Some(ResultGrouping::IdenticalText),
                    None,
                )
                .unwrap(),
        ));
    }
    pages.push(page(
        "pagination",
        engine
            .search_and_count_exact(
                "מחשב".into(),
                vec![],
                2,
                1,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap(),
    ));
    pages.push(page(
        "facet",
        engine
            .search_and_count_exact(
                "מחשב".into(),
                vec!["/devices".into()],
                20,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap(),
    ));
    pages.push(page(
        "vocalized",
        engine
            .search_and_count_exact(
                "מַחְשֵׁב".into(),
                vec![],
                20,
                0,
                ResultsOrder::Catalogue,
                true,
                false,
                None,
                None,
            )
            .unwrap(),
    ));
    pages.push(page(
        "advanced-negative",
        engine
            .search_and_count_advanced(
                "מסך".into(),
                "ירוק".into(),
                vec![],
                20,
                0,
                0,
                0,
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                Default::default(),
                ResultsOrder::Catalogue,
                false,
                false,
                SearchScope::SameParagraph,
                SearchScope::SameParagraph,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap(),
    ));
    json!({
        "document_count": engine.get_document_count(),
        "pages": pages,
        "book_counts": engine.count_by_book_exact("מחשב".into(), vec![], false, false, None).unwrap(),
    })
}
