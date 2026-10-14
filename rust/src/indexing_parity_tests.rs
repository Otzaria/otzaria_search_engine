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
