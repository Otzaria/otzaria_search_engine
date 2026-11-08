//! Isolated public-API renderer scheduling diagnostic; use identical bytes on H and E.
//! The wide cases are idle searches after indexing. The separate, unchanged
//! indexing_throughput target retains the original concurrent-indexing workload.

use search_engine::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource, LibraryBookStatus,
};
use search_engine::api::search_engine::{
    check_index_compatibility, configure_line_source, normalize_text_for_indexing,
    set_vocalized_snippets, BookOrderEntry, MergedSibling, ResultsOrder, SearchEngine,
    SearchResult, TextStatus, TextStorage,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Instant;

const QUERY: &str = "מחטמדידה";
const HIT_COUNTS: [u32; 8] = [1, 7, 8, 10, 15, 16, 50, 100];
const PROFILES: [&str; 3] = ["short", "long", "mixed"];
const WARMUP_QUERIES: usize = 5;
const MEASURED_QUERIES: usize = 30;
const PERCENTILE_RULE: &str = "sorted[ceil((n-1)*p)]";

struct VocalizedDisplayGuard;

impl VocalizedDisplayGuard {
    fn enable() -> Self {
        set_vocalized_snippets(true);
        Self
    }
}

impl Drop for VocalizedDisplayGuard {
    fn drop(&mut self) {
        set_vocalized_snippets(false);
    }
}

struct Row {
    source: String,
    plain_bytes: usize,
    vocalized_bytes: usize,
}

impl Row {
    fn plain(source: String) -> Self {
        Self {
            plain_bytes: source.len(),
            source,
            vocalized_bytes: 0,
        }
    }
}

struct Fixture {
    // Drop the engine before removing its directory.
    engine: SearchEngine,
    _root: tempfile::TempDir,
    rows: Vec<Row>,
    book_number: u32,
    metadata: Value,
}

impl Fixture {
    fn new(label: &str, storage: TextStorage, rows: Vec<Row>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let database = root.path().join("source.db");
        let mut connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE line(id INTEGER PRIMARY KEY,bookId INTEGER,lineIndex INTEGER,content TEXT);
                 CREATE INDEX idx_line_book_index ON line(bookId,lineIndex);",
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();
        let mut source_hasher = Sha256::new();
        {
            let mut insert = transaction
                .prepare("INSERT INTO line VALUES(?1,1,?2,?3)")
                .unwrap();
            for (ordinal, row) in rows.iter().enumerate() {
                assert!(!row.source.contains('\n'));
                assert_eq!(row.source.trim(), row.source);
                assert!(!row.source.contains("  "));
                assert_eq!(
                    normalize_text_for_indexing(row.source.clone()).len(),
                    row.plain_bytes,
                    "fixture byte estimate must match public ingestion normalization"
                );
                source_hasher.update((row.source.len() as u64).to_le_bytes());
                source_hasher.update(row.source.as_bytes());
                insert
                    .execute(rusqlite::params![ordinal + 1, ordinal, &row.source])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        drop(connection);
        configure_line_source(database.to_string_lossy().into_owned()).unwrap();
        let index = root.path().join("index");
        std::fs::create_dir(&index).unwrap();
        let mut engine = SearchEngine::new(index.to_str().unwrap());
        engine.set_economy_indexing(true).unwrap();
        engine.set_bulk_indexing(true).unwrap();
        engine.open_index_writer().unwrap();
        let mut indexed = engine
            .add_library_books(
                vec![LibraryBookInput {
                    source: LibraryBookSource::Library,
                    attached_slug: None,
                    book_id: 1,
                    db_title: None,
                    title: format!("render scheduling {label}"),
                    topics: "/benchmark/render-scheduling".into(),
                    file_path: "id:1".into(),
                    extra_facets: None,
                    text_storage: storage,
                    source_hash: Some(1373),
                }],
                &IndexingCancellationToken::new(),
            )
            .unwrap();
        assert_eq!(indexed.len(), 1);
        let book = indexed.remove(0);
        assert_eq!(book.status, LibraryBookStatus::Indexed);
        assert_eq!(
            book.text_storage, storage,
            "line-source fallback changes the case"
        );
        assert_eq!(book.docs as usize, rows.len());
        engine.commit().unwrap();
        assert_eq!(engine.get_document_count(), rows.len() as u64);
        engine
            .set_book_order(vec![BookOrderEntry {
                file_path: "id:1".into(),
                catalogue_order: 0,
                generation_order: 0,
            }])
            .unwrap();
        let metadata = json!({
            "label": label,
            "storage": storage_name(storage),
            "indexed_docs": rows.len(),
            "source_utf8_bytes": rows.iter().map(|row| row.source.len()).collect::<Vec<_>>(),
            "plain_utf8_bytes": rows.iter().map(|row| row.plain_bytes).collect::<Vec<_>>(),
            "vocalized_utf8_bytes": rows.iter().map(|row| row.vocalized_bytes).collect::<Vec<_>>(),
            "plain_byte_counts_verified_via_public_normalizer": true,
            "source_rows_sha256": format!("{:x}", source_hasher.finalize()),
            "source_hash_rule": "each row: little-endian u64 byte length, then UTF-8 source bytes",
            "schema_version": check_index_compatibility(index.to_string_lossy().into_owned()).found_schema_version,
            "book_number": book.book_number,
            "book_stamps": [book.docs as u64, book.content_hash, book.text_hash, book.metadata_hash, book.source_hash],
        });
        Self {
            engine,
            _root: root,
            rows,
            book_number: book.book_number,
            metadata,
        }
    }
}

fn storage_name(storage: TextStorage) -> &'static str {
    match storage {
        TextStorage::LibraryDb => "LibraryDb",
        TextStorage::InIndex => "InIndex",
    }
}

fn status_name(status: TextStatus) -> &'static str {
    match status {
        TextStatus::Ok => "Ok",
        TextStatus::Stale => "Stale",
        TextStatus::Unavailable => "Unavailable",
    }
}

fn fill_to_bytes(prefix: &str, bytes: usize) -> String {
    // These inputs have no HTML, presentation forms or attached marks, and only
    // single spaces. Ingestion preserves their UTF-8 byte lengths.
    const FILLER: &str = "מילוי ";
    assert!(prefix.ends_with(' ') && prefix.len() < bytes);
    let mut text = String::with_capacity(bytes);
    text.push_str(prefix);
    while text.len() + FILLER.len() < bytes {
        text.push_str(FILLER);
    }
    text.extend(std::iter::repeat_n('x', bytes - text.len()));
    assert_eq!(text.len(), bytes);
    text
}

fn plain_row(ordinal: usize, bytes: usize) -> Row {
    Row::plain(fill_to_bytes(&format!("{QUERY} שורה {ordinal:03} "), bytes))
}

fn profile_rows(profile: &str) -> Vec<Row> {
    (0..100)
        .map(|ordinal| {
            let bytes = match profile {
                "short" => 1024,
                "long" => 64 * 1024,
                "mixed" => [1024, 32 * 1024, 1024, 64 * 1024][ordinal % 4],
                _ => panic!("unknown fixed profile"),
            };
            plain_row(ordinal, bytes)
        })
        .collect()
}

fn search(engine: &SearchEngine, query: &str, limit: u32, match_nikud: bool) -> Vec<SearchResult> {
    engine
        .search_exact(
            query.into(),
            vec![],
            limit,
            0,
            ResultsOrder::Catalogue,
            match_nikud,
            false,
            None,
            None,
        )
        .unwrap()
}

fn sibling_snapshot(sibling: &MergedSibling) -> Value {
    json!({
        "title": &sibling.title,
        "reference": &sibling.reference,
        "id": sibling.id,
        "segment": sibling.segment,
        "is_pdf": sibling.is_pdf,
        "file_path": &sibling.file_path,
    })
}

fn snapshot(results: &[SearchResult]) -> Vec<Value> {
    results
        .iter()
        .map(|hit| {
            json!({
                "title": &hit.title,
                "reference": &hit.reference,
                "text": &hit.text,
                "id": hit.id,
                "segment": hit.segment,
                "is_pdf": hit.is_pdf,
                "file_path": &hit.file_path,
                "merged_count": hit.merged_count,
                "merged": hit.merged.iter().map(sibling_snapshot).collect::<Vec<_>>(),
                "text_status": status_name(hit.text_status),
                "continues_to_next_line": hit.continues_to_next_line,
            })
        })
        .collect()
}

fn check_results(fixture: &Fixture, hits: &[SearchResult], segments: &[u64], cross_line: bool) {
    assert_eq!(hits.len(), segments.len());
    for (hit, &segment) in hits.iter().zip(segments) {
        assert_eq!(hit.segment, segment, "catalogue line order changed");
        assert_eq!(hit.id, ((fixture.book_number as u64) << 32) + segment + 1);
        assert_eq!(hit.file_path, "id:1");
        assert!(!hit.is_pdf);
        assert_eq!(hit.text_status, TextStatus::Ok);
        assert_eq!(hit.merged_count, 1);
        assert!(hit.merged.is_empty());
        assert_eq!(hit.continues_to_next_line, cross_line);
        assert!(hit.text.contains("<font color=red>"), "missing highlight");
        if cross_line {
            assert!(hit.text.contains("<br>"), "missing following-line display");
        }
    }
}

fn checked_record(
    output: &Path,
    fixture: &Fixture,
    storage: TextStorage,
    profile: &str,
    case_id: String,
    query: &str,
    match_nikud: bool,
    vocalized_display: bool,
    hits: &[SearchResult],
    requested_hits: u32,
    timed: bool,
    phase: &str,
) -> Value {
    let snapshot_file = format!("{case_id}.json");
    let mut bytes = serde_json::to_vec(&snapshot(hits)).unwrap();
    bytes.push(b'\n');
    std::fs::write(output.join(&snapshot_file), bytes).unwrap();
    let mut line_bytes = Vec::with_capacity(hits.len());
    let mut vocalized_line_bytes = Vec::with_capacity(hits.len());
    let mut next_line_bytes = Vec::with_capacity(hits.len());
    for hit in hits {
        let ordinal = usize::try_from(hit.segment).unwrap();
        line_bytes.push(fixture.rows[ordinal].plain_bytes);
        vocalized_line_bytes.push(fixture.rows[ordinal].vocalized_bytes);
        next_line_bytes.push(if hit.continues_to_next_line {
            fixture.rows[ordinal + 1].plain_bytes
        } else {
            0
        });
    }
    let total_line_bytes: usize = line_bytes.iter().sum();
    let total_vocalized_bytes: usize = vocalized_line_bytes.iter().sum();
    let total_next_line_bytes: usize = next_line_bytes.iter().sum();
    json!({
        "schema_version": 1,
        "case_id": case_id,
        "phase": phase,
        "storage": storage_name(storage),
        "profile": profile,
        "query": query,
        "match_nikud": match_nikud,
        "vocalized_display": vocalized_display,
        "requested_hits": requested_hits,
        "result_count": hits.len(),
        "timed": timed,
        "indexing_overlapped": false,
        "warmup_queries": if timed { WARMUP_QUERIES } else { 0 },
        "measured_queries": if timed { MEASURED_QUERIES } else { 0 },
        "post_timing_validation_queries": if timed { 1 } else { 0 },
        "post_timing_snapshot_verified": false,
        "latency_ms": [],
        "p50_ms": null, "p95_ms": null, "p99_ms": null,
        "percentile_rule": PERCENTILE_RULE,
        "snapshot_file": snapshot_file,
        "line_bytes": line_bytes,
        "vocalized_line_bytes": vocalized_line_bytes,
        "next_line_bytes": next_line_bytes,
        "total_line_bytes": total_line_bytes,
        "total_vocalized_bytes": total_vocalized_bytes,
        "total_next_line_bytes": total_next_line_bytes,
        "total_render_bytes": total_line_bytes + total_vocalized_bytes + total_next_line_bytes,
        "text_statuses": hits.iter().map(|hit| status_name(hit.text_status)).collect::<Vec<_>>(),
        "highlighted_results": hits.iter().filter(|hit| hit.text.contains("<font color=red>")).count(),
        "cross_line_results": hits.iter().filter(|hit| hit.continues_to_next_line).count(),
        "fixture": &fixture.metadata,
    })
}

fn percentile(samples: &[f64], fraction: f64) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * fraction).ceil() as usize]
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

