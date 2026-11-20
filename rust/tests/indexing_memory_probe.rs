//! Opt-in probe: peak heap of a library build and of the `optimize` that ends it.
//! OTZARIA_PROBE_BOOKS / _LINES / _COMMIT_BOOKS size the run; OTZARIA_PROBE_BULK=1 skips
//! background merges like the release builder does, unset mimics the app's build.

use search_engine::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource,
};
use search_engine::api::search_engine::{configure_line_source, SearchEngine, TextStorage};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

struct Counting;
static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = System.alloc(layout);
        if !pointer.is_null() {
            let now = CURRENT.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        System.dealloc(pointer, layout)
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let moved = System.realloc(pointer, layout, size);
        if !moved.is_null() {
            if size >= layout.size() {
                let now = CURRENT.fetch_add(size - layout.size(), Ordering::Relaxed)
                    + (size - layout.size());
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                CURRENT.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Peak commit charge and peak working set of the whole process, in bytes: unlike the
/// counting allocator, these include SQLite's C heap and mapped index files.
#[cfg(windows)]
fn process_peaks() -> (usize, usize) {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set: usize,
        working_set: usize,
        quota_peak_paged_pool: usize,
        quota_paged_pool: usize,
        quota_peak_non_paged_pool: usize,
        quota_non_paged_pool: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Default::default()
    };
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (counters.peak_pagefile_usage, counters.peak_working_set)
}

#[cfg(not(windows))]
fn process_peaks() -> (usize, usize) {
    (0, 0)
}

fn mb(bytes: usize) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn phase_start() -> usize {
    let now = CURRENT.load(Ordering::Relaxed);
    PEAK.store(now, Ordering::Relaxed);
    now
}

fn env(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn fixture(path: &std::path::Path, books: usize, lines: usize) -> Vec<LibraryBookInput> {
    let mut connection = rusqlite::Connection::open(path).unwrap();
    connection.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; CREATE TABLE line(id INTEGER PRIMARY KEY,bookId INTEGER,lineIndex INTEGER,content TEXT); CREATE INDEX idx_line_book_index ON line(bookId,lineIndex);").unwrap();
    let letters: Vec<char> = "אבגדהוזחטיכלמנסעפצקרשת".chars().collect();
    // A Zipf-like vocabulary of a few hundred thousand words, so term dictionaries
    // and postings resemble a real library more than a repeated sentence does.
    let word = |seed: u64| -> String {
        let mut value = seed;
        let mut out = String::new();
        loop {
            out.push(letters[(value % letters.len() as u64) as usize]);
            value /= letters.len() as u64;
            if value == 0 || out.chars().count() >= 6 {
                break out;
            }
        }
    };
    let transaction = connection.transaction().unwrap();
    {
        let mut insert = transaction
            .prepare("INSERT INTO line VALUES(?1,?2,?3,?4)")
            .unwrap();
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        for book in 0..books {
            for ordinal in 0..lines {
                let mut content = String::new();
                for _ in 0..12 {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    let rank = (state % 400_000) * (state % 400_000) / 400_000;
                    content.push_str(&word(rank));
                    content.push(' ');
                }
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
            title: format!("ספר {book}"),
            topics: format!("/ספרייה/{}", book % 50),
            file_path: format!("id:{}", book + 1),
            extra_facets: Some(vec!["/author/x".into()]),
            text_storage: TextStorage::LibraryDb,
            source_hash: Some(71),
        })
        .collect()
}

/// The first `limit` books of a real `seforim.db`, opened read-only by the line source.
fn real_library(path: &std::path::Path, limit: usize) -> Vec<LibraryBookInput> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = connection
        .prepare("SELECT id, title, categoryId FROM book ORDER BY id LIMIT ?1")
        .unwrap();
    let rows = statement
        .query_map([limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap();
    rows.map(|row| {
        let (id, title, category) = row.unwrap();
        LibraryBookInput {
            source: LibraryBookSource::Library,
            attached_slug: None,
            book_id: id,
            db_title: None,
            title,
            topics: format!("/ספרייה/{category}"),
            file_path: format!("id:{id}"),
            extra_facets: None,
            text_storage: TextStorage::LibraryDb,
            source_hash: Some(71),
        }
    })
    .collect()
}

#[test]
#[ignore = "memory probe; run alone in release"]
fn probe_library_build_memory() {
    let books = env("OTZARIA_PROBE_BOOKS", 400);
    let lines = env("OTZARIA_PROBE_LINES", 2_500);
    let commit_books = env("OTZARIA_PROBE_COMMIT_BOOKS", 40);
    let bulk = std::env::var_os("OTZARIA_PROBE_BULK").is_some();
    let economy = std::env::var_os("OTZARIA_PROBE_ECONOMY").is_some();
    let root = tempfile::tempdir_in(
        std::env::var_os("OTZARIA_PROBE_DIR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(std::env::temp_dir),
    )
    .unwrap();
    let real = std::env::var_os("OTZARIA_PROBE_SOURCE_DB").map(std::path::PathBuf::from);
    let database = real
        .clone()
        .unwrap_or_else(|| root.path().join("source.db"));
    let inputs = match &real {
        Some(path) => real_library(path, books),
        None => fixture(&database, books, lines),
    };
    let expected_docs = match &real {
        Some(_) => None,
        None => Some((books * lines) as u64),
    };
    println!(
        "OTZARIA_PROBE fixture_peak_commit_mb={:.0}",
        mb(process_peaks().0)
    );
    let index = root.path().join("index");
    std::fs::create_dir(&index).unwrap();
    configure_line_source(database.to_str().unwrap().into()).unwrap();
    let mut engine = SearchEngine::new(index.to_str().unwrap());
    engine.set_bulk_indexing(bulk).unwrap();
    engine.set_economy_indexing(economy).unwrap();
    engine.open_index_writer().unwrap();

    let base = phase_start();
    let started = Instant::now();
    let mut inputs = inputs.into_iter().peekable();
    while inputs.peek().is_some() {
        let chunk: Vec<_> = inputs.by_ref().take(commit_books).collect();
        engine
            .add_library_books(chunk, &IndexingCancellationToken::new())
            .unwrap();
        engine.commit().unwrap();
    }
    let build_peak = PEAK.load(Ordering::Relaxed) - base;
    let build_ms = started.elapsed().as_millis();
    let segments_before = engine.get_segment_count().unwrap();

    let base = phase_start();
    let started = Instant::now();
    engine.optimize().unwrap();
    let optimize_peak = PEAK.load(Ordering::Relaxed) - base;
    let optimize_ms = started.elapsed().as_millis();
    let docs = engine.get_document_count();
    if let Some(expected) = expected_docs {
        assert_eq!(docs, expected);
    }
    println!(
        "OTZARIA_PROBE docs={} bulk={bulk} economy={economy} build_peak_mb={:.0} build_ms={build_ms} \
         segments={segments_before}->{} optimize_peak_mb={:.0} optimize_ms={optimize_ms} \
         optimize_bytes_per_doc={:.1} process_peak_commit_mb={:.0} process_peak_ws_mb={:.0}",
        docs,
        mb(build_peak),
        engine.get_segment_count().unwrap(),
        mb(optimize_peak),
        optimize_peak as f64 / docs as f64,
        mb(process_peaks().0),
        mb(process_peaks().1),
    );
}
