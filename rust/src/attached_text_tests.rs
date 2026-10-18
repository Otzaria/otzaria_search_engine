//! `TextStorage::LibraryDb` for books of attached databases (`db:<slug>:<bookId>`): any
//! number of them at once, each configured, removed and checked on its own.

use crate::api::library_indexing::{
    library_book_content_fingerprint, IndexingCancellationToken, LibraryBookInput,
    LibraryBookSource, LibraryBookStatus,
};
use crate::api::search_engine::*;
use crate::external_text_tests::{doc_store_view, exact, guard, train_dictionary};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const FIRST: [&str; 4] = [
    "<h1>מסד ראשון</h1>",
    "בראשית ברא אלהים במסד הראשון",
    "שורה עם <b>הדגשה</b> &amp; סימנים",
    "אור ראשון",
];
const SECOND: [&str; 3] = [
    "<h1>מסד שני</h1>",
    "בראשית ברא אלהים במסד השני דחוס",
    "וְהָאָרֶץ הָיְתָה תֹהוּ",
];

/// An attached database in seforim.db's layout holding book 1 = `rows`; `compressed`
/// stores them as zstd frames. Row ids run against `lineIndex` order.
fn write_attached(path: &Path, rows: &[&str], compressed: bool) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY NOT NULL, content BLOB);
         CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL);
         INSERT INTO book VALUES (1, 'ספר');",
    )
    .unwrap();
    let dict = train_dictionary();
    conn.execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [&dict])
        .unwrap();
    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    for (index, row) in rows.iter().enumerate().rev() {
        let id = 100 - index as i64;
        conn.execute(
            "INSERT INTO line VALUES (?1, 1, ?2)",
            rusqlite::params![id, index as i64],
        )
        .unwrap();
        if compressed {
            let frame = compressor.compress(row.as_bytes()).unwrap();
            conn.execute(
                "INSERT INTO line_content VALUES (?1, ?2)",
                rusqlite::params![id, frame],
            )
        } else {
            conn.execute(
                "INSERT INTO line_content VALUES (?1, ?2)",
                rusqlite::params![id, row],
            )
        }
        .unwrap();
    }
}

fn attached_db(dir: &Path, slug: &str, rows: &[&str], compressed: bool) -> PathBuf {
    let db = dir.join(format!("{slug}.db"));
    write_attached(&db, rows, compressed);
    configure_attached_line_source(
        slug.to_string(),
        Some(db.to_string_lossy().into_owned()),
        false,
    )
    .unwrap();
    db
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
            "/מצורף".to_string(),
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

fn index(dir: &Path, storage: TextStorage) -> SearchEngine {
    std::fs::create_dir_all(dir).unwrap();
    let mut engine = SearchEngine::new(dir.to_str().unwrap());
    add_book(&mut engine, "db:first:1", 0, &FIRST, storage);
    add_book(&mut engine, "db:second.v2:1", 1, &SECOND, storage);
    engine.commit().unwrap();
    engine
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

#[test]
fn two_attached_databases_answer_like_stored_text() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    attached_db(dir.path(), "first", &FIRST, false);
    attached_db(dir.path(), "second.v2", &SECOND, true);
    let stored = index(&dir.path().join("stored"), TextStorage::InIndex);
    let external = index(&dir.path().join("external"), TextStorage::LibraryDb);
    assert!(doc_store_view(&external)
        .iter()
        .all(|(path, has_text, check)| path.starts_with("db:") && !has_text && check.is_some()));
    for word in ["בראשית", "אור", "הדגשה", "היתה", "דחוס", "מסד"] {
        let (a, b) = (exact(&stored, word), exact(&external, word));
        assert!(!a.is_empty(), "{word}");
        assert_eq!(view(&a), view(&b), "{word}");
    }
    // The same book id in both databases: each hit reads its own one.
    let both = exact(&external, "בראשית");
    assert_eq!(both.len(), 2);
    assert!(both.iter().all(|r| r.text_status == TextStatus::Ok));
    for slug in ["first", "second.v2"] {
        let status = attached_line_source_status(slug.to_string());
        assert!(status.configured && status.open, "{slug}");
    }
    let status = line_source_status();
    assert!(!status.configured && !status.user_books_configured);
    assert!(!attached_line_source_status("never".to_string()).configured);
}

#[test]
fn removing_an_attached_database_leaves_its_lines_unavailable() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let first = attached_db(dir.path(), "first", &FIRST, false);
    attached_db(dir.path(), "second.v2", &SECOND, true);
    let engine = index(&dir.path().join("idx"), TextStorage::LibraryDb);
    let generation = attached_line_source_status("first".to_string()).generation;

    // Unconfiguring closes the file at once: the app may delete it.
    configure_attached_line_source("first".to_string(), None, false).unwrap();
    let status = attached_line_source_status("first".to_string());
    assert!(!status.configured && !status.open && status.generation > generation);
    std::fs::remove_file(&first).unwrap();
    let gone = hit(&engine, "הדגשה", "db:first:1");
    assert_eq!(
        (gone.text_status, gone.text.as_str()),
        (TextStatus::Unavailable, "")
    );
    assert_eq!(
        hit(&engine, "היתה", "db:second.v2:1").text_status,
        TextStatus::Ok
    );

    // Attached again (here a new copy at the same path): read again.
    write_attached(&first, &FIRST, true);
    configure_attached_line_source(
        "first".to_string(),
        Some(first.to_string_lossy().into_owned()),
        false,
    )
    .unwrap();
    assert_eq!(
        hit(&engine, "הדגשה", "db:first:1").text_status,
        TextStatus::Ok
    );
    // Unconfiguring a slug never seen is a no-op; an empty path is refused.
    configure_attached_line_source("never".to_string(), None, false).unwrap();
    assert!(
        configure_attached_line_source("first".to_string(), Some(String::new()), false).is_err()
    );
    assert!(configure_attached_line_source(
        "bad:slug".to_string(),
        Some(first.to_string_lossy().into_owned()),
        false
    )
    .is_err());
}