fn measure(fixture: &Fixture, requested_hits: u32, record: &mut Value) {
    for _ in 0..WARMUP_QUERIES {
        drop(std::hint::black_box(search(
            &fixture.engine,
            QUERY,
            requested_hits,
            false,
        )));
    }
    let mut latencies = Vec::with_capacity(MEASURED_QUERIES);
    for _ in 0..MEASURED_QUERIES {
        let started = Instant::now();
        let results = search(&fixture.engine, QUERY, requested_hits, false);
        let milliseconds = started.elapsed().as_secs_f64() * 1000.0;
        // Consumption/drop, validation, vector insertion and serialization stay
        // outside the measured search call. black_box retains every result field.
        drop(std::hint::black_box(results));
        latencies.push(milliseconds);
    }
    record["p50_ms"] = json!(percentile(&latencies, 0.50));
    record["p95_ms"] = json!(percentile(&latencies, 0.95));
    record["p99_ms"] = json!(percentile(&latencies, 0.99));
    record["latency_ms"] = json!(latencies);
}

fn is_timed(profile: &str, hits: u32) -> bool {
    match profile {
        "short" => matches!(hits, 1 | 10 | 50 | 100),
        "long" => matches!(hits, 10 | 100),
        "mixed" => hits == 10,
        _ => false,
    }
}

