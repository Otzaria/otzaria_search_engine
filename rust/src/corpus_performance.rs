//! Opt-in, real-corpus measurements. All writes go to OTZARIA_PERF_INDEX and OTZARIA_PERF_OUT.

use super::*;
use crate::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource,
};
use std::fs::OpenOptions;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

struct Config {
    database: PathBuf,
    index: PathBuf,
    output: PathBuf,
    mode: String,
    samples: usize,
}

impl Config {
    fn record_indexing(&self) {
        let metrics = crate::indexing_diagnostics::snapshot();
        self.record(serde_json::json!({"kind":"indexing_diagnostics",
            "buffered_bytes":metrics.buffered_bytes,"peak_buffered_bytes":metrics.peak_buffered_bytes,
            "spilled_bytes":metrics.spilled_bytes,"source_lock_wait_micros":metrics.source_lock_wait_micros,
            "source_lock_hold_micros":metrics.source_lock_hold_micros,"source_read_micros":metrics.source_read_micros,
            "prepare_micros":metrics.prepare_micros}));
    }
    fn read() -> Self {
        let path = |name| PathBuf::from(std::env::var_os(name).expect(name));
        Self {
            database: path("OTZARIA_PERF_DB"),
            index: path("OTZARIA_PERF_INDEX"),
            output: path("OTZARIA_PERF_OUT"),
            mode: std::env::var("OTZARIA_PERF_MODE").unwrap_or_else(|_| "search".into()),
            samples: std::env::var("OTZARIA_PERF_SAMPLES")
                .ok()
                .and_then(|n| n.parse().ok())
                .unwrap_or(100),
        }
    }

