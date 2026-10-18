//! `TextStorage::LibraryDb` for personal books (`uid:<bookId>`), whose rows are read from the
//! user-books database: the old single-table layout (`line.content`, TEXT or a zstd frame),
//! kept open for writing by the app (WAL) while the engine reads it.

use crate::api::search_engine::*;
use crate::external_text_tests::{doc_store_view, exact, guard, train_dictionary};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const PLAIN_ROWS: [&str; 5] = [
    "<h1>ספר אישי</h1>",
    "בראשית ברא אלהים את השמים ואת הארץ",
    "וְהָאָרֶץ הָיְתָה תֹהוּ וָבֹהוּ",
    "שורה עם <b>הדגשה</b> &amp; סימנים ורמב\"ם",
    "אור",
];
const COMPRESSED_ROWS: [&str; 4] = [
    "<h1>ספר דחוס</h1>",
    "בראשית ברא אלהים בספר הדחוס",
    "שורה דחוסה עם <i>תגית</i> ומים",
    "אור",
];

/// A user-books database shaped like the app's `user_books.db`, in WAL mode: book 1 as TEXT
/// rows, book 2 as zstd frames of a `zstd_dict` dictionary.
fn write_user_books(path: &Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         CREATE TABLE line (id INTEGER PRIMARY KEY AUTOINCREMENT, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL, content TEXT NOT NULL, heRef TEXT,
                            tocEntryId INTEGER);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL);",
    )
    .unwrap();
    let dict = train_dictionary();
    conn.execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [&dict])
        .unwrap();
    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    for (index, row) in PLAIN_ROWS.iter().enumerate().rev() {
        conn.execute(
            "INSERT INTO line (bookId, lineIndex, content) VALUES (1, ?1, ?2)",
            rusqlite::params![index as i64, row],
        )
        .unwrap();
    }
    for (index, row) in COMPRESSED_ROWS.iter().enumerate() {
        let frame = compressor.compress(row.as_bytes()).unwrap();
        conn.execute(
            "INSERT INTO line (bookId, lineIndex, content) VALUES (2, ?1, ?2)",
            rusqlite::params![index as i64, frame],
        )
        .unwrap();
    }
}

fn add_book(
    engine: &mut SearchEngine,
    file_path: &str,
    order: u32,
    rows: &[&str],
    storage: TextStorage,
) {
    let added = engine
        .add_text_book_placed(
            "ספר".to_string(),
            "/אישי".to_string(),
            file_path.to_string(),
            order,
            0,
            rows.join("\n"),
            None,
            storage,
            None,
        )
        .unwrap();
    assert_eq!(added as usize, rows.len(), "{file_path}");
}

fn index_user_books(dir: &Path, storage: TextStorage) -> SearchEngine {
    std::fs::create_dir_all(dir).unwrap();
    let mut engine = SearchEngine::new(dir.to_str().unwrap());
    add_book(&mut engine, "uid:1", 0, &PLAIN_ROWS, storage);
    add_book(&mut engine, "uid:2", 1, &COMPRESSED_ROWS, storage);
    engine.commit().unwrap();
    engine
}

fn user_books_db(dir: &Path) -> PathBuf {
    let db = dir.join("user_books.db");
    write_user_books(&db);
    configure_user_books_line_source(Some(db.to_string_lossy().into_owned())).unwrap();
    db
}

fn view(results: &[SearchResult]) -> Vec<(String, u64, String, TextStatus)> {
    results
        .iter()
        .map(|r| {
            (
                r.file_path.clone(),
                r.segment,
                r.text.clone(),
                r.text_status,
            )
        })
        .collect()
}

fn hit(e: &SearchEngine, word: &str, file_path: &str) -> SearchResult {
    exact(e, word)
        .into_iter()
        .find(|r| r.file_path == file_path)
        .unwrap_or_else(|| panic!("{word} in {file_path}"))
}

fn unhighlighted(text: &str) -> String {
    text.replace("<font color=red>", "").replace("</font>", "")
}

#[test]
fn user_book_rows_answer_like_stored_text() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    user_books_db(dir.path());
    let stored = index_user_books(&dir.path().join("stored"), TextStorage::InIndex);
    let external = index_user_books(&dir.path().join("external"), TextStorage::LibraryDb);
    assert!(doc_store_view(&external)
        .iter()
        .all(|(path, has_text, check)| path.starts_with("uid:") && !has_text && check.is_some()));
    for word in ["בראשית", "אור", "הדגשה", "תגית", "הארץ", "דחוסה"] {
        let (a, b) = (exact(&stored, word), exact(&external, word));
        assert!(!a.is_empty(), "{word}");
        assert_eq!(view(&a), view(&b), "{word}");
    }
    // The library source is not configured: every row above came from the user-books one.
    let status = line_source_status();
    assert!(!status.configured && status.user_books_configured && status.user_books_open);
}

