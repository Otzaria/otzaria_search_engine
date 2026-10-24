//! `add_library_books` against the bytes the app reads for a book and hands to
//! `add_text_book_bytes`: the same documents, fingerprints and storage.

use crate::api::library_indexing::{
    library_book_content_fingerprint, IndexingCancellationToken, LibraryBookInput,
    LibraryBookSource, LibraryBookStatus,
};
use crate::api::search_engine::{
    compute_content_fingerprint_bytes, configure_line_source, SearchEngine, TextBookMeta,
    TextStorage,
};
use crate::library_indexing::Outcome;
use std::path::Path;

const BOM: &str = "\u{FEFF}";

fn image() -> String {
    format!(
        "data:image/png;base64,{}",
        "iVBORw0KGgoAAAANSUhEUgAA".repeat(4)
    )
}

/// `(id, title, rows)`; `None` is a row without a `line_content` row.
type FixtureBook = (i64, &'static str, Vec<Option<String>>);

fn fixture_books() -> Vec<FixtureBook> {
    let plain = |rows: &[&str]| rows.iter().map(|r| Some(r.to_string())).collect();
    let mut long: Vec<Option<String>> = (0..300)
        .map(|n| Some(format!("שורה {n} בראשית ברא (א) אלהים ורמב\"ם [ב] וְהָאָרֶץ")))
        .collect();
    long.insert(0, Some("<h1>ספר ארוך</h1>".to_string()));
    long.insert(100, Some("<h2>חלק ב</h2>".to_string()));
    vec![
        (
            1,
            "פשוט",
            plain(&[
                "<h1>פשוט</h1>",
                "<h2>פרק א</h2>",
                "בראשית ברא אלהים את השמים ואת הארץ",
                "וַיֹּ֥אמֶר אֱלֹהִ֖ים יְהִ֣י א֑וֹר&nbsp;וַֽיְהִי אֽוֹר׃",
                "",
                "שורה עם <b>הדגשה</b> &amp; סימנים (הוצא) [היצא] {פ}",
                "קצר data:abc נשאר",
                "סיום חלונות\r",
            ]),
        ),
        (
            2,
            "תמונות ו-BOM",
            vec![
                Some(format!("{BOM}<h1>ספר</h1> בראשית")),
                Some(format!("מילים <img src=\"{}\"> בראשית", image())),
                Some(format!("{BOM}שורה שמתחילה בסימן")),
                None,
                Some(format!("שתי תמונות {} ו-{}", image(), image())),
            ],
        ),
        (3, "BOM נקי", plain(&[&format!("{BOM}בראשית ברא"), "אור"])),
        (
            4,
            "שורה עם מעבר",
            plain(&["ראשונה", "שנייה\nבתוכה", "שלישית"]),
        ),
        (
            5,
            "דחוס",
            plain(&["<h1>דחוס</h1>", "בראשית ברא", "וְהָאָרֶץ הָיְתָה"]),
        ),
        (6, "ארוך", long),
        (7, "ריק", plain(&[""])),
    ]
}

/// A database shaped like seforim.db; book 5's rows are zstd frames of a dictionary, and
/// rowid order is not `lineIndex` order.
fn write_fixture(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY NOT NULL, content TEXT);
         CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL);",
    )
    .unwrap();
    let dict = crate::external_text_tests::train_dictionary();
    conn.execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [&dict])
        .unwrap();
    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    let mut line_id = 100;
    for (id, title, rows) in fixture_books() {
        conn.execute(
            "INSERT INTO book (id, title) VALUES (?1, ?2)",
            rusqlite::params![id, title],
        )
        .unwrap();
        for (index, row) in rows.iter().enumerate().rev() {
            line_id += 1;
            conn.execute(
                "INSERT INTO line (id, bookId, lineIndex) VALUES (?1, ?2, ?3)",
                rusqlite::params![line_id, id, index as i64 * 3],
            )
            .unwrap();
            let Some(row) = row else { continue };
            if id == 5 {
                let frame = compressor.compress(row.as_bytes()).unwrap();
                conn.execute(
                    "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                    rusqlite::params![line_id, frame],
                )
                .unwrap();
            } else {
                conn.execute(
                    "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                    rusqlite::params![line_id, row],
                )
                .unwrap();
            }
        }
    }
}

/// What the app hands `add_text_book_bytes` for a book (`loadTextBookSource`), read the
/// way `BookTextReader` reads it: the joined bytes, or — when `data:` occurs — the text
/// decoded as one string and cleaned of images; the storage `textStorageFor` picks; and
/// the image rows (`dataUriLineOrdinals`).
struct AppInput {
    bytes: Vec<u8>,
    storage: TextStorage,
    stored_lines: Option<Vec<u32>>,
}

fn app_input(db: &Path, book_id: i64) -> Option<AppInput> {
    let conn = rusqlite::Connection::open_with_flags(
        db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let dicts: Vec<Vec<u8>> = if conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='zstd_dict')",
            [],
            |r| r.get::<_, bool>(0),
        )
        .unwrap()
    {
        conn.prepare("SELECT dict FROM zstd_dict")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    } else {
        Vec::new()
    };
    let split = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='line_content')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap();
    let sql = if split {
        "SELECT CAST(lc.content AS BLOB), typeof(lc.content) = 'blob' FROM line l \
         LEFT JOIN line_content lc ON lc.id = l.id WHERE l.bookId = ?1 ORDER BY l.lineIndex, l.id"
    } else {
        "SELECT CAST(content AS BLOB), typeof(content) = 'blob' FROM line \
         WHERE bookId = ?1 ORDER BY lineIndex, id"
    };
    let mut stmt = conn.prepare(sql).unwrap();
    let mut rows = stmt.query([book_id]).unwrap();
    let mut bytes = Vec::new();
    let mut count = 0;
    let mut row_has_newline = false;
    while let Some(row) = rows.next().unwrap() {
        if count > 0 {
            bytes.push(b'\n');
        }
        let raw: Option<Vec<u8>> = row.get(0).unwrap();
        let raw = raw.unwrap_or_default();
        let content = if row.get::<_, Option<bool>>(1).unwrap() == Some(true) {
            dicts
                .iter()
                .find_map(|dict| {
                    zstd::bulk::Decompressor::with_dictionary(dict)
                        .ok()?
                        .decompress(&raw, 1 << 24)
                        .ok()
                })
                .expect("a frame of a dictionary in zstd_dict")
        } else {
            raw
        };
        row_has_newline |= content.contains(&b'\n');
        bytes.extend_from_slice(&content);
        count += 1;
    }
    if bytes.is_empty() {
        return None;
    }
    let storage = if row_has_newline {
        TextStorage::InIndex
    } else {
        TextStorage::LibraryDb
    };
    if !bytes.windows(5).any(|w| w == b"data:") {
        return Some(AppInput {
            bytes,
            storage,
            stored_lines: None,
        });
    }
    let stored: Vec<u32> = bytes
        .split(|&b| b == b'\n')
        .enumerate()
        .filter(|(_, line)| {
            let line = String::from_utf8_lossy(line);
            crate::line_source::strip_data_uris_for_index(&line) != line
        })
        .map(|(n, _)| n as u32)
        .collect();
    let text = String::from_utf8_lossy(&bytes);
    let text = text.strip_prefix(BOM).unwrap_or(&text);
    let text = crate::line_source::strip_data_uris_for_index(text);
    (!text.is_empty()).then(|| AppInput {
        bytes: text.into_bytes(),
        storage,
        stored_lines: (!stored.is_empty()).then_some(stored),
    })
}

