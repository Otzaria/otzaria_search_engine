//! Opt-in public-API indexing benchmark, also compilable unchanged on the pre-PR base.

use search_engine::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource, LibraryBookStatus,
};
use search_engine::api::search_engine::{configure_line_source, SearchEngine, TextStorage};
use std::time::Instant;

fn fixture(path: &std::path::Path, books: usize, lines: usize) -> Vec<LibraryBookInput> {
    let mut connection = rusqlite::Connection::open(path).unwrap();
    connection.execute_batch("CREATE TABLE line(id INTEGER PRIMARY KEY,bookId INTEGER,lineIndex INTEGER,content TEXT); CREATE INDEX idx_line_book_index ON line(bookId,lineIndex);").unwrap();
    let transaction = connection.transaction().unwrap();
    let text = "שורה עם טקסט מקורי ארוך מספיק לבדיקת הכנת מסמכים וגבולות התקציב ".repeat(8);
    {
        let mut insert = transaction
            .prepare("INSERT INTO line VALUES(?1,?2,?3,?4)")
            .unwrap();
        for book in 0..books {
            for ordinal in 0..lines {
                let content = match ordinal % 257 {
                    0 => format!("<h2>פרק {ordinal}</h2>"),
                    1 => "קטן מאד".into(),
                    _ => format!("{text} {ordinal}"),
                };
                insert
                    .execute(rusqlite::params![
                        book * lines + ordinal + 1,
                        book + 1,
                        ordinal,
                        content
                    ])
                    .unwrap();
            }
        }
    }
    transaction.commit().unwrap();
    (0..books)
        .map(|book| LibraryBookInput {
            source: LibraryBookSource::Library,
            attached_slug: None,
            book_id: (book + 1) as i64,
            db_title: None,
            title: "בדיקת תקציב".into(),
            topics: format!("/ספרייה/{}", book + 1),
            file_path: format!("id:{}", book + 1),
            extra_facets: Some(vec!["/author/x".into()]),
            text_storage: TextStorage::LibraryDb,
            source_hash: Some(71),
        })
        .collect()
}

#[test]
#[ignore = "synthetic throughput comparison; run alone in release"]
fn measure_indexing_throughput() {
    let economy = std::env::var_os("OTZARIA_BENCH_ECONOMY").is_some();
    for (name, books, lines) in [("medium", 4, 12_000), ("large", 1, 80_000)] {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("source.db");
        let inputs = fixture(&database, books, lines);
        let index = root.path().join("index");
        std::fs::create_dir(&index).unwrap();
        configure_line_source(database.to_str().unwrap().into()).unwrap();
        let mut engine = SearchEngine::new(index.to_str().unwrap());
        engine.set_bulk_indexing(true).unwrap();
        engine.set_economy_indexing(economy).unwrap();
        engine.open_index_writer().unwrap();
        let started = Instant::now();
        let results = engine
            .add_library_books(inputs, &IndexingCancellationToken::new())
            .unwrap();
        let add_ms = started.elapsed().as_secs_f64() * 1000.0;
        engine.commit().unwrap();
        let committed_ms = started.elapsed().as_secs_f64() * 1000.0;
        engine.optimize().unwrap();
        let total_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(engine.get_document_count(), (books * lines) as u64);
        assert!(results
            .iter()
            .all(|result| result.status == LibraryBookStatus::Indexed));
        let stamps: Vec<_> = results
            .iter()
            .map(|result| {
                (
                    result.docs,
                    result.content_hash,
                    result.text_hash,
                    result.metadata_hash,
                    result.source_hash,
                )
            })
            .collect();
        println!(
            "OTZARIA_BENCH {}",
            serde_json::json!({"fixture":name,"economy":economy,"books":books,"docs":books*lines,"add_ms":add_ms,"commit_ms":committed_ms,"total_ms":total_ms,"stamps":stamps})
        );
    }
}