fn preflight(output: &Path, storage: TextStorage, kind: usize) -> Value {
    let (profile, id, query, match_nikud, rows, segments, cross_line) = match kind {
        0..=2 => {
            let total = 16383 + kind;
            let mut rows: Vec<_> = (0..9).map(|ordinal| plain_row(ordinal, 1024)).collect();
            rows.push(plain_row(9, total - 9 * 1024));
            assert_eq!(rows.iter().map(|row| row.plain_bytes).sum::<usize>(), total);
            (
                "byte-boundary",
                format!("boundary-{total}-hits-010"),
                QUERY,
                false,
                rows,
                (0..10).collect::<Vec<u64>>(),
                false,
            )
        }
        3 => {
            let rows = (0..2)
                .map(|ordinal| {
                    let mut plain = format!("שלום שורה {ordinal:03} ");
                    let mut vocalized = format!("שָׁלוֹם שורה {ordinal:03} ");
                    for _ in 0..400 {
                        plain.push_str("מילוי ");
                        vocalized.push_str("מִילוּי ");
                    }
                    plain.push_str("סוף");
                    vocalized.push_str("סוף");
                    Row {
                        plain_bytes: plain.len(),
                        vocalized_bytes: vocalized.len(),
                        source: vocalized,
                    }
                })
                .collect::<Vec<_>>();
            let plain: usize = rows.iter().map(|row| row.plain_bytes).sum();
            let both: usize = rows
                .iter()
                .map(|row| row.plain_bytes + row.vocalized_bytes)
                .sum();
            assert!(plain <= 16 * 1024 && both > 16 * 1024);
            (
                "vocalized-budget",
                "vocalized-budget-hits-002".into(),
                "שלום",
                false,
                rows,
                vec![0, 1],
                false,
            )
        }
        4 => (
            "cross-line-long-neighbor",
            "cross-line-long-neighbor-hits-002".into(),
            "פתיחה סיום",
            false,
            vec![
                Row::plain("גוף פתיחה".into()),
                Row::plain(fill_to_bytes("סיום ", 64 * 1024)),
                Row::plain("עוד פתיחה".into()),
                Row::plain(fill_to_bytes("סיום ", 64 * 1024)),
            ],
            vec![0, 2],
            true,
        ),
        _ => unreachable!(),
    };
    let fixture = Fixture::new(&id, storage, rows);
    // PlainAndVocalized retains both texts for LibraryDb as well as InIndex.
    // A vocalized-field query would discard plain text in the LibraryDb path.
    let _vocalized_display = (kind == 3).then(VocalizedDisplayGuard::enable);
    let all = search(&fixture.engine, query, 100, match_nikud);
    check_results(&fixture, &all, &segments, cross_line);
    let requested_hits = segments.len() as u32;
    let page = search(&fixture.engine, query, requested_hits, match_nikud);
    check_results(&fixture, &page, &segments, cross_line);
    assert_eq!(snapshot(&page), snapshot(&all));
    if kind == 3 {
        assert!(page.iter().all(|hit| hit.text.contains("שָׁלוֹם")));
    }
    let record = checked_record(
        output,
        &fixture,
        storage,
        profile,
        format!("{}-{id}", storage_name(storage)),
        query,
        match_nikud,
        kind == 3,
        &page,
        requested_hits,
        false,
        "preflight",
    );
    if cross_line {
        assert_eq!(record["next_line_bytes"], json!([65536, 65536]));
        assert!(record["total_line_bytes"].as_u64().unwrap() < 16 * 1024);
    }
    record
}