fn new_engine(dir: &Path) -> SearchEngine {
    std::fs::create_dir_all(dir).unwrap();
    SearchEngine::new(dir.to_str().unwrap())
}

fn library_input(book_id: i64, title: &str) -> LibraryBookInput {
    LibraryBookInput {
        source: LibraryBookSource::Library,
        attached_slug: None,
        book_id,
        db_title: Some(title.to_string()),
        title: format!("כותרת {title}"),
        topics: format!("/ספרייה/{book_id}"),
        file_path: format!("id:{book_id}"),
        extra_facets: Some(vec![format!("/era/{book_id}"), "/author/x".to_string()]),
        text_storage: TextStorage::LibraryDb,
        source_hash: Some(book_id as u64 * 7 + 1),
    }
}

/// Every live document, stored fields as JSON, sorted.
fn stored_documents(engine: &SearchEngine) -> Vec<String> {
    let searcher = engine.searcher_for_tests();
    let schema = searcher.schema().clone();
    let mut out = Vec::new();
    for (ord, reader) in searcher.segment_readers().iter().enumerate() {
        for doc in reader.doc_ids_alive() {
            let document: tantivy::TantivyDocument = searcher
                .doc(tantivy::DocAddress::new(ord as u32, doc))
                .unwrap();
            out.push(tantivy::Document::to_json(&document, &schema));
        }
    }
    out.sort();
    out
}

/// The documents `add_text_book_bytes` builds from the app's input against those
/// `add_library_books` builds from the database: identical, field for field, indexed-only
/// fields included.
fn assert_prepared_alike(engine: &SearchEngine, db: &Path, input: &LibraryBookInput) {
    let schema = engine.searcher_for_tests().schema().clone();
    let outcome = crate::library_indexing::prepare(engine, input);
    let Some(app) = app_input(db, input.book_id) else {
        assert!(
            matches!(&outcome, Outcome::Done(done) if done.status == LibraryBookStatus::Missing),
            "book {}",
            input.book_id
        );
        return;
    };
    let Outcome::Prepared(mut prepared) = outcome else {
        panic!("book {} was not prepared", input.book_id);
    };
    let text = String::from_utf8(app.bytes.clone()).unwrap();
    // `library_storage_or_fallback`: LibraryDb holds when the book's rows are its lines.
    let rows =
        crate::line_source::with_store_for_indexing(crate::line_source::Source::Library, |store| {
            store.book_row_count(input.book_id)
        })
        .ok();
    let storage = match app.storage {
        TextStorage::LibraryDb if rows != Some(text.split('\n').count() as u64) => {
            TextStorage::InIndex
        }
        storage => storage,
    };
    let meta = TextBookMeta {
        title: input.title.clone(),
        topics: input.topics.clone(),
        file_path: input.file_path.clone(),
        extra_facets: input.extra_facets.clone().unwrap_or_default(),
        source_hash: input.source_hash.unwrap_or(0),
    };
    let mut expected = engine
        .prepare_text_book(
            &meta,
            &text,
            storage,
            app.stored_lines.as_deref().unwrap_or_default(),
        )
        .unwrap();
    let digests = |prepared: &mut crate::api::search_engine::PreparedTextBook| {
        let mut hashes = Vec::new();
        prepared
            .for_each_document(|document| {
                hashes.push(compute_content_fingerprint_bytes(
                    tantivy::Document::to_json(&document, &schema).into_bytes(),
                ));
                Ok(())
            })
            .unwrap();
        hashes
    };
    assert_eq!(
        digests(&mut prepared),
        digests(&mut expected),
        "book {}",
        input.book_id
    );
    assert_eq!(prepared.text_storage, expected.text_storage);
    assert_eq!(prepared.content_hash, expected.content_hash);
    assert_eq!(
        prepared.text_hash,
        compute_content_fingerprint_bytes(app.bytes)
    );
}