#[test]
fn a_changed_attached_row_is_stale() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let db = attached_db(dir.path(), "first", &FIRST, false);
    let engine = index(&dir.path().join("idx"), TextStorage::LibraryDb);
    Connection::open(&db)
        .unwrap()
        .execute(
            "UPDATE line_content SET content = 'שורה אחרת לגמרי' \
             WHERE id = (SELECT id FROM line WHERE lineIndex = 2)",
            [],
        )
        .unwrap();
    let changed = hit(&engine, "הדגשה", "db:first:1");
    assert_eq!(
        (changed.text_status, changed.text.as_str()),
        (TextStatus::Stale, "שורה אחרת לגמרי")
    );
    assert_eq!(
        hit(&engine, "אור", "db:first:1").text_status,
        TextStatus::Ok
    );
}

#[test]
fn an_immutable_attached_database_is_read_through_its_uri() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    // A WAL database in a folder whose name needs escaping in a URI.
    let folder = dir.path().join("מסד #1 %20");
    std::fs::create_dir_all(&folder).unwrap();
    let db = folder.join("first.db");
    write_attached(&db, &FIRST, true);
    Connection::open(&db)
        .unwrap()
        .execute_batch("PRAGMA journal_mode=WAL")
        .unwrap();
    configure_attached_line_source(
        "first".to_string(),
        Some(db.to_string_lossy().into_owned()),
        true,
    )
    .unwrap();
    let engine = index(&dir.path().join("idx"), TextStorage::LibraryDb);
    assert!(doc_store_view(&engine)
        .iter()
        .filter(|(path, _, _)| path == "db:first:1")
        .all(|(_, has_text, _)| !has_text));
    assert_eq!(
        hit(&engine, "הדגשה", "db:first:1").text_status,
        TextStatus::Ok
    );
}

#[test]
fn an_unreadable_or_different_attached_book_keeps_its_text_in_the_index() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let fallbacks = |slug: &str| attached_line_source_status(slug.to_string()).library_fallbacks;
    let (first, second) = (fallbacks("first"), fallbacks("second.v2"));
    let library = line_source_status().library_fallbacks;
    // Unconfigured: nothing can vouch for the rows.
    let unconfigured = index(&dir.path().join("a"), TextStorage::LibraryDb);
    assert!(doc_store_view(&unconfigured)
        .iter()
        .all(|(_, has_text, _)| *has_text));
    assert_eq!(
        (fallbacks("first"), fallbacks("second.v2")),
        (first + 1, second + 1)
    );
    assert_eq!(line_source_status().library_fallbacks, library);
    let first = first + 1;

    // A row count that differs from the book's lines.
    attached_db(dir.path(), "first", &FIRST, false);
    let idx = dir.path().join("b");
    std::fs::create_dir_all(&idx).unwrap();
    let mut engine = SearchEngine::new(idx.to_str().unwrap());
    add_book(
        &mut engine,
        "db:first:1",
        0,
        &FIRST[..3],
        TextStorage::LibraryDb,
    );
    engine.commit().unwrap();
    assert_eq!(fallbacks("first"), first + 1);
    assert!(doc_store_view(&engine)
        .iter()
        .all(|(_, has_text, _)| *has_text));
    assert_eq!(
        hit(&engine, "הדגשה", "db:first:1").text_status,
        TextStatus::Ok
    );

    // Text stored in the index (an index built before) needs no database at all.
    configure_attached_line_source("first".to_string(), None, false).unwrap();
    assert_eq!(
        view(&exact(&unconfigured, "בראשית")),
        view(&exact(
            &index(&dir.path().join("c"), TextStorage::InIndex),
            "בראשית"
        ))
    );
}

