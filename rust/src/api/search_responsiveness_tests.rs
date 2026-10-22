use super::*;
use tempfile::TempDir;

#[test]
fn empty_grouped_stream_pages_do_not_prepare_highlights() {
    let (engine, _dir) = fixture();
    let field = engine.schema.get_field("text").unwrap();
    for grouping in [ResultGrouping::SameSection, ResultGrouping::IdenticalText] {
        for (limit, offset, no_hits) in [(0, 0, false), (10, 10_000, false), (10, 0, true)] {
            let request = engine.lexical_cancellations.begin(None);
            let query: Box<dyn Query> = if no_hits {
                Box::new(EmptyQuery)
            } else {
                Box::new(AllQuery)
            };
            let mut updates = Vec::new();
            engine
                .run_search_stream_with_counts(
                    query,
                    |_| panic!("an empty page must not prepare highlights"),
                    field,
                    limit,
                    offset,
                    &ResultsOrder::Catalogue,
                    &HighlightConfig::default(),
                    50,
                    false,
                    Some(&grouping),
                    &request,
                    &mut |update| {
                        updates.push(update);
                        true
                    },
                )
                .unwrap();
            assert_eq!(updates.len(), 1);
            assert_eq!(updates[0].total_count, Some(if no_hits { 0 } else { 34 }));
            assert!(updates[0].results.is_empty());
        }
    }
}

