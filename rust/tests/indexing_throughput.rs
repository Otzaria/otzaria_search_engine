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