#[test]
fn library_books_index_like_the_bytes_the_app_reads() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    write_fixture(&db);
    configure_line_source(db.display().to_string()).unwrap();
    let inputs: Vec<LibraryBookInput> = fixture_books()
        .iter()
        .map(|(id, title, _)| library_input(*id, title))
        .collect();

    let mut by_app = new_engine(&dir.path().join("app"));
    by_app.open_index_writer().unwrap();
    for input in &inputs {
        assert_prepared_alike(&by_app, &db, input);
        if let Some(app) = app_input(&db, input.book_id) {
            by_app
                .add_text_book_bytes(
                    input.title.clone(),
                    input.topics.clone(),
                    input.file_path.clone(),
                    app.bytes,
                    input.extra_facets.clone(),
                    app.storage,
                    app.stored_lines,
                    input.source_hash,
                )
                .unwrap();
        }
    }
    by_app.commit().unwrap();

    let mut by_engine = new_engine(&dir.path().join("engine"));
    by_engine.open_index_writer().unwrap();
    let results = by_engine
        .add_library_books(inputs.clone(), &IndexingCancellationToken::new())
        .unwrap();
    by_engine.commit().unwrap();

    use LibraryBookStatus::*;
    use TextStorage::*;
    let statuses: Vec<(i64, LibraryBookStatus, TextStorage)> = results
        .iter()
        .map(|r| (r.book_id, r.status, r.text_storage))
        .collect();
    assert_eq!(
        statuses,
        [
            (1, Indexed, LibraryDb),
            (2, Indexed, LibraryDb),
            (3, Indexed, LibraryDb),
            (4, Indexed, InIndex),
            (5, Indexed, LibraryDb),
            (6, Indexed, LibraryDb),
            (7, Missing, LibraryDb),
        ]
    );
    assert_eq!(stored_documents(&by_engine), stored_documents(&by_app));
    assert_eq!(
        by_engine.get_book_fingerprints().unwrap(),
        by_app.get_book_fingerprints().unwrap()
    );
    let text_hashes = by_app.get_book_text_fingerprints().unwrap();
    assert_eq!(by_engine.get_book_text_fingerprints().unwrap(), text_hashes);
    let content_hashes = by_app.get_book_fingerprints().unwrap();
    for result in results.iter().filter(|r| r.status == Indexed) {
        let key = format!("id:{}", result.book_id);
        assert_eq!(Some(&result.text_hash), text_hashes.get(&key));
        assert_eq!(Some(&result.content_hash), content_hashes.get(&key));
        assert_eq!(
            library_book_content_fingerprint(
                LibraryBookSource::Library,
                None,
                result.book_id,
                Some(fixture_books()[result.book_id as usize - 1].1.to_string()),
            )
            .unwrap(),
            Some(result.text_hash)
        );
    }
    // A batch book carries every stamp the app's path records, and its result says which.
    let stamps = by_engine.get_book_stamps().unwrap();
    assert_eq!(stamps, by_app.get_book_stamps().unwrap());
    for result in results.iter().filter(|r| r.status == Indexed) {
        let key = format!("id:{}", result.book_id);
        let book = stamps.iter().find(|book| book.file_path == key).unwrap();
        assert_eq!(
            (result.book_number, result.metadata_hash, result.source_hash),
            (book.book_number, book.metadata_hash, book.source_hash)
        );
        assert_eq!(result.source_hash, result.book_id as u64 * 7 + 1);
        assert_eq!(
            result.book_number, result.book_id as u32,
            "numbered in input order"
        );
    }
    assert_eq!(
        library_book_content_fingerprint(LibraryBookSource::Library, None, 7, None).unwrap(),
        None
    );
}

#[test]
fn a_cancelled_unreadable_or_mismatched_book_adds_nothing() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    write_fixture(&db);
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let mut inputs: Vec<LibraryBookInput> = fixture_books()
        .iter()
        .map(|(id, title, _)| library_input(*id, title))
        .collect();

    let results = engine
        .add_library_books(inputs.clone(), &IndexingCancellationToken::new())
        .unwrap();
    assert!(results
        .iter()
        .all(|r| r.status == LibraryBookStatus::Unavailable && r.docs == 0));

    configure_line_source(db.display().to_string()).unwrap();
    let cancel = IndexingCancellationToken::new();
    cancel.cancel();
    let results = engine.add_library_books(inputs.clone(), &cancel).unwrap();
    assert!(results
        .iter()
        .all(|r| r.status == LibraryBookStatus::Cancelled && r.docs == 0));

    // A title that no longer matches; LibraryDb under another book's file path.
    inputs[0].db_title = Some("אחר".to_string());
    inputs[1].file_path = "id:99".to_string();
    let results = engine
        .add_library_books(inputs[..2].to_vec(), &IndexingCancellationToken::new())
        .unwrap();
    assert_eq!(results[0].status, LibraryBookStatus::Missing);
    assert_eq!(results[1].status, LibraryBookStatus::Failed);
    engine.commit().unwrap();
    assert!(stored_documents(&engine).is_empty());
}

#[test]
#[ignore = "needs OTZARIA_SEFORIM_DB pointing at a real seforim.db"]
fn real_library_books_prepare_like_the_bytes_the_app_reads() {
    let _guard = crate::external_text_tests::guard();
    let Ok(db) = std::env::var("OTZARIA_SEFORIM_DB") else {
        return;
    };
    let step: usize = std::env::var("OTZARIA_PARITY_STEP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    configure_line_source(db.clone()).unwrap();
    let dir = tempfile::TempDir::new().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    engine.open_index_writer().unwrap();
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .unwrap();
    let all_books: Vec<(i64, String)> = conn
        .prepare("SELECT id, title FROM book WHERE totalLines > 0 ORDER BY id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let forced: std::collections::HashSet<i64> = std::env::var("OTZARIA_PARITY_BOOK_IDS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|id| id.trim().parse().ok())
        .collect();
    let books: Vec<_> = all_books
        .into_iter()
        .enumerate()
        .filter(|(ordinal, (id, _))| ordinal % step.max(1) == 0 || forced.contains(id))
        .map(|(_, book)| book)
        .collect();
    for (id, title) in &books {
        assert_prepared_alike(&engine, Path::new(&db), &library_input(*id, title));
    }
    println!(
        "{} books: documents and fingerprints identical",
        books.len()
    );
}

/// Books prepared on several threads still land in input order, and searches run while a
/// batch is in flight: the batch holds the engine by shared reference.
#[test]
fn a_batch_enqueues_in_input_order_while_searches_run() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY NOT NULL, content TEXT);",
    )
    .unwrap();
    // Sizes vary so that later books finish preparing before earlier ones.
    conn.execute_batch("BEGIN").unwrap();
    for book in 1..=40i64 {
        conn.execute(
            "INSERT INTO book (id, title) VALUES (?1, ?2)",
            rusqlite::params![book, format!("ספר {book}")],
        )
        .unwrap();
        let lines = if book % 7 == 1 { 3000 } else { 5 };
        for line in 0..lines {
            let id = book * 10_000 + line;
            conn.execute(
                "INSERT INTO line (id, bookId, lineIndex) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, book, line],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                rusqlite::params![id, format!("שורה {line} של ספר {book} בראשית ברא")],
            )
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);
    configure_line_source(db.display().to_string()).unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let inputs: Vec<LibraryBookInput> = (1..=40)
        .map(|book| library_input(book, &format!("ספר {book}")))
        .collect();
    let engine_ref = &engine;
    let results = std::thread::scope(|scope| {
        let batch = scope.spawn(move || {
            engine_ref
                .add_library_books(inputs, &IndexingCancellationToken::new())
                .unwrap()
        });
        while !batch.is_finished() {
            engine_ref.get_book_fingerprints().unwrap();
        }
        batch.join().unwrap()
    });
    assert!(results
        .iter()
        .enumerate()
        .all(|(n, r)| r.book_id == n as i64 + 1 && r.status == LibraryBookStatus::Indexed));
    engine.commit().unwrap();
    let searcher = engine.searcher_for_tests();
    let mut docs = 0;
    for reader in searcher.segment_readers() {
        let column = reader.fast_fields().u64("id").unwrap();
        let ids: Vec<u64> = reader
            .doc_ids_alive()
            .filter_map(|doc| column.first(doc))
            .collect();
        // Each indexing thread keeps the order documents were added in.
        assert!(
            ids.windows(2).all(|pair| pair[0] < pair[1]),
            "out of input order"
        );
        docs += ids.len();
    }
    assert_eq!(docs, 6 * 3000 + 34 * 5);
}