    fn record(&self, value: serde_json::Value) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.output)
            .unwrap();
        serde_json::to_writer(&mut file, &value).unwrap();
        writeln!(file).unwrap();
    }

    fn engine(&self) -> SearchEngine {
        std::fs::create_dir_all(&self.index).unwrap();
        configure_line_source(self.database.display().to_string()).unwrap();
        let mut engine = SearchEngine::new(self.index.to_str().unwrap());
        let dictionary = self.database.with_file_name("lexical.db");
        if dictionary.is_file() {
            assert!(engine.set_magic_dictionary_path(dictionary.display().to_string()));
        }
        engine.warm_book_order().unwrap();
        let searcher = engine.index_reader.searcher();
        self.record(serde_json::json!({
            "kind": "setup", "index": self.index, "docs": searcher.num_docs(),
            "snapshot": format!("{:016x}", page_cache::snapshot_of(&searcher)),
            "segments": searcher.segment_readers().iter().map(|reader| serde_json::json!({
                "id": reader.segment_id().uuid_string(), "max_doc": reader.max_doc(),
                "deleted": reader.num_deleted_docs(), "delete_opstamp": reader.delete_opstamp(),
            })).collect::<Vec<_>>(),
        }));
        engine
    }

    fn books(&self, stress: bool) -> Vec<LibraryBookInput> {
        let connection = rusqlite::Connection::open_with_flags(
            &self.database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let sql = if stress {
            "SELECT id,title FROM book WHERE id IN (SELECT id FROM book WHERE totalLines > 0 ORDER BY totalLines DESC LIMIT 4) ORDER BY id"
        } else if std::env::var_os("OTZARIA_PERF_FULL").is_some() {
            "SELECT id,title FROM book WHERE totalLines > 0 ORDER BY id"
        } else {
            "SELECT id,title FROM book WHERE totalLines > 0 AND \
             ((id % 37 = 0 AND totalLines <= 20000) OR id IN (SELECT id FROM book WHERE totalLines > 0 ORDER BY totalLines DESC LIMIT 5)) ORDER BY id"
        };
        let books = connection
            .prepare(sql)
            .unwrap()
            .query_map([], |row| {
                let id = row.get::<_, i64>(0)?;
                let title = row.get::<_, String>(1)?;
                Ok(LibraryBookInput {
                    source: LibraryBookSource::Library,
                    attached_slug: None,
                    book_id: id,
                    db_title: Some(title.clone()),
                    title,
                    topics: format!("/benchmark/{id}"),
                    file_path: format!("id:{id}"),
                    extra_facets: None,
                    source_hash: None,
                    text_storage: TextStorage::LibraryDb,
                })
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        books
    }
}

#[derive(Clone, Copy)]
struct Case {
    query: &'static str,
    relevance: bool,
    group: u8,
    offset: u32,
}

fn stream_sample(engine: &SearchEngine, case: Case) -> serde_json::Value {
    let started = Instant::now();
    let mut first_ms = None;
    let mut first_size = 0;
    let mut results = Vec::new();
    let mut total = None;
    let mut groups = None;
    let mut book_counts = None;
    let mut truncated = false;
    engine
        .exact_stream_into(
            case.query.into(),
            100,
            case.offset,
            if case.relevance {
                ResultsOrder::Relevance
            } else {
                ResultsOrder::Catalogue
            },
            match case.group {
                1 => Some(ResultGrouping::SameSection),
                2 => Some(ResultGrouping::IdenticalText),
                _ => None,
            },
            50,
            None,
            &mut |update| {
                if !update.results.is_empty() && first_ms.is_none() {
                    first_ms = Some(started.elapsed().as_secs_f64() * 1000.0);
                    first_size = update.results.len();
                }
                if update.total_count.is_some() {
                    total = update.total_count;
                }
                if update.group_count.is_some() {
                    groups = update.group_count;
                }
                if update.book_counts.is_some() {
                    book_counts = update.book_counts;
                }
                truncated |= update.truncated;
                results.extend(update.results);
                true
            },
        )
        .unwrap();
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(results.len() <= 100);
    if let (Some(total), Some(counts)) = (total, &book_counts) {
        assert_eq!(
            total as u64,
            counts.values().map(|n| u64::from(*n)).sum::<u64>()
        );
    }
    let mut digest = DefaultHasher::new();
    let mut identity_digest = DefaultHasher::new();
    let mut text_digest = DefaultHasher::new();
    let mut highlight_digest = DefaultHasher::new();
    let highlight = HighlightConfig::default();
    for result in &results {
        (
            result.id,
            &result.file_path,
            result.segment,
            &result.text,
            result.merged_count,
        )
            .hash(&mut digest);
        (
            result.id,
            &result.file_path,
            result.segment,
            result.merged_count,
        )
            .hash(&mut identity_digest);
        result.text.hash(&mut text_digest);
        let spans: Vec<_> = result
            .text
            .split(highlight.highlight_prefix.as_str())
            .skip(1)
            .filter_map(|tail| {
                tail.split_once(highlight.highlight_postfix.as_str())
                    .map(|(span, _)| span)
            })
            .collect();
        spans.hash(&mut highlight_digest);
        for sibling in &result.merged {
            (sibling.id, &sibling.file_path).hash(&mut digest);
            (sibling.id, &sibling.file_path, sibling.segment).hash(&mut identity_digest);
        }
    }
    let dump = std::env::var_os("OTZARIA_PERF_DUMP_RESULTS").map(|_| {
        results.iter().map(|result| serde_json::json!({
            "id": result.id, "file_path": result.file_path, "segment": result.segment,
            "title": result.title, "reference": result.reference, "text": result.text,
            "text_status": format!("{:?}", result.text_status),
            "continues_to_next_line": result.continues_to_next_line,
            "merged_count": result.merged_count,
            "merged": result.merged.iter().map(|sibling| serde_json::json!({
                "id": sibling.id, "file_path": sibling.file_path, "segment": sibling.segment,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>()
    });
    serde_json::json!({
        "kind": "stream", "query": case.query, "order": if case.relevance { "relevance" } else { "catalogue" },
        "grouping": case.group, "offset": case.offset,
        "first_ms": first_ms, "first_size": first_size, "total_ms": total_ms,
        "total": total, "groups": groups, "hits": results.len(), "truncated": truncated,
        "digest": format!("{:016x}",digest.finish()),
        "identity_digest": format!("{:016x}",identity_digest.finish()),
        "text_digest": format!("{:016x}",text_digest.finish()),
        "highlight_digest": format!("{:016x}",highlight_digest.finish()), "results": dump,
    })
}

fn cases() -> Vec<Case> {
    let mut cases = Vec::new();
    for query in ["שבת", "אמר", "תורה", "קריאת שמע", "פיקוח נפש", "רמבם"]
    {
        for relevance in [false, true] {
            cases.push(Case {
                query,
                relevance,
                group: 0,
                offset: 0,
            });
        }
    }
    for group in [1, 2] {
        for offset in [0, 100] {
            cases.push(Case {
                query: "שבת",
                relevance: false,
                group,
                offset,
            });
        }
    }
    cases
}

fn strategy_sample(
    engine: &SearchEngine,
    case: Case,
    split: bool,
    fuzzy: bool,
) -> serde_json::Value {
    engine.page_cache.clear();
    let started = Instant::now();
    let searcher = engine.index_reader.searcher();
    let voc = VocalizedFlags::new(false, false);
    let (query, plan) = if fuzzy {
        let tokens = engine.index_token_texts(case.query).unwrap();
        let phrases = engine.quoted_phrase_token_groups(case.query).unwrap();
        let query = engine
            .build_fuzzy_search_query(&tokens, &phrases, &[], 1, case.relevance)
            .unwrap();
        let plan = engine
            .fuzzy_highlight_plan(&searcher, &tokens, &phrases, 1)
            .unwrap();
        (query, plan)
    } else {
        let (query, _) = engine.build_exact_query(case.query, &[], &voc).unwrap();
        let plan = engine
            .exact_highlight_plan(&searcher, case.query, &voc)
            .unwrap();
        (query, plan)
    };
    let page = engine
        .ranked_page(
            &searcher,
            query.as_ref(),
            100,
            0,
            &if case.relevance {
                ResultsOrder::Relevance
            } else {
                ResultsOrder::Catalogue
            },
            if split {
                PageCounts::None
            } else {
                PageCounts::TotalAndBooks
            },
        )
        .unwrap();
    let hl = HighlightConfig::default();
    let field = engine.search_text_field(&voc).unwrap();
    let generator = SearchEngine::make_snippet_generator(
        &searcher,
        plan.query.as_deref().unwrap_or(query.as_ref()),
        field,
        &hl,
    )
    .unwrap();
    let first_len = page.addresses.len().min(10);
    let mut results = SearchEngine::build_results_with_generator(
        &engine.schema,
        &searcher,
        &generator,
        field,
        page.addresses[..first_len].to_vec(),
        &hl,
        plan.phrase.as_ref(),
    )
    .unwrap();
    let first_ms = started.elapsed().as_secs_f64() * 1000.0;
    let counts = if split {
        searcher
            .search(query.as_ref(), &BookCountCollector)
            .unwrap()
    } else {
        page.book_counts.unwrap()
    };
    results.extend(
        SearchEngine::build_results_with_generator(
            &engine.schema,
            &searcher,
            &generator,
            field,
            page.addresses[first_len..].to_vec(),
            &hl,
            plan.phrase.as_ref(),
        )
        .unwrap(),
    );
    let mut digest = DefaultHasher::new();
    for result in &results {
        (result.id, &result.file_path, result.segment, &result.text).hash(&mut digest);
    }
    serde_json::json!({"kind":"strategy", "query":case.query, "relevance":case.relevance,
        "split":split,"fuzzy":fuzzy,"first_ms":first_ms,"total_ms":started.elapsed().as_secs_f64()*1000.0,
        "total":counts.values().map(|n|u64::from(*n)).sum::<u64>(), "digest":format!("{:016x}",digest.finish())})
}

#[test]
#[ignore = "needs OTZARIA_PERF_DB/INDEX/OUT; run alone in release"]
fn measure_corpus() {
    let config = Config::read();
    match config.mode.as_str() {
        "build" => {
            assert!(
                !config.index.join("meta.json").exists(),
                "benchmark build needs a new index"
            );
            let mut engine = config.engine();
            let books = config.books(false);
            let order: Vec<_> = books
                .iter()
                .enumerate()
                .map(|(i, book)| BookOrderEntry {
                    file_path: book.file_path.clone(),
                    catalogue_order: i as u32,
                    generation_order: i as u32,
                })
                .collect();
            engine.set_book_order(order).unwrap();
            engine.set_bulk_indexing(true).unwrap();
            engine.open_index_writer().unwrap();
            let started = Instant::now();
            let results = engine
                .add_library_books(books, &IndexingCancellationToken::new())
                .unwrap();
            for result in &results {
                assert!(result.error.is_none(), "{:?}", result);
            }
            let added_ms = started.elapsed().as_secs_f64() * 1000.0;
            engine.commit().unwrap();
            engine.set_bulk_indexing(false).unwrap();
            engine.optimize().unwrap();
            config.record(serde_json::json!({ "kind":"build", "books":results.len(),
                "docs":engine.get_document_count(), "add_ms":added_ms,
                "total_ms":started.elapsed().as_secs_f64()*1000.0, "segments":engine.get_segment_count().unwrap() }));
            config.record_indexing();
        }
        "search" => {
            let engine = config.engine();
            for case in cases() {
                config.record(stream_sample(&engine, case));
                for _ in 1..config.samples {
                    config.record(stream_sample(&engine, case));
                }
            }
        }
        "strategy" => {
            let engine = config.engine();
            for fuzzy in [false, true] {
                for case in cases().into_iter().filter(|case| case.group == 0) {
                    for n in 0..config.samples {
                        for split in if n % 2 == 0 {
                            [false, true]
                        } else {
                            [true, false]
                        } {
                            let chosen = std::env::var("OTZARIA_PERF_STRATEGY").unwrap_or_default();
                            if chosen == "split" && !split || chosen == "combined" && split {
                                continue;
                            }
                            config.record(strategy_sample(&engine, case, split, fuzzy));
                        }
                    }
                }
            }
        }
        "index" | "mixed" => {
            let mut engine = config.engine();
            if std::env::var_os("OTZARIA_PERF_ECONOMY").is_some() {
                engine.set_economy_indexing(true).unwrap();
            }
            engine.set_bulk_indexing(true).unwrap();
            engine.open_index_writer().unwrap();
            let books = config.books(true);
            let started = Instant::now();
            std::thread::scope(|scope| {
                let shared = &engine;
                let indexing = scope.spawn(move || {
                    shared
                        .add_library_books(books, &IndexingCancellationToken::new())
                        .unwrap()
                });
                if config.mode == "mixed" {
                    let all_cases = cases();
                    let mut n = 0;
                    while !indexing.is_finished() {
                        config.record(stream_sample(shared, all_cases[n % 4]));
                        n += 1;
                    }
                }
                let results = indexing.join().unwrap();
                for result in &results {
                    assert!(result.error.is_none(), "{:?}", result);
                }
                config.record(serde_json::json!({ "kind":"index", "books":results.len(),
                    "docs":results.iter().map(|r|u64::from(r.docs)).sum::<u64>(),
                    "total_ms":started.elapsed().as_secs_f64()*1000.0 }));
            });
            engine.rollback().unwrap();
            config.record_indexing();
        }
        mode => panic!("unknown benchmark mode {mode}"),
    }
}

#[test]
#[ignore = "generated SQLite fixtures; run alone in release"]
fn measure_preparation_modes() {
    let _guard = crate::external_text_tests::guard();
    let root = tempfile::tempdir().unwrap();
    for (name, book_count, lines, order) in [
        (
            "medium",
            4usize,
            12_000usize,
            vec![false, true, true, false, false, true],
        ),
        ("large", 1, 80_000, vec![true, false, false, true]),
    ] {
        let path = root.path().join(format!("{name}.db"));
        let inputs =
            crate::library_indexing_tests::preparation_budget_fixture(&path, book_count, lines);
        let mut expected = None;
        for (run, economy) in order.into_iter().enumerate() {
            let index = root.path().join(format!("{name}-{run}"));
            std::fs::create_dir(&index).unwrap();
            configure_line_source(path.to_str().unwrap().into()).unwrap();
            let mut engine = SearchEngine::new(index.to_str().unwrap());
            engine.set_bulk_indexing(true).unwrap();
            engine.set_economy_indexing(economy).unwrap();
            crate::indexing_diagnostics::reset();
            let started = Instant::now();
            let results = engine
                .add_library_books(inputs.clone(), &IndexingCancellationToken::new())
                .unwrap();
            let add_ms = started.elapsed().as_secs_f64() * 1000.0;
            engine.commit().unwrap();
            let total_ms = started.elapsed().as_secs_f64() * 1000.0;
            let metrics = crate::indexing_diagnostics::snapshot();
            if name == "medium" && !economy {
                assert_eq!(metrics.spilled_bytes, 0);
            }
            assert!(results
                .iter()
                .all(|result| result.status
                    == crate::api::library_indexing::LibraryBookStatus::Indexed));
            assert_eq!(engine.get_document_count(), (book_count * lines) as u64);
            let stamps: Vec<_> = results
                .iter()
                .map(|result| {
                    (
                        result.docs,
                        result.content_hash,
                        result.text_hash,
                        result.metadata_hash,
                    )
                })
                .collect();
            let search = stream_sample(
                &engine,
                Case {
                    query: "שורה",
                    relevance: false,
                    group: 0,
                    offset: 0,
                },
            );
            let parity = (stamps, search["digest"].clone(), search["total"].clone());
            if let Some(expected) = &expected {
                assert_eq!(&parity, expected);
            } else {
                expected = Some(parity);
            }
            println!(
                "{}",
                serde_json::json!({"fixture":name,"economy":economy,"run":run,"books":book_count,"docs":book_count*lines,"add_ms":add_ms,"total_ms":total_ms,"spilled_bytes":metrics.spilled_bytes,"peak_buffered_bytes":metrics.peak_buffered_bytes,"search_digest":search["digest"]})
            );
        }
    }
}