#[test]
#[ignore = "fixed public-API render scheduling diagnostic; run alone in release"]
fn measure_render_scheduling() {
    let started = Instant::now();
    let rss_before_kib = process_memory_kib("VmRSS:");
    set_vocalized_snippets(false);
    let order = std::env::var("OTZARIA_RENDER_CASE_ORDER").unwrap_or_else(|_| "forward".into());
    assert!(matches!(order.as_str(), "forward" | "reverse"));
    let reverse = order == "reverse";
    let output = PathBuf::from(
        std::env::var_os("OTZARIA_RENDER_OUTPUT_DIR")
            .expect("OTZARIA_RENDER_OUTPUT_DIR is required"),
    );
    std::fs::create_dir_all(&output).unwrap();
    assert!(
        std::fs::read_dir(&output).unwrap().next().is_none(),
        "snapshot directory must be empty"
    );
    let storages = [TextStorage::LibraryDb, TextStorage::InIndex];
    let mut preflight_cases: Vec<_> = storages
        .into_iter()
        .flat_map(|storage| (0..5).map(move |kind| (storage, kind)))
        .collect();
    let mut groups: Vec<_> = storages
        .into_iter()
        .flat_map(|storage| PROFILES.into_iter().map(move |profile| (storage, profile)))
        .collect();
    let mut counts = HIT_COUNTS;
    if reverse {
        preflight_cases.reverse();
        groups.reverse();
        counts.reverse();
    }
    let mut ordered_case_ids = Vec::new();
    let mut timed_case_ids = Vec::new();
    let mut behavior_case_ids = Vec::new();
    for (storage, kind) in preflight_cases {
        let record = preflight(&output, storage, kind);
        let case_id = record["case_id"].as_str().unwrap().to_owned();
        ordered_case_ids.push(case_id.clone());
        behavior_case_ids.push(case_id);
        println!("OTZARIA_RENDER_CASE {record}");
    }
    for (storage, profile) in groups {
        let fixture = Fixture::new(profile, storage, profile_rows(profile));
        let full = search(&fixture.engine, QUERY, 100, false);
        check_results(&fixture, &full, &(0..100).collect::<Vec<u64>>(), false);
        let full_snapshot = snapshot(&full);
        let mut checked = Vec::new();
        // Check and write every page snapshot before timing any page of this fixture.
        for requested_hits in counts {
            let page = search(&fixture.engine, QUERY, requested_hits, false);
            check_results(
                &fixture,
                &page,
                &(0..requested_hits as u64).collect::<Vec<_>>(),
                false,
            );
            assert_eq!(
                snapshot(&page).as_slice(),
                &full_snapshot[..requested_hits as usize]
            );
            let case_id = format!(
                "{}-{profile}-hits-{requested_hits:03}",
                storage_name(storage)
            );
            let record = checked_record(
                &output,
                &fixture,
                storage,
                profile,
                case_id,
                QUERY,
                false,
                false,
                &page,
                requested_hits,
                is_timed(profile, requested_hits),
                "standard",
            );
            checked.push((requested_hits, record));
        }
        for (requested_hits, mut record) in checked {
            let case_id = record["case_id"].as_str().unwrap().to_owned();
            if record["timed"].as_bool().unwrap() {
                measure(&fixture, requested_hits, &mut record);
                let hot = search(&fixture.engine, QUERY, requested_hits, false);
                check_results(
                    &fixture,
                    &hot,
                    &(0..requested_hits as u64).collect::<Vec<_>>(),
                    false,
                );
                let mut hot_snapshot = serde_json::to_vec(&snapshot(&hot)).unwrap();
                hot_snapshot.push(b'\n');
                assert_eq!(
                    hot_snapshot,
                    std::fs::read(output.join(record["snapshot_file"].as_str().unwrap())).unwrap(),
                    "hot-cache output changed after the timed samples"
                );
                record["post_timing_snapshot_verified"] = json!(true);
                timed_case_ids.push(case_id.clone());
            } else {
                behavior_case_ids.push(case_id.clone());
            }
            ordered_case_ids.push(case_id);
            println!("OTZARIA_RENDER_CASE {record}");
        }
    }
    assert_eq!(ordered_case_ids.len(), 58);
    assert_eq!(timed_case_ids.len(), 14);
    assert_eq!(behavior_case_ids.len(), 44);
    println!(
        "OTZARIA_RENDER_COMPLETE {}",
        json!({
            "schema_version": 1,
            "fixture": "render-scheduling-v1",
            "fixture_source_sha256": format!("{:x}", Sha256::digest(include_bytes!("render_scheduling.rs"))),
            "case_order": order,
            "total_cases": 58,
            "completed_timed_cases": 14,
            "completed_behavior_cases": 44,
            "warmup_queries": WARMUP_QUERIES,
            "measured_queries": MEASURED_QUERIES,
            "post_timing_validation_queries_per_timed_case": 1,
            "percentile_rule": PERCENTILE_RULE,
            "ordered_case_ids": ordered_case_ids,
            "timed_case_ids": timed_case_ids,
            "behavior_case_ids": behavior_case_ids,
            "cpus": std::thread::available_parallelism().map(|count| count.get()).ok(),
            "indexing_overlapped": false,
            "default_vocalized_display": false,
            "rss_before_kib": rss_before_kib,
            "peak_rss_kib": process_memory_kib("VmHWM:"),
            "memory_scope": "whole process, including fixture setup, indexing, checks and all search cases",
            "timing_scope": "full idle search_exact after indexing; validation, serialization and result drop excluded",
            "profile_line_bytes": {"short": [1024], "long": [65536], "mixed_cycle": [1024,32768,1024,65536]},
            "elapsed_seconds": started.elapsed().as_secs_f64(),
        })
    );
}