#[test]
fn a_book_that_panics_fails_alone_and_the_batch_finishes() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY NOT NULL, content TEXT);
         BEGIN;",
    )
    .unwrap();
    for book in 1..=8i64 {
        conn.execute(
            "INSERT INTO book (id, title) VALUES (?1, ?2)",
            rusqlite::params![book, format!("ספר {book}")],
        )
        .unwrap();
        for line in 0..200 {
            let id = book * 10_000 + line;
            conn.execute(
                "INSERT INTO line (id, bookId, lineIndex) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, book, line],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                rusqlite::params![id, format!("שורה {line} של ספר {book} בראשית ברא")],
            )
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);
    configure_line_source(db.display().to_string()).unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let engine = std::sync::Arc::new(engine);
    crate::library_indexing::PANIC_ON_BOOK.store(1, std::sync::atomic::Ordering::Relaxed);
    let (done, results) = std::sync::mpsc::channel();
    let batch = engine.clone();
    std::thread::spawn(move || {
        let inputs: Vec<LibraryBookInput> = (1..=8)
            .map(|book| library_input(book, &format!("ספר {book}")))
            .collect();
        let _ = done.send(batch.add_library_books(inputs, &IndexingCancellationToken::new()));
    });
    let results = results.recv_timeout(std::time::Duration::from_secs(60));
    crate::library_indexing::PANIC_ON_BOOK.store(i64::MIN, std::sync::atomic::Ordering::Relaxed);
    let results = results.expect("the batch finishes").unwrap();
    assert_eq!(results[0].status, LibraryBookStatus::Failed);
    assert!(results[1..]
        .iter()
        .all(|r| r.status == LibraryBookStatus::Indexed));
}

// ── Schema 7: footnote bodies in `line_note` ──────────────────────────────────────────

const MARKED: &str = "גוף<sup class=\"footnote-marker\">א</sup> המשך\
                      <sup class=\"footnote-marker\">ב</sup> סוף";
const PLACED: &str = "גוף<sup class=\"footnote-marker\">א</sup> <b>המשך</b> סוף";

/// `(book, row lineIndex, noteIndex, charPos, hasMarker, body)`.
type FixtureNote = (i64, i64, i64, i64, bool, &'static str);

fn notes_fixture_books() -> Vec<(i64, &'static str, Vec<String>)> {
    vec![
        (
            21,
            "הערות",
            vec![
                format!("{BOM}<h1>הערות</h1>"),
                MARKED.to_string(),
                PLACED.to_string(),
                format!(
                    "תמונה <img src=\"{}\"> אחרי<sup class=\"footnote-marker\">ג</sup>",
                    image()
                ),
                "שורה בלי הערה".to_string(),
            ],
        ),
        (
            22,
            "בלי הערות",
            vec!["<h1>בלי הערות</h1>".to_string(), MARKED.to_string()],
        ),
    ]
}

fn fixture_notes() -> Vec<FixtureNote> {
    vec![
        (21, 1, 0, -1, true, "ראשונה זרזיר"),
        (21, 1, 1, -1, false, "נספחת קורקבן"),
        (21, 1, 2, -1, true, "שנייה חרטום"),
        (21, 2, 0, 4, true, "מוקדמת"),
        (21, 2, 1, 9, false, "באמצע תמנון"),
        (21, 3, 0, -1, true, "בתמונה ינשוף"),
    ]
}

/// What the app indexes for book 21 (`loadTextBookSource`): the bodies back in their lines,
/// the BOM dropped by decoding, the image cleaned off.
fn book_21_text() -> String {
    [
        "<h1>הערות</h1>",
        "גוף<sup class=\"footnote-marker\">א</sup><i class=\"footnote\">ראשונה זרזיר</i> המשך\
         <sup class=\"footnote-marker\">ב</sup><i class=\"footnote\">שנייה חרטום</i> סוף\
         <i class=\"footnote\">נספחת קורקבן</i>",
        "גוף<sup class=\"footnote-marker\">א</sup><i class=\"footnote\">מוקדמת</i> \
         <b>המשך</b><i class=\"footnote\">באמצע תמנון</i> סוף",
        "תמונה <img src=\"\"> אחרי<sup class=\"footnote-marker\">ג</sup>\
         <i class=\"footnote\">בתמונה ינשוף</i>",
        "שורה בלי הערה",
    ]
    .join("\n")
}

/// A schema-7 database; without `with_note_table`, the same rows in a schema-6 one.
fn write_notes_fixture(path: &Path, with_note_table: bool) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE book (id INTEGER PRIMARY KEY NOT NULL, title TEXT NOT NULL);
         CREATE TABLE line (id INTEGER PRIMARY KEY NOT NULL, bookId INTEGER NOT NULL,
                            lineIndex INTEGER NOT NULL);
         CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
         CREATE TABLE line_content (id INTEGER PRIMARY KEY NOT NULL, content TEXT);",
    )
    .unwrap();
    if with_note_table {
        conn.execute_batch(
            "CREATE TABLE line_note (lineId INTEGER NOT NULL, noteIndex INTEGER NOT NULL,
                                     charPos INTEGER NOT NULL, hasMarker INTEGER NOT NULL, body TEXT NOT NULL,
                                     PRIMARY KEY (lineId, noteIndex)) WITHOUT ROWID;",
        )
        .unwrap();
    }
    for (book, title, rows) in notes_fixture_books() {
        conn.execute(
            "INSERT INTO book (id, title) VALUES (?1, ?2)",
            rusqlite::params![book, title],
        )
        .unwrap();
        // Rowid order is not `lineIndex` order.
        for (index, row) in rows.iter().enumerate().rev() {
            let id = book * 1000 - index as i64;
            conn.execute(
                "INSERT INTO line (id, bookId, lineIndex) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, book, index as i64],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO line_content (id, content) VALUES (?1, ?2)",
                rusqlite::params![id, row],
            )
            .unwrap();
        }
    }
    if with_note_table {
        for (book, line, note, char_pos, has_marker, body) in fixture_notes().into_iter().rev() {
            conn.execute(
                "INSERT INTO line_note (lineId, noteIndex, charPos, hasMarker, body)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    book * 1000 - line,
                    note,
                    char_pos,
                    i64::from(has_marker),
                    body
                ],
            )
            .unwrap();
        }
    }
}

