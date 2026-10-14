//! The indexing pipeline's shortcuts against the straightforward code they replace, on the
//! real library: every `OTZARIA_PARITY_STEP`-th book (default 20) of `OTZARIA_SEFORIM_DB`.

use crate::line_source::{LineStore, RowText};
use std::path::Path;

/// The rows of every sampled book, as the indexing path receives them.
pub(crate) fn sample_books() -> Option<Vec<(i64, Vec<String>)>> {
    let db = std::env::var("OTZARIA_SEFORIM_DB").ok()?;
    let step: usize = std::env::var("OTZARIA_PARITY_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let mut store = LineStore::open(Path::new(&db)).unwrap();
    let ids: Vec<i64> = {
        let conn = rusqlite::Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        let ids = conn
            .prepare("SELECT id FROM book WHERE totalLines > 0 ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .step_by(step)
            .collect();
        ids
    };
    let books = ids
        .into_iter()
        .filter_map(|id| {
            let rows = store.fetch_book(id).unwrap()?;
            let rows = rows
                .into_iter()
                .map(|row| match row {
                    RowText::Found(text) => text,
                    _ => String::new(),
                })
                .collect();
            Some((id, rows))
        })
        .collect();
    Some(books)
}

#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB pointing at a real seforim.db"]
fn real_library_line_edges_match_the_full_walk() {
    let Some(books) = sample_books() else {
        return;
    };
    let mut analyzer =
        tantivy::tokenizer::TextAnalyzer::builder(crate::hebrew_tokenizer::HebrewTokenizer {
            emit_quote_free: true,
            keep_marks: false,
        })
        .filter(tantivy::tokenizer::RemoveLongFilter::limit(128))
        .filter(tantivy::tokenizer::LowerCaser)
        .build();
    let mut lines = 0usize;
    for (id, rows) in &books {
        for row in rows {
            for raw in row.split('\n') {
                let plain = crate::hebrew_query::normalize_text_for_indexing(raw);
                assert_eq!(
                    crate::cross_line::line_edges(&mut analyzer, &plain),
                    crate::cross_line::line_edges_reference(&mut analyzer, &plain),
                    "book {id}: {plain:?}"
                );
                lines += 1;
            }
        }
    }
    println!("{} books, {lines} lines: line edges identical", books.len());
}

#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB pointing at a real seforim.db"]
fn real_library_one_pass_normalization_matches_the_separate_passes() {
    let Some(books) = sample_books() else {
        return;
    };
    let mut lines = 0usize;
    for (_, rows) in &books {
        for row in rows {
            for raw in row.split('\n') {
                crate::hebrew_query::assert_normalization_parity(raw);
                lines += 1;
            }
        }
    }
    println!(
        "{} books, {lines} lines: normalization identical",
        books.len()
    );
}

/// Single-thread time of the replaced passes against their replacements; run `--release`.
#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB; run with --release"]
fn real_library_prepare_passes_timing() {
    let Some(books) = sample_books() else {
        return;
    };
    let raw: Vec<String> = books
        .iter()
        .flat_map(|(_, rows)| rows.iter().flat_map(|row| row.split('\n')))
        .map(str::to_string)
        .collect();
    // This thread's CPU time, the least of three runs: other processes share the machine.
    let time = |f: &mut dyn FnMut()| {
        (0..3)
            .map(|_| {
                let started = thread_cpu_time();
                f();
                thread_cpu_time() - started
            })
            .min()
            .unwrap()
    };
    let mut sink = 0usize;
    let old_norm = time(&mut || {
        for line in &raw {
            let html = crate::hebrew_query::strip_html_for_indexing_reference(line);
            let plain = crate::hebrew_query::fold_strip_collapse(&html, true, |_| false);
            let vocalized = crate::hebrew_query::contains_attached_marks(line).then(|| {
                let html = crate::hebrew_query::strip_html_for_indexing_reference(line);
                crate::hebrew_query::fold_strip_collapse(&html, false, |_| false)
            });
            sink += plain.len() + vocalized.map_or(0, |v| v.len());
        }
    });
    let plain: Vec<String> = raw
        .iter()
        .map(|line| crate::hebrew_query::normalize_text_for_indexing(line))
        .collect();
    let new_norm = time(&mut || {
        for line in &raw {
            let (p, v) = crate::hebrew_query::normalize_line_for_indexing(line);
            sink += p.len() + v.map_or(0, |v| v.len());
        }
    });
    let mut analyzer =
        tantivy::tokenizer::TextAnalyzer::builder(crate::hebrew_tokenizer::HebrewTokenizer {
            emit_quote_free: true,
            keep_marks: false,
        })
        .filter(tantivy::tokenizer::RemoveLongFilter::limit(128))
        .filter(tantivy::tokenizer::LowerCaser)
        .build();
    let old_edges = time(&mut || {
        for line in &plain {
            sink += crate::cross_line::line_edges_reference(&mut analyzer, line).is_some() as usize;
        }
    });
    let new_edges = time(&mut || {
        for line in &plain {
            sink += crate::cross_line::line_edges(&mut analyzer, line).is_some() as usize;
        }
    });
    let books_lines = keyed_books(&books);
    let old_keys = time(&mut || {
        for lines in &books_lines {
            sink += crate::semantic_keys::sidecar_column_values(&line_refs(lines)).len();
        }
    });
    // One thread, as the others: the production call fans out over rayon.
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let new_keys = pool.install(|| {
        time(&mut || {
            for lines in &books_lines {
                sink += crate::semantic_keys::column_values(&line_refs(lines)).len();
            }
        })
    });
    let mb = raw.iter().map(String::len).sum::<usize>() as f64 / 1e6;
    println!(
        "{mb:.1} MB, {} lines: normalize {old_norm:?} -> {new_norm:?}; line edges {old_edges:?} -> {new_edges:?}; keys {old_keys:?} -> {new_keys:?} ({sink})",
        raw.len()
    );
}

/// CPU time of the calling thread (wall time where that is not available).
fn thread_cpu_time() -> std::time::Duration {
    #[cfg(windows)]
    {
        extern "system" {
            fn GetCurrentThread() -> isize;
            fn GetThreadTimes(
                thread: isize,
                creation: *mut u64,
                exit: *mut u64,
                kernel: *mut u64,
                user: *mut u64,
            ) -> i32;
        }
        let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
        // SAFETY: four valid out-pointers to FILETIME-sized values.
        let ok = unsafe {
            GetThreadTimes(
                GetCurrentThread(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        assert_ne!(ok, 0, "GetThreadTimes failed");
        std::time::Duration::from_nanos((kernel + user) * 100)
    }
    #[cfg(not(windows))]
    {
        static START: once_cell::sync::Lazy<std::time::Instant> =
            once_cell::sync::Lazy::new(std::time::Instant::now);
        START.elapsed()
    }
}

/// Each book's lines as the index keys them: normalized text, and the heading block each
/// belongs to.
fn keyed_books(books: &[(i64, Vec<String>)]) -> Vec<Vec<(String, u64)>> {
    books
        .iter()
        .map(|(_, rows)| {
            let mut section = 0u64;
            rows.iter()
                .flat_map(|row| row.split('\n'))
                .map(|raw| {
                    if raw.starts_with("<h") {
                        section += 1;
                    }
                    (
                        crate::hebrew_query::normalize_text_for_indexing(raw),
                        section,
                    )
                })
                .collect()
        })
        .collect()
}

fn line_refs(
    lines: &[(String, u64)],
) -> Vec<otzaria_semantic_search::semantic::chunk_key::LineRef<'_>> {
    lines
        .iter()
        .map(
            |(text, section)| otzaria_semantic_search::semantic::chunk_key::LineRef {
                text,
                section: *section,
            },
        )
        .collect()
}

#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB pointing at a real seforim.db"]
fn real_library_streamed_keys_are_the_sidecar_keys() {
    let Some(books) = sample_books() else {
        return;
    };
    let mut lines = 0;
    for book in keyed_books(&books) {
        let refs = line_refs(&book);
        assert_eq!(
            crate::semantic_keys::column_values(&refs),
            crate::semantic_keys::sidecar_column_values(&refs)
        );
        lines += refs.len();
    }
    println!("{} books, {lines} lines: chunk keys identical", books.len());
}