fn percentile(samples: &mut [f64], percentile: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(f64::total_cmp);
    Some(samples[((samples.len() - 1) as f64 * percentile).ceil() as usize])
}

fn process_memory_kib(field: &str) -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| {
            line.strip_prefix(field)?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
}

#[test]
#[ignore = "concurrent search/indexing comparison; run alone in release on each revision"]
fn measure_search_during_indexing() {
    use search_engine::api::search_engine::ResultsOrder;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let lines = std::env::var("OTZARIA_BENCH_LINES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3_000);
    let economy = std::env::var_os("OTZARIA_BENCH_ECONOMY").is_some();
    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("source.db");
    let mut inputs = fixture(&database, 9, lines);
    let index = root.path().join("index");
    std::fs::create_dir(&index).unwrap();
    configure_line_source(database.to_str().unwrap().into()).unwrap();
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    engine.set_economy_indexing(economy).unwrap();
    engine.set_bulk_indexing(true).unwrap();
    engine.open_index_writer().unwrap();
    let seed = inputs.remove(0);
    engine
        .add_library_books(vec![seed], &IndexingCancellationToken::new())
        .unwrap();
    engine.commit().unwrap();
    let search = || {
        engine
            .search_exact(
                "טקסט".into(),
                vec![],
                10,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
                None,
            )
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect::<Vec<_>>()
    };
    let expected = search();
    assert_eq!(expected.len(), 10);
    let mut idle_samples = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        assert_eq!(search(), expected);
        idle_samples.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    search_engine::indexing_diagnostics::reset();
    let rss_before_kib = process_memory_kib("VmRSS:");
    let done = AtomicBool::new(false);
    let mut search_samples = Vec::new();
    let started = Instant::now();
    let results = std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let result = engine.add_library_books(inputs, &IndexingCancellationToken::new());
            done.store(true, Ordering::Release);
            result.unwrap()
        });
        while !done.load(Ordering::Acquire) {
            let started = Instant::now();
            assert_eq!(
                search(),
                expected,
                "committed search snapshot changed during indexing"
            );
            search_samples.push(started.elapsed().as_secs_f64() * 1000.0);
            std::thread::sleep(Duration::from_millis(10));
        }
        worker.join().unwrap()
    });
    let add_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert!(
        !search_samples.is_empty(),
        "no search overlapped the fixture; increase OTZARIA_BENCH_LINES"
    );
    assert!(results
        .iter()
        .all(|result| result.status == LibraryBookStatus::Indexed));
    engine.commit().unwrap();
    let total_ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(engine.get_document_count(), (9 * lines) as u64);
    let metrics = search_engine::indexing_diagnostics::snapshot();
    let stamps: Vec<_> = results
        .iter()
        .map(|r| (r.docs, r.content_hash, r.text_hash, r.metadata_hash))
        .collect();
    println!(
        "OTZARIA_RESPONSIVENESS {}",
        serde_json::json!({
            "query_workload": "repeated exact query with warm search caches",
            "economy": economy, "cpus": std::thread::available_parallelism().map(|n|n.get()).ok(),
            "indexed_docs":8*lines, "search_samples":search_samples.len(),
            "search_idle_p95_ms":percentile(&mut idle_samples, 0.95),
            "search_during_p50_ms":percentile(&mut search_samples, 0.50),
            "search_during_p95_ms":percentile(&mut search_samples, 0.95),
            "search_during_p99_ms":percentile(&mut search_samples, 0.99),
            "add_ms":add_ms, "total_ms":total_ms, "docs_per_sec":(8*lines) as f64 / (total_ms/1000.0),
            "rss_before_kib":rss_before_kib, "peak_rss_kib":process_memory_kib("VmHWM:"),
            "peak_buffered_bytes":metrics.peak_buffered_bytes,"spilled_bytes":metrics.spilled_bytes,
            "diagnostics":format!("{metrics:?}"), "stamps": stamps,
        })
    );
}