fn hit_books(engine: &SearchEngine, word: &str) -> Vec<(String, String)> {
    engine
        .search(
            vec![word.to_string()],
            vec![],
            20,
            0,
            0,
            100,
            crate::api::search_engine::ResultsOrder::Catalogue,
            None,
            None,
        )
        .unwrap()
        .into_iter()
        .map(|r| (r.file_path, r.text))
        .collect()
}

#[test]
fn line_note_bodies_are_indexed_like_the_apps_rehydrated_text() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    write_notes_fixture(&db, true);
    configure_line_source(db.display().to_string()).unwrap();
    let inputs: Vec<LibraryBookInput> = notes_fixture_books()
        .iter()
        .map(|(id, title, _)| library_input(*id, title))
        .collect();

    let mut by_engine = new_engine(&dir.path().join("engine"));
    by_engine.open_index_writer().unwrap();
    let results = by_engine
        .add_library_books(inputs.clone(), &IndexingCancellationToken::new())
        .unwrap();
    by_engine.commit().unwrap();
    let statuses: Vec<(i64, LibraryBookStatus, TextStorage)> = results
        .iter()
        .map(|r| (r.book_id, r.status, r.text_storage))
        .collect();
    assert_eq!(
        statuses,
        [
            (21, LibraryBookStatus::Indexed, TextStorage::InIndex),
            (22, LibraryBookStatus::Indexed, TextStorage::LibraryDb),
        ]
    );

    // The app's per-book path: the rehydrated bytes with InIndex, the plain book as rows.
    let mut by_app = new_engine(&dir.path().join("app"));
    let text = book_21_text();
    for (input, bytes, storage, stored_lines) in [
        (
            &inputs[0],
            text.clone().into_bytes(),
            TextStorage::InIndex,
            Some(vec![3]),
        ),
        (
            &inputs[1],
            format!("<h1>בלי הערות</h1>\n{MARKED}").into_bytes(),
            TextStorage::LibraryDb,
            None,
        ),
    ] {
        by_app
            .add_text_book_bytes(
                input.title.clone(),
                input.topics.clone(),
                input.file_path.clone(),
                bytes,
                input.extra_facets.clone(),
                storage,
                stored_lines,
                input.source_hash,
            )
            .unwrap();
    }
    by_app.commit().unwrap();
    assert_eq!(stored_documents(&by_engine), stored_documents(&by_app));
    assert_eq!(
        by_engine.get_book_fingerprints().unwrap(),
        by_app.get_book_fingerprints().unwrap()
    );
    assert_eq!(
        by_engine.get_book_text_fingerprints().unwrap(),
        by_app.get_book_text_fingerprints().unwrap()
    );
    // `textBookContentMatchesIndex` hashes the same text the batch indexed.
    let expected_hash = compute_content_fingerprint_bytes(text.clone().into_bytes());
    assert_eq!(results[0].text_hash, expected_hash);
    assert_eq!(
        library_book_content_fingerprint(
            LibraryBookSource::Library,
            None,
            21,
            Some("הערות".to_string())
        )
        .unwrap(),
        Some(expected_hash)
    );

    // Words found only in footnotes, at the end, after a marker and at an offset; a body's
    // first word too, which the marker's sup keeps apart from the marker's letter.
    for word in [
        "זרזיר",
        "קורקבן",
        "חרטום",
        "תמנון",
        "ינשוף",
        "ראשונה",
        "שנייה",
        "מוקדמת",
        "בתמונה",
    ] {
        let hits = hit_books(&by_engine, word);
        assert_eq!(hits.len(), 1, "{word}");
        assert_eq!(hits[0].0, "id:21", "{word}");
        let highlighted = format!("<font color=red>{word}</font>");
        assert!(hits[0].1.contains(&highlighted), "{word}");
    }
    assert!(hit_books(&by_engine, "אראשונה").is_empty());
}

/// Turns a notes fixture into the release shape: every line and every `line_note` body a
/// zstd frame of the dictionary in `zstd_dict`, as SeforimLibrary's compressLineContent stores them.
fn compress_notes_fixture(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let dict = crate::external_text_tests::train_dictionary();
    conn.execute_batch("CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL);")
        .unwrap();
    conn.execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [&dict])
        .unwrap();
    let mut compressor = zstd::bulk::Compressor::with_dictionary(3, &dict).unwrap();
    for (table, key, column) in [
        ("line_content", "CAST(id AS TEXT)", "content"),
        ("line_note", "lineId || ':' || noteIndex", "body"),
    ] {
        let rows: Vec<(String, String)> = conn
            .prepare(&format!("SELECT {key}, {column} FROM {table}"))
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for (k, text) in rows {
            let frame = compressor.compress(text.as_bytes()).unwrap();
            conn.execute(
                &format!("UPDATE {table} SET {column} = ?1 WHERE {key} = ?2"),
                rusqlite::params![frame, k],
            )
            .unwrap();
        }
    }
    let plain: i64 = conn
        .query_row(
            "SELECT (SELECT count(*) FROM line_content WHERE typeof(content) <> 'blob') + \
             (SELECT count(*) FROM line_note WHERE typeof(body) <> 'blob')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(plain, 0);
}

#[test]
fn compressed_line_note_bodies_are_indexed_like_plain_ones() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let inputs: Vec<LibraryBookInput> = notes_fixture_books()
        .iter()
        .map(|(id, title, _)| library_input(*id, title))
        .collect();
    let mut engines = Vec::new();
    for compressed in [false, true] {
        let db = dir.path().join(format!("seforim-{compressed}.db"));
        write_notes_fixture(&db, true);
        if compressed {
            compress_notes_fixture(&db);
        }
        configure_line_source(db.display().to_string()).unwrap();
        let mut engine = new_engine(&dir.path().join(format!("engine-{compressed}")));
        engine.open_index_writer().unwrap();
        let results = engine
            .add_library_books(inputs.clone(), &IndexingCancellationToken::new())
            .unwrap();
        engine.commit().unwrap();
        assert!(results
            .iter()
            .all(|r| r.status == LibraryBookStatus::Indexed));
        assert_eq!(
            results[0].text_hash,
            compute_content_fingerprint_bytes(book_21_text().into_bytes())
        );
        engines.push(engine);
    }
    assert_eq!(stored_documents(&engines[0]), stored_documents(&engines[1]));
    for word in ["זרזיר", "קורקבן", "חרטום", "תמנון", "ינשוף"] {
        let hits = hit_books(&engines[1], word);
        assert_eq!(hits.len(), 1, "{word}");
        assert_eq!(hits[0].0, "id:21", "{word}");
    }
}