#[test]
fn attached_book_keys_must_be_canonical() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    for bad in [
        "db:", "db:1", "db::1", "db:a:", "db:a:x", "db:a:-1", "db:a:007", "db:a:+7", "db:a b:1",
        "db:a/b:1", "DB:a:1",
    ] {
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
        assert!(
            err.to_string().contains("db:<slug>:<bookId>"),
            "{bad}: {err:#}"
        );
    }
    // The app's slugs: letters of any script, digits, `._-`; the id follows the last `:`.
    for good in ["db:ספרייה_2.v-1:7", "db:x:0"] {
        assert!(engine
            .add_text_book_placed(
                "t".to_string(),
                "/a".to_string(),
                good.to_string(),
                0,
                0,
                "בראשית".to_string(),
                None,
                TextStorage::LibraryDb,
                None,
            )
            .is_ok());
    }
}

fn input(
    slug: Option<&str>,
    source: LibraryBookSource,
    file_path: &str,
    order: u32,
) -> LibraryBookInput {
    LibraryBookInput {
        source,
        attached_slug: slug.map(str::to_string),
        book_id: 1,
        db_title: Some("ספר".to_string()),
        title: "ספר".to_string(),
        topics: "/מצורף".to_string(),
        file_path: file_path.to_string(),
        extra_facets: None,
        text_storage: TextStorage::LibraryDb,
        source_hash: Some(u64::from(order) + 1),
    }
}

#[test]
fn add_library_books_reads_attached_databases() {
    let _guard = guard();
    let dir = TempDir::new().unwrap();
    attached_db(dir.path(), "first", &FIRST, false);
    attached_db(dir.path(), "second.v2", &SECOND, true);
    let idx = dir.path().join("idx");
    std::fs::create_dir_all(&idx).unwrap();
    let mut engine = SearchEngine::new(idx.to_str().unwrap());
    engine.open_index_writer().unwrap();
    let results = engine
        .add_library_books(
            vec![
                input(Some("first"), LibraryBookSource::Attached, "db:first:1", 0),
                input(
                    Some("second.v2"),
                    LibraryBookSource::Attached,
                    "db:second.v2:1",
                    1,
                ),
                // The slug must name the file_path's database, and only with `Attached`.
                input(
                    Some("first"),
                    LibraryBookSource::Attached,
                    "db:second.v2:1",
                    2,
                ),
                input(None, LibraryBookSource::Attached, "db:first:1", 3),
                input(Some("first"), LibraryBookSource::Library, "id:1", 4),
                input(Some("never"), LibraryBookSource::Attached, "db:never:1", 5),
            ],
            &IndexingCancellationToken::new(),
        )
        .unwrap();
    let statuses: Vec<_> = results.iter().map(|r| r.status).collect();
    assert_eq!(
        statuses,
        [
            LibraryBookStatus::Indexed,
            LibraryBookStatus::Indexed,
            LibraryBookStatus::Failed,
            LibraryBookStatus::Failed,
            LibraryBookStatus::Failed,
            LibraryBookStatus::Unavailable,
        ]
    );
    assert!(results[..2]
        .iter()
        .all(|r| r.text_storage == TextStorage::LibraryDb));
    engine.commit().unwrap();
    assert!(doc_store_view(&engine)
        .iter()
        .all(|(_, has_text, _)| !has_text));
    let by_app = index(&dir.path().join("app"), TextStorage::LibraryDb);
    for word in ["בראשית", "הדגשה", "היתה"] {
        assert_eq!(
            view(&exact(&by_app, word)),
            view(&exact(&engine, word)),
            "{word}"
        );
    }
    let text_hashes = engine.get_book_text_fingerprints().unwrap();
    for (slug, result) in [("first", &results[0]), ("second.v2", &results[1])] {
        assert_eq!(
            Some(&result.text_hash),
            text_hashes.get(&format!("db:{slug}:1"))
        );
        assert_eq!(
            library_book_content_fingerprint(
                LibraryBookSource::Attached,
                Some(slug.to_string()),
                1,
                Some("ספר".to_string()),
            )
            .unwrap(),
            Some(result.text_hash)
        );
    }
    assert!(library_book_content_fingerprint(LibraryBookSource::Attached, None, 1, None).is_err());
}