#[test]
fn a_write_by_the_app_is_seen_by_the_next_window_and_checked() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let db = user_books_db(dir.path());
    let engine = index_user_books(&dir.path().join("idx"), TextStorage::LibraryDb);
    assert_eq!(hit(&engine, "הדגשה", "uid:1").text_status, TextStatus::Ok);

    // The app's connection stays open in WAL mode; the engine is never suspended.
    let writer = Connection::open(&db).unwrap();
    writer
        .execute_batch(
            "BEGIN IMMEDIATE;
         UPDATE line SET content = 'שורה אחרת לגמרי' WHERE bookId = 1 AND lineIndex = 3;",
        )
        .unwrap();
    // An open write transaction blocks no reader, which still sees the committed row.
    assert_eq!(hit(&engine, "הדגשה", "uid:1").text_status, TextStatus::Ok);
    writer.execute_batch("COMMIT").unwrap();
    let changed = hit(&engine, "הדגשה", "uid:1");
    assert_eq!(
        (changed.text_status, changed.text.as_str()),
        (TextStatus::Stale, "שורה אחרת לגמרי")
    );
    assert_eq!(hit(&engine, "השמים", "uid:1").text_status, TextStatus::Ok);

    // A deleted book (the app removing a personal book) reads as stale and empty.
    writer
        .execute("DELETE FROM line WHERE bookId = 2", [])
        .unwrap();
    let gone = hit(&engine, "תגית", "uid:2");
    assert_eq!(
        (gone.text_status, gone.text.as_str()),
        (TextStatus::Stale, "")
    );
    assert_eq!(hit(&engine, "השמים", "uid:1").text_status, TextStatus::Ok);
}

#[test]
fn an_unreadable_or_different_user_book_keeps_its_text_in_the_index() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let fallbacks = || line_source_status().user_books_fallbacks;
    let before = fallbacks();
    let library_before = line_source_status().library_fallbacks;
    // Unconfigured: nothing can vouch for the rows.
    let unconfigured = index_user_books(&dir.path().join("a"), TextStorage::LibraryDb);
    assert!(doc_store_view(&unconfigured)
        .iter()
        .all(|(_, has_text, _)| *has_text));
    assert_eq!(fallbacks(), before + 2);
    assert_eq!(line_source_status().library_fallbacks, library_before);

    let db = user_books_db(dir.path());
    // A row count that differs from the book's lines (the app's copy is newer).
    let idx = dir.path().join("b");
    std::fs::create_dir_all(&idx).unwrap();
    let mut engine = SearchEngine::new(idx.to_str().unwrap());
    add_book(
        &mut engine,
        "uid:1",
        0,
        &PLAIN_ROWS[..4],
        TextStorage::LibraryDb,
    );
    add_book(
        &mut engine,
        "uid:2",
        1,
        &COMPRESSED_ROWS,
        TextStorage::LibraryDb,
    );
    engine.commit().unwrap();
    assert_eq!(fallbacks(), before + 3);
    let stored: Vec<_> = doc_store_view(&engine)
        .into_iter()
        .map(|(path, has_text, _)| (path, has_text))
        .collect();
    assert!(stored
        .iter()
        .all(|(path, has_text)| *has_text == (path == "uid:1")));

    // Unconfiguring closes the file: the app may delete or move it at once.
    configure_user_books_line_source(None).unwrap();
    let status = line_source_status();
    assert!(!status.user_books_configured && !status.user_books_open);
    std::fs::remove_file(&db).unwrap();
    let unavailable = hit(&engine, "תגית", "uid:2");
    assert_eq!(
        (unavailable.text_status, unavailable.text.as_str()),
        (TextStatus::Unavailable, "")
    );
    assert_eq!(hit(&engine, "השמים", "uid:1").text_status, TextStatus::Ok);
    assert!(configure_user_books_line_source(Some(String::new())).is_err());
}

#[test]
fn library_and_user_books_with_the_same_id_do_not_mix() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let library = dir.path().join("seforim.db");
    let library_rows = [
        "<h1>ספר רשמי</h1>",
        "בראשית ברא אלהים בספר הרשמי",
        "אור רשמי",
    ];
    crate::external_text_tests::write_book(&library, 1, &library_rows);
    configure_line_source(library.to_string_lossy().into_owned()).unwrap();
    user_books_db(dir.path());

    let idx = dir.path().join("idx");
    std::fs::create_dir_all(&idx).unwrap();
    let mut engine = SearchEngine::new(idx.to_str().unwrap());
    add_book(
        &mut engine,
        "id:1",
        0,
        &library_rows,
        TextStorage::LibraryDb,
    );
    add_book(&mut engine, "uid:1", 1, &PLAIN_ROWS, TextStorage::LibraryDb);
    engine.commit().unwrap();
    assert!(doc_store_view(&engine)
        .iter()
        .all(|(_, has_text, _)| !has_text));

    // One window holds rows of both books, at the same ordinals.
    let both = exact(&engine, "בראשית");
    assert_eq!(both.len(), 2);
    for r in &both {
        let expected = if r.file_path == "id:1" {
            library_rows[1]
        } else {
            PLAIN_ROWS[1]
        };
        assert_eq!(r.text_status, TextStatus::Ok, "{}", r.file_path);
        assert_eq!(unhighlighted(&r.text), expected, "{}", r.file_path);
    }

    // Suspending the library leaves personal books readable, and the other way round.
    suspend_line_source().unwrap();
    assert_eq!(
        hit(&engine, "בראשית", "id:1").text_status,
        TextStatus::Unavailable
    );
    assert_eq!(hit(&engine, "בראשית", "uid:1").text_status, TextStatus::Ok);
    resume_line_source().unwrap();
    configure_user_books_line_source(None).unwrap();
    assert_eq!(hit(&engine, "בראשית", "id:1").text_status, TextStatus::Ok);
    assert_eq!(
        hit(&engine, "בראשית", "uid:1").text_status,
        TextStatus::Unavailable
    );
}

#[test]
fn user_book_keys_must_be_canonical() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    for bad in ["uid:", "uid:x", "uid:-1", "uid:007", "uid:1 ", "UID:1"] {
        let err = engine
            .add_text_book_placed(
                "t".to_string(),
                "/a".to_string(),
                bad.to_string(),
                0,
                0,
                "בראשית".to_string(),
                None,
                TextStorage::LibraryDb,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("uid:<bookId>"), "{bad}: {err:#}");
    }
}