#[test]
fn a_note_body_that_cannot_be_decoded_fails_the_book_instead_of_dropping_the_note() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    write_notes_fixture(&db, true);
    let conn = rusqlite::Connection::open(&db).unwrap();
    // A frame in a database without `zstd_dict`.
    conn.execute(
        "UPDATE line_note SET body = X'28B52FFD00' WHERE lineId = 20999 AND noteIndex = 0",
        [],
    )
    .unwrap();
    drop(conn);
    configure_line_source(db.display().to_string()).unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let results = engine
        .add_library_books(
            vec![library_input(21, "הערות")],
            &IndexingCancellationToken::new(),
        )
        .unwrap();
    engine.commit().unwrap();
    assert_eq!(results[0].status, LibraryBookStatus::Failed);
    assert!(hit_books(&engine, "המשך").is_empty());
}

#[test]
fn invalid_utf8_in_a_note_fails_the_whole_book_before_adding_documents() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    for compressed in [true, false] {
        let db = dir.path().join(format!("seforim-{compressed}.db"));
        write_notes_fixture(&db, true);
        if compressed {
            compress_notes_fixture(&db);
        }
        let conn = rusqlite::Connection::open(&db).unwrap();
        let invalid_utf8 = [0xff, b'a'];
        let body = if compressed {
            let dict: Vec<u8> = conn
                .query_row("SELECT dict FROM zstd_dict", [], |r| r.get(0))
                .unwrap();
            zstd::bulk::Compressor::with_dictionary(3, &dict)
                .unwrap()
                .compress(&invalid_utf8)
                .unwrap()
        } else {
            invalid_utf8.to_vec()
        };
        // The last note in the book: earlier valid rows must not be partially indexed.
        conn.execute(
            if compressed {
                "UPDATE line_note SET body = ?1 WHERE lineId = 20997 AND noteIndex = 0"
            } else {
                "UPDATE line_note SET body = CAST(?1 AS TEXT) WHERE lineId = 20997 AND noteIndex = 0"
            },
            [body],
        )
        .unwrap();
        drop(conn);
        let mut store = crate::line_source::LineStore::open(&db).unwrap();
        let error = store
            .read_book_for_indexing(21, None, true, &|| false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("book 21 line 3 note 0"), "{error}");
        assert!(error.contains("UTF-8"), "{error}");
        configure_line_source(db.display().to_string()).unwrap();
        let mut engine = new_engine(&dir.path().join(format!("index-{compressed}")));
        engine.open_index_writer().unwrap();
        let results = engine
            .add_library_books(
                vec![library_input(21, "הערות")],
                &IndexingCancellationToken::new(),
            )
            .unwrap();
        engine.commit().unwrap();
        assert_eq!(results[0].status, LibraryBookStatus::Failed, "{compressed}");
        assert_eq!(results[0].docs, 0);
        assert!(stored_documents(&engine).is_empty());
        assert!(results[0].error.is_some());
    }
}

#[test]
fn a_has_marker_other_than_0_or_1_fails_the_book() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    for (case, value) in ["2", "'x'"].into_iter().enumerate() {
        let db = dir.path().join(format!("seforim-{case}.db"));
        write_notes_fixture(&db, true);
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            &format!(
                "UPDATE line_note SET hasMarker = {value} WHERE lineId = 20999 AND noteIndex = 0"
            ),
            [],
        )
        .unwrap();
        drop(conn);
        configure_line_source(db.display().to_string()).unwrap();
        let mut engine = new_engine(&dir.path().join(format!("index-{case}")));
        engine.open_index_writer().unwrap();
        let results = engine
            .add_library_books(
                vec![library_input(21, "הערות")],
                &IndexingCancellationToken::new(),
            )
            .unwrap();
        engine.commit().unwrap();
        assert_eq!(results[0].status, LibraryBookStatus::Failed, "{value}");
    }
}

#[test]
fn without_line_note_or_outside_the_library_the_rows_are_indexed_as_they_are() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let schema6 = dir.path().join("seforim.db");
    write_notes_fixture(&schema6, false);
    let attached = dir.path().join("attached.db");
    write_notes_fixture(&attached, true);
    configure_line_source(schema6.display().to_string()).unwrap();
    crate::api::search_engine::configure_attached_line_source(
        "att".to_string(),
        Some(attached.display().to_string()),
        false,
    )
    .unwrap();

    let library = library_input(21, "הערות");
    // The app puts bodies back only in books of the library (`_dbBookNotes`).
    let attached_book = LibraryBookInput {
        source: LibraryBookSource::Attached,
        attached_slug: Some("att".to_string()),
        file_path: "db:att:21".to_string(),
        ..library_input(21, "הערות")
    };
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let results = engine
        .add_library_books(
            vec![library.clone(), attached_book.clone()],
            &IndexingCancellationToken::new(),
        )
        .unwrap();
    engine.commit().unwrap();
    assert!(results.iter().all(
        |r| r.status == LibraryBookStatus::Indexed && r.text_storage == TextStorage::LibraryDb
    ));
    assert!(hit_books(&engine, "קורקבן").is_empty());
    let mut books: Vec<String> = hit_books(&engine, "המשך")
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    books.sort();
    assert_eq!(books, ["db:att:21", "db:att:21", "id:21", "id:21"]);

    let rows = notes_fixture_books().swap_remove(0).2;
    let text = rows.join("\n");
    let text = crate::line_source::strip_data_uris_for_index(text.strip_prefix(BOM).unwrap());
    for (source, slug) in [
        (LibraryBookSource::Library, None),
        (LibraryBookSource::Attached, Some("att".to_string())),
    ] {
        assert_eq!(
            library_book_content_fingerprint(source, slug, 21, None).unwrap(),
            Some(compute_content_fingerprint_bytes(text.clone().into_bytes()))
        );
    }
}