fn fixture() -> (SearchEngine, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    for ordinal in 0..34 {
        engine
            .add_document(
                ordinal,
                "ספר לדוגמה",
                "סימן",
                "/root",
                "שלום עולם דברי תורה",
                ordinal as u64,
                false,
                "book.txt",
                Some(ordinal / 2),
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    (engine, dir)
}

fn exact(
    engine: &SearchEngine,
    offset: u32,
    grouping: Option<ResultGrouping>,
    request: Option<u32>,
) -> Result<Vec<SearchResult>> {
    engine.search_exact(
        "שלום".into(),
        vec![],
        10,
        offset,
        ResultsOrder::Catalogue,
        false,
        false,
        grouping,
        request,
    )
}

#[test]
fn non_streamed_pages_and_counts_respect_cancellation_and_leave_other_requests_usable() {
    let (engine, _dir) = fixture();
    engine.cancel_lexical_search(7);
    let errors = [
        exact(&engine, 0, None, Some(7)).err().unwrap(),
        exact(&engine, 10, Some(ResultGrouping::SameSection), Some(7))
            .err()
            .unwrap(),
        engine
            .search_and_count_exact(
                "שלום".into(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .search_fuzzy(
                "שלום".into(),
                vec![],
                10,
                10,
                1,
                ResultsOrder::Relevance,
                false,
                false,
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .search_and_count_fuzzy(
                "שלום".into(),
                vec![],
                10,
                0,
                1,
                ResultsOrder::Relevance,
                true,
                false,
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .search(
                vec!["שלום".into()],
                vec![],
                10,
                0,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .search_and_count(
                vec!["שלום".into()],
                vec![],
                10,
                0,
                0,
                100,
                ResultsOrder::Catalogue,
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .search_advanced(
                "שלום".into(),
                String::new(),
                vec![],
                10,
                0,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                ResultsOrder::Relevance,
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
                None,
                None,
                None,
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .search_and_count_advanced(
                "שלום".into(),
                String::new(),
                vec![],
                10,
                0,
                0,
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                ResultsOrder::Relevance,
                false,
                false,
                SearchScope::WordDistance,
                SearchScope::WordDistance,
                None,
                None,
                None,
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .generate_index_highlight_pattern(
                "שלום".into(),
                0,
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                None,
                Some(7),
            )
            .err()
            .unwrap(),
        engine
            .generate_index_fuzzy_highlight_pattern("שלום".into(), 1, Some(7))
            .err()
            .unwrap(),
    ];
    for error in errors {
        assert!(LexicalSearchCancelled::is(&error), "{error:?}");
    }
    assert_eq!(exact(&engine, 10, None, Some(8)).unwrap().len(), 10);
    assert_eq!(exact(&engine, 0, None, None).unwrap().len(), 10);
}

#[test]
fn grouped_stream_hydrates_requested_batches_and_caches_all_groups_for_pagination() {
    let _sources = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (engine, _dir) = fixture();
    let mut events = Vec::new();
    engine
        .exact_stream_into(
            "שלום".into(),
            100,
            0,
            ResultsOrder::Catalogue,
            Some(ResultGrouping::SameSection),
            50,
            None,
            &mut |event| {
                events.push(event);
                true
            },
        )
        .unwrap();
    assert_eq!(events[0].total_count, Some(34));
    assert_eq!(events[0].group_count, Some(17));
    assert_eq!(
        events
            .iter()
            .skip(1)
            .map(|event| event.results.len())
            .collect::<Vec<_>>(),
        vec![17]
    );
    let streamed: Vec<_> = events.into_iter().flat_map(|event| event.results).collect();
    assert!(streamed
        .iter()
        .all(|result| result.merged_count == 2 && result.merged.len() == 1));
    assert_eq!(engine.grouped_cache.0.lock().unwrap().len(), 1);
    let page = exact(&engine, 10, Some(ResultGrouping::SameSection), None).unwrap();
    assert_eq!(
        page.iter().map(|result| result.id).collect::<Vec<_>>(),
        streamed[10..]
            .iter()
            .map(|result| result.id)
            .collect::<Vec<_>>()
    );
    engine.release_memory();
    assert_eq!(engine.grouped_cache.0.lock().unwrap().len(), 0);
    assert_eq!(
        exact(&engine, 10, Some(ResultGrouping::SameSection), None)
            .unwrap()
            .iter()
            .map(|result| result.id)
            .collect::<Vec<_>>(),
        page.iter().map(|result| result.id).collect::<Vec<_>>()
    );
}

#[test]
fn streamed_small_and_large_chunks_keep_the_paged_results_and_highlights() {
    let (engine, _dir) = fixture();
    let bits = |results: Vec<SearchResult>| {
        results
            .into_iter()
            .map(|result| {
                (
                    result.id,
                    result.segment,
                    result.file_path,
                    result.text,
                    result.text_status,
                    result.continues_to_next_line,
                    result.merged_count,
                    result
                        .merged
                        .into_iter()
                        .map(|sibling| (sibling.id, sibling.segment, sibling.file_path))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    };
    for grouping in [None, Some(ResultGrouping::SameSection)] {
        let paged = engine
            .search_exact(
                "שלום".into(),
                vec![],
                100,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                grouping,
                None,
            )
            .unwrap();
        let mut streamed = Vec::new();
        engine
            .exact_stream_into(
                "שלום".into(),
                100,
                0,
                ResultsOrder::Catalogue,
                grouping,
                50,
                None,
                &mut |event| {
                    streamed.extend(event.results);
                    true
                },
            )
            .unwrap();
        assert_eq!(bits(streamed), bits(paged));
    }
}

#[test]
fn cancelling_after_counts_prevents_hydration_and_any_further_event() {
    let (engine, _dir) = fixture();
    let mut events = 0;
    let result = engine.exact_stream_into(
        "שלום".into(),
        100,
        0,
        ResultsOrder::Catalogue,
        None,
        50,
        Some(9),
        &mut |event| {
            events += 1;
            assert!(event.results.is_empty());
            engine.cancel_lexical_search(9);
            true
        },
    );
    assert!(LexicalSearchCancelled::is(&result.err().unwrap()));
    assert_eq!(events, 1);
    assert_eq!(exact(&engine, 0, None, Some(10)).unwrap().len(), 10);
}

#[test]
fn cancelling_during_highlight_preparation_discards_the_page() {
    let (engine, _dir) = fixture();
    let request = engine.lexical_cancellations.begin(Some(11));
    let result = request.scope(|| {
        engine.run_search(
            Box::new(AllQuery),
            |_| {
                engine.cancel_lexical_search(11);
                Ok(HighlightPlan::none())
            },
            engine.schema.get_field("text")?,
            10,
            0,
            &ResultsOrder::Catalogue,
            &HighlightConfig::default(),
            None,
        )
    });
    assert!(LexicalSearchCancelled::is(&result.err().unwrap()));
}

#[test]
fn search_snapshot_changes_only_when_search_inputs_change() {
    let _sources = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (mut engine, _dir) = fixture();
    let first = engine.search_snapshot();
    assert_eq!(engine.search_snapshot(), first);
    engine.index_reader.reload().unwrap();
    assert_eq!(engine.search_snapshot(), first);
    engine.release_memory();
    assert_eq!(engine.search_snapshot(), first);
    engine.set_magic_dictionary_path("missing-lexical.db".into());
    let dictionary = engine.search_snapshot();
    assert_ne!(dictionary, first);
    engine
        .set_book_order(vec![BookOrderEntry {
            file_path: "book.txt".into(),
            catalogue_order: 1,
            generation_order: 1,
        }])
        .unwrap();
    let ordered = engine.search_snapshot();
    assert_ne!(ordered, dictionary);
    engine
        .add_document(
            35, "ספר", "סימן", "/root", "שלום", 35, false, "book.txt", None, None,
        )
        .unwrap();
    engine.commit().unwrap();
    assert_ne!(engine.search_snapshot(), ordered);
}

#[test]
fn a_missing_snippet_is_bounded_and_escaped_instead_of_returning_the_full_line() {
    let (mut engine, _dir) = fixture();
    let text = "&value ".repeat(1_000);
    engine
        .add_document(
            100, "ספר", "סימן", "/root", &text, 100, false, "long.txt", None, None,
        )
        .unwrap();
    engine.commit().unwrap();
    let results = engine
        .run_search(
            Box::new(AllQuery),
            |_| Ok(HighlightPlan::none()),
            engine.schema.get_field("text").unwrap(),
            100,
            0,
            &ResultsOrder::Catalogue,
            &HighlightConfig {
                max_chars: 80,
                ..HighlightConfig::default()
            },
            None,
        )
        .unwrap();
    let long = results
        .iter()
        .find(|result| result.file_path == "long.txt")
        .unwrap();
    assert!(long.text.len() < 500, "{} bytes", long.text.len());
    assert!(long.text.contains("&amp;"));
}

#[test]
fn a_group_page_collected_before_memory_pressure_is_not_retained_after_it() {
    let _sources = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (engine, _dir) = fixture();
    let searcher = engine.index_reader.searcher();
    let query: Box<dyn Query> = Box::new(AllQuery);
    let key = engine
        .group_cache_key(
            &searcher,
            query.as_ref(),
            &ResultGrouping::SameSection,
            &ResultsOrder::Catalogue,
        )
        .unwrap();
    let page = engine
        .collect_grouped(
            &searcher,
            query.as_ref(),
            &ResultGrouping::SameSection,
            100,
            0,
            &ResultsOrder::Catalogue,
        )
        .unwrap();
    engine.release_memory();
    engine.grouped_cache.put(key, Arc::new(page));
    assert_eq!(engine.grouped_cache.0.lock().unwrap().len(), 0);
}

#[test]
fn releasing_memory_also_releases_idle_term_work_keys() {
    let _sources = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (engine, _dir) = fixture();
    engine
        .materialize_term_set(
            &["שלום".into()],
            &[],
            engine.schema.get_field("text").unwrap(),
            100,
            u64::MAX,
        )
        .unwrap();
    assert!(engine.term_work.retained_keys() > 0);
    engine.release_memory();
    assert_eq!(engine.term_work.retained_keys(), 0);
}

#[test]
fn user_regex_queries_cancel_after_term_materialization_and_before_collection() {
    let (engine, _dir) = fixture();
    for raw in ["/שלום/", "/שלום עולם/"] {
        let token = UserRegexCancellationToken::new();
        let searcher = engine.index_reader.searcher();
        let regex = SearchEngine::user_regex(raw).unwrap();
        let (query, _, _) = engine
            .user_regex_term_query(&searcher, &regex, &[], &token.flag)
            .unwrap();
        token.cancel();
        assert!(searcher.search(query.as_ref(), &Count).is_err());
        let error = SearchEngine::user_regex_checked(
            &token.flag,
            searcher.search(query.as_ref(), &BookCountCollector),
        )
        .unwrap_err();
        assert!(error.to_string().contains(crate::user_regex::CANCELLED));
        let mut collector = FacetCollector::for_field("topics");
        collector.add_facet("/root");
        assert!(SearchEngine::user_regex_checked(
            &token.flag,
            searcher.search(query.as_ref(), &collector)
        )
        .is_err());
    }
}

#[test]
fn user_regex_streams_stop_after_callback_cancellation_even_when_cached_or_empty() {
    let _sources = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (engine, _dir) = fixture();
    for raw in ["/שלום/", "/שלום עולם/", "/שלום .* תורה/"] {
        let warm = Arc::default();
        engine
            .user_regex_stream_into(
                raw,
                &[],
                100,
                0,
                &ResultsOrder::Catalogue,
                10,
                &warm,
                &mut |_| true,
            )
            .unwrap();
        for limit in [0, 100] {
            if limit == 0
                && SearchEngine::user_regex(raw).unwrap().kind == crate::user_regex::RegexKind::Line
            {
                continue;
            }
            let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut events = Vec::new();
            engine
                .user_regex_stream_into(
                    raw,
                    &[],
                    limit,
                    0,
                    &ResultsOrder::Catalogue,
                    1,
                    &cancel,
                    &mut |event| {
                        if !event.results.is_empty() || (limit == 0 && event.total_count.is_some())
                        {
                            cancel.store(true, Ordering::Relaxed);
                        }
                        events.push(event);
                        true
                    },
                )
                .unwrap();
            let final_event = events.last().unwrap();
            assert!(
                final_event.done && final_event.cancelled,
                "{raw}, limit={limit}"
            );
            assert_eq!(
                events
                    .iter()
                    .map(|event| event.results.len())
                    .sum::<usize>(),
                usize::from(limit > 0)
            );
        }
    }
}

#[test]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn failed_economy_reader_reload_restores_writer_and_explicit_mode() {
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    engine.open_index_writer().unwrap();
    let original_heap = engine.writer_heap_size;
    let error = engine
        .set_economy_indexing_with_reload(true, |_| anyhow::bail!("injected reader reload failure"))
        .unwrap_err();
    assert!(error.to_string().contains("injected"));
    assert!(!engine.economy_indexing);
    assert_eq!(engine.writer_heap_size, original_heap);
    assert!(engine.index_writer.is_some());
    engine.set_economy_indexing(true).unwrap();
    let error = engine
        .set_economy_indexing_with_reload(false, |_| {
            anyhow::bail!("injected reader reload failure")
        })
        .unwrap_err();
    assert!(error.to_string().contains("injected"));
    assert!(engine.economy_indexing);
    assert_eq!(engine.writer_heap_size, ECONOMY_WRITER_HEAP_SIZE);
    assert!(engine.index_writer.is_some());
    engine.set_economy_indexing(false).unwrap();
}

#[test]
fn normal_fast_preparation_has_byte_and_line_safety_caps_and_document_spill() {
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    assert!(engine.normal_fast_preparation("a\nb\n"));
    assert!(!engine.normal_fast_preparation(&"\n".repeat(65_536)));
    assert!(!engine.normal_fast_preparation(&"x".repeat((32 << 20) + 1)));
    let meta = TextBookMeta {
        title: "ספר".into(),
        topics: "/root".into(),
        file_path: "book.txt".into(),
        extra_facets: vec![],
        source_hash: 0,
    };
    engine.open_index_writer().unwrap();
    let text = "<h1>פרק</h1>\nשלום עולם\n".repeat(200);
    let memory = engine
        .prepare_text_book_fast(&meta, &text, TextStorage::InIndex, &[], usize::MAX, &|| {
            false
        })
        .unwrap();
    let mut spilled = engine
        .prepare_text_book_fast(&meta, &text, TextStorage::InIndex, &[], 1, &|| false)
        .unwrap();
    assert!(spilled.spooled_documents.is_some());
    let mut actual = Vec::new();
    spilled
        .for_each_document(|document| {
            actual.push(tantivy::Document::to_json(&document, &engine.schema));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        actual,
        memory
            .documents
            .iter()
            .map(|document| tantivy::Document::to_json(document, &engine.schema))
            .collect::<Vec<_>>()
    );
    engine.set_economy_indexing(true).unwrap();
    assert!(!engine.normal_fast_preparation("a\nb"));
}