#[test]
fn a_footnote_tag_inside_a_marker_does_not_hide_the_external_body() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    write_notes_fixture(&db, true);
    let conn = rusqlite::Connection::open(&db).unwrap();
    // Exact stored shape from generator InlineNotesTest: a footnote tag inside a sup.
    let stored = "א<sup class=\"footnote-marker\">1<i class=\"footnote\"></i></sup> ב";
    conn.execute("UPDATE line_content SET content=? WHERE id=20999", [stored])
        .unwrap();
    conn.execute("DELETE FROM line_note WHERE lineId=20999", [])
        .unwrap();
    conn.execute(
        "INSERT INTO line_note VALUES (20999,0,2,1,?)",
        ["הערה זרזיר"],
    )
    .unwrap();
    drop(conn);
    configure_line_source(db.display().to_string()).unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let result = engine
        .add_library_books(
            vec![library_input(21, "הערות")],
            &IndexingCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(result[0].status, LibraryBookStatus::Indexed);
    engine.commit().unwrap();
    let hits = hit_books(&engine, "זרזיר");
    assert_eq!(
        hits.len(),
        1,
        "body in line_note must be searchable even when the sup contains an empty footnote tag"
    );
}

#[test]
fn an_empty_marker_after_leading_tags_preserves_indexed_text_and_word_boundaries() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::TempDir::new().unwrap();
    let db = dir.path().join("seforim.db");
    write_notes_fixture(&db, true);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let stored = "<p><b><sup class=\"footnote-marker\"></sup>המשך</b></p>";
    let expected = "<p><b><sup class=\"footnote-marker\"></sup><i class=\"footnote\">הערה זרזיר</i>המשך</b></p>";
    conn.execute("UPDATE line_content SET content=? WHERE id=20999", [stored])
        .unwrap();
    conn.execute("DELETE FROM line_note WHERE lineId=20999", [])
        .unwrap();
    conn.execute(
        "INSERT INTO line_note VALUES (20999,0,0,1,?)",
        ["הערה זרזיר"],
    )
    .unwrap();
    drop(conn);
    configure_line_source(db.display().to_string()).unwrap();
    let input = library_input(21, "הערות");
    let mut from_db = new_engine(&dir.path().join("db_index"));
    from_db.open_index_writer().unwrap();
    let result = from_db
        .add_library_books(vec![input.clone()], &IndexingCancellationToken::new())
        .unwrap();
    assert_eq!(result[0].status, LibraryBookStatus::Indexed);
    from_db.commit().unwrap();
    let mut from_source = new_engine(&dir.path().join("source_index"));
    let mut rows = book_21_text()
        .split('\n')
        .map(str::to_string)
        .collect::<Vec<_>>();
    rows[1] = expected.to_string();
    from_source
        .add_text_book_bytes(
            input.title,
            input.topics,
            input.file_path,
            rows.join("\n").into_bytes(),
            input.extra_facets,
            TextStorage::InIndex,
            Some(vec![3]),
            input.source_hash,
        )
        .unwrap();
    from_source.commit().unwrap();
    assert_eq!(stored_documents(&from_db), stored_documents(&from_source));
    for word in ["זרזיר", "זרזירהמשך"] {
        assert_eq!(
            hit_books(&from_db, word),
            hit_books(&from_source, word),
            "{word}"
        );
    }
}

#[test]
fn spooled_documents_preserve_every_field_and_neighbour_keys() {
    use tantivy::schema::Value;
    let dir = tempfile::TempDir::new().unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let mut lines = Vec::new();
    for ordinal in 0..1100 {
        lines.push(match ordinal % 257 {
            0 => format!("<h2>פרק {ordinal}</h2>"),
            1 | 252 | 253 | 254 | 255 | 256 => "קצר מאד".to_string(),
            2 => "טקסט מנוקד וַיֹּאמֶר אֱלֹהִים וכל מילה שומרת על ניקודה".to_string(),
            3 => "שורה ארוכה שמשמשת גם לחלון סמנטי ".repeat(80),
            _ => format!("שורה {ordinal} מלאה מילים כדי להיבדק בפני עצמה"),
        });
    }
    lines.push(String::new());
    let text = lines.join("\n");
    let input = library_input(1, "גבולות");
    let meta = TextBookMeta {
        title: input.title,
        topics: input.topics,
        file_path: input.file_path,
        extra_facets: input.extra_facets.unwrap(),
        source_hash: 8,
    };
    let mut buffer = crate::indexing_buffers::TextBuffer::new(16);
    buffer.push_str(&text).unwrap();
    let mut stored = crate::indexing_buffers::OrdinalBuffer::default();
    for ordinal in [2, 255, 256, 257, 1024] {
        stored.push(ordinal).unwrap();
    }
    let mut prepared = engine
        .prepare_text_book_buffer(
            &meta,
            &mut buffer,
            TextStorage::LibraryDb,
            &mut stored,
            1024,
            &|| false,
        )
        .unwrap();
    assert!(prepared.spooled_documents.is_some());
    assert!(prepared.documents.is_empty());
    let mut expected = engine
        .prepare_text_book(
            &meta,
            &text,
            TextStorage::LibraryDb,
            &[257, 2, 1024, 255, 256],
        )
        .unwrap();
    let schema = engine.searcher_for_tests().schema().clone();
    let collect = |prepared: &mut crate::api::search_engine::PreparedTextBook| {
        let mut documents = Vec::new();
        prepared
            .for_each_document(|document| {
                documents.push(document);
                Ok(())
            })
            .unwrap();
        documents
    };
    let documents = collect(&mut prepared);
    let expected_documents = collect(&mut expected);
    assert_eq!(documents.len(), lines.len());
    for (actual, expected) in documents.iter().zip(&expected_documents) {
        assert_eq!(
            tantivy::Document::to_json(actual, &schema),
            tantivy::Document::to_json(expected, &schema)
        );
    }
    let normalized: Vec<String> = lines
        .iter()
        .map(|line| crate::hebrew_query::normalize_text_for_indexing(line))
        .collect();
    let mut section = 0u64;
    let sections: Vec<u64> = lines
        .iter()
        .map(|line| {
            if line.starts_with("<h") {
                section += 1;
            }
            section
        })
        .collect();
    let key_lines: Vec<otzaria_semantic_search::semantic::chunk_key::LineRef<'_>> = normalized
        .iter()
        .zip(&sections)
        .map(
            |(text, section)| otzaria_semantic_search::semantic::chunk_key::LineRef {
                text,
                section: *section,
            },
        )
        .collect();
    let keys = crate::semantic_keys::sidecar_column_values(&key_lines);
    let field = schema.get_field("chunkKey").unwrap();
    for (ordinal, (document, key)) in documents.iter().zip(keys).enumerate() {
        assert_eq!(
            document
                .get_first(field)
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            key,
            "line {ordinal}"
        );
    }
    assert_eq!(prepared.content_hash, expected.content_hash);
    assert_eq!(prepared.text_hash, expected.text_hash);
    assert_eq!(prepared.metadata_hash, expected.metadata_hash);
}

#[test]
fn cancellation_during_preparation_discards_a_spooled_book() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::TempDir::new().unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    let text = "שורה ארוכה מספיק כדי להיכנס לאינדקס ולהיבדק\n".repeat(1100);
    let mut buffer = crate::indexing_buffers::TextBuffer::new(16);
    buffer.push_str(&text).unwrap();
    let mut stored = crate::indexing_buffers::OrdinalBuffer::default();
    let meta = TextBookMeta {
        title: "ספר".into(),
        topics: "/ספר".into(),
        file_path: "id:1".into(),
        extra_facets: Vec::new(),
        source_hash: 0,
    };
    let checks = AtomicUsize::new(0);
    let error = engine
        .prepare_text_book_buffer(
            &meta,
            &mut buffer,
            TextStorage::LibraryDb,
            &mut stored,
            1024,
            &|| checks.fetch_add(1, Ordering::Relaxed) > 300,
        )
        .err()
        .expect("preparation must stop");
    assert!(error.to_string().contains("interrupted"));
    engine.commit().unwrap();
    assert_eq!(engine.searcher_for_tests().num_docs(), 0);
}

#[test]
fn a_search_only_engine_takes_no_writer_lock() {
    let dir = tempfile::TempDir::new().unwrap();
    let engine = new_engine(&dir.path().join("index"));
    assert!(!engine.holds_writer_for_tests());
    let mut writer = new_engine(&dir.path().join("index"));
    writer.open_index_writer().unwrap();
    assert!(writer.holds_writer_for_tests());
    assert!(!engine.holds_writer_for_tests());
    assert!(engine.get_book_fingerprints().unwrap().is_empty());
}

#[test]
fn economy_mode_limits_preparation_as_well_as_the_writer() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    let normal = engine.library_preparation_policy();
    engine.set_economy_indexing(true).unwrap();
    let economy = engine.library_preparation_policy();
    assert_eq!(
        economy,
        crate::indexing_buffers::PreparationPolicy::new(true, 1)
    );
    assert!(
        normal.book_workers >= economy.book_workers
            && normal.cpu_threads >= economy.cpu_threads
            && normal.document_bytes >= economy.document_bytes
    );
    assert!(!engine.holds_writer_for_tests());
}

#[test]
fn legacy_library_without_split_content_or_dictionaries_preserves_all_fields() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("legacy.db");
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT);
        CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
        INSERT INTO book VALUES(1,'ישן');",
    )
    .unwrap();
    for (id, text) in [
        format!("{BOM}<h1>ישן</h1>"),
        format!("מילים {} בראשית", image()),
        "".to_string(),
    ]
    .iter()
    .enumerate()
    {
        conn.execute(
            "INSERT INTO line VALUES(?1,1,?2,?3)",
            rusqlite::params![id + 1, id, text],
        )
        .unwrap();
    }
    drop(conn);
    configure_line_source(db.display().to_string()).unwrap();
    let mut engine = new_engine(&dir.path().join("index"));
    engine.open_index_writer().unwrap();
    assert_prepared_alike(&engine, &db, &library_input(1, "ישן"));
}

#[test]
fn notes_after_the_joined_text_range_are_still_validated() {
    let _guard = crate::external_text_tests::guard();
    let dir = tempfile::tempdir().unwrap();
    for (case, update) in [
        "UPDATE line_note SET hasMarker=2 WHERE lineId=20997",
        "UPDATE line_note SET body=X'28B52FFD00' WHERE lineId=20997",
        "UPDATE line_note SET body=CAST(X'FF61' AS TEXT) WHERE lineId=20997",
    ]
    .iter()
    .enumerate()
    {
        let db = dir.path().join(format!("notes-{case}.db"));
        write_notes_fixture(&db, true);
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection
            .execute("UPDATE line SET lineIndex=lineIndex*100+100", [])
            .unwrap();
        connection.execute(update, []).unwrap();
        drop(connection);
        configure_line_source(db.to_str().unwrap().into()).unwrap();
        let mut engine = new_engine(&dir.path().join(format!("index-{case}")));
        engine.open_index_writer().unwrap();
        let results = engine
            .add_library_books(
                vec![library_input(21, "הערות")],
                &IndexingCancellationToken::new(),
            )
            .unwrap();
        assert_eq!(results[0].status, LibraryBookStatus::Failed, "case {case}");
        engine.commit().unwrap();
        assert!(stored_documents(&engine).is_empty(), "case {case}");
        assert!(
            library_book_content_fingerprint(LibraryBookSource::Library, None, 21, None).is_err(),
            "case {case}"
        );
    }
}

pub(crate) fn preparation_budget_fixture(
    path: &Path,
    books: usize,
    lines: usize,
) -> Vec<LibraryBookInput> {
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
            db_title: None,
            ..library_input((book + 1) as i64, "בדיקת תקציב")
        })
        .collect()
}

#[test]
#[cfg(not(any(target_os = "android", target_os = "ios")))]
fn normal_preparation_avoids_medium_book_spools_without_changing_documents() {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let _guard = crate::external_text_tests::guard();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("budget.db");
    let input = preparation_budget_fixture(&path, 1, 12_000).remove(0);
    configure_line_source(path.to_str().unwrap().into()).unwrap();
    let mut engine = new_engine(&directory.path().join("index"));
    engine.open_index_writer().unwrap();
    let mut expected = None;
    for economy in [false, true] {
        engine.set_economy_indexing(economy).unwrap();
        let Outcome::Prepared(mut prepared) = crate::library_indexing::prepare(&engine, &input)
        else {
            panic!("preparation failed");
        };
        assert_eq!(prepared.spooled_documents.is_some(), economy);
        let mut hasher = DefaultHasher::new();
        let mut count = 0;
        let searcher = engine.searcher_for_tests();
        prepared
            .for_each_document(|document| {
                tantivy::Document::to_json(&document, searcher.schema()).hash(&mut hasher);
                count += 1;
                Ok(())
            })
            .unwrap();
        let actual = (
            count,
            hasher.finish(),
            prepared.content_hash,
            prepared.text_hash,
            prepared.metadata_hash,
        );
        if let Some(expected) = expected {
            assert_eq!(actual, expected);
        } else {
            expected = Some(actual);
        }
    }
}
