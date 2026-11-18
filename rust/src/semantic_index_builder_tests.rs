use super::*;
fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let db = root.join("seforim.db");
    let source = root.join("released-index");
    std::fs::create_dir(&source).unwrap();
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("CREATE TABLE schema_meta(key TEXT PRIMARY KEY,value TEXT); INSERT INTO schema_meta VALUES('db_version','38'),('db_schema_version','7');
      CREATE TABLE book(id INTEGER PRIMARY KEY,title TEXT,contentHash INTEGER);
      CREATE TABLE line(id INTEGER PRIMARY KEY,bookId INTEGER,lineIndex INTEGER);
      CREATE INDEX idx_line_book_index ON line(bookId,lineIndex);
      CREATE TABLE line_content(id INTEGER PRIMARY KEY,content TEXT);
      CREATE TABLE line_note(lineId INTEGER,noteIndex INTEGER,charPos INTEGER,hasMarker INTEGER,body TEXT,PRIMARY KEY(lineId,noteIndex));
      INSERT INTO book VALUES(101,'שם במסד א',123),(205,'שם במסד ב',456);
      INSERT INTO line VALUES(1,101,0),(2,101,1),(3,205,0),(4,205,1);
      INSERT INTO line_content VALUES(1,'<h1>הלכות שבת</h1>'),(2,'הגוף המקורי והמשך הדברים בהלכה למעשה'),(3,'<h1>פרשת בראשית</h1>'),(4,'בראשית ברא אלהים את השמים ואת הארץ');
      INSERT INTO line_note VALUES(2,0,5,0,'פירוש הערה נפרד וארוך מספיק להטמעה');").unwrap();
    drop(conn);
    configure_line_source(db.to_str().unwrap().to_string()).unwrap();
    let mut engine = SearchEngine::new(source.to_str().unwrap());
    engine.open_index_writer().unwrap();
    engine.force_book_number_for_tests("id:101", 3);
    engine.force_book_number_for_tests("id:205", 11);
    engine
        .set_book_order(vec![
            BookOrderEntry {
                file_path: "id:101".to_string(),
                catalogue_order: 70,
                generation_order: 5,
            },
            BookOrderEntry {
                file_path: "id:205".to_string(),
                catalogue_order: 2,
                generation_order: 1,
            },
        ])
        .unwrap();
    let inputs = [
        (101, "שם במסד א", "כותרת מן הקטלוג א", "/הלכה/שבת", 123),
        (205, "שם במסד ב", "כותרת מן הקטלוג ב", "/תנך/תורה", 456),
    ]
    .into_iter()
    .map(|(id, dbtitle, title, topics, hash)| LibraryBookInput {
        source: LibraryBookSource::Library,
        attached_slug: None,
        book_id: id,
        db_title: Some(dbtitle.to_string()),
        title: title.to_string(),
        topics: topics.to_string(),
        file_path: format!("id:{id}"),
        extra_facets: Some(vec![
            "/author/רמבם".to_string(),
            "/era/ראשונים".to_string(),
            "/base".to_string(),
        ]),
        source_hash: Some(hash),
        text_storage: TextStorage::LibraryDb,
    })
    .collect();
    let results = engine
        .add_library_books(inputs, &IndexingCancellationToken::new())
        .unwrap();
    assert!(results
        .iter()
        .all(|r| r.status == LibraryBookStatus::Indexed));
    engine.commit().unwrap();
    drop(engine);
    let path = source.join("otzaria_index_meta.json");
    let mut metadata = json(&path).unwrap();
    metadata["semantic_projection_version"] = 0.into();
    std::fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    (source, db)
}

#[test]
fn fresh_rebuild_preserves_sparse_ids_text_catalogue_and_note_provenance() {
    let _guard = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let (source, db) = fixture(dir.path());
    let out = dir.path().join("fresh");
    let source_meta = std::fs::read(source.join("meta.json")).unwrap();
    let db_sha = digest_file(&db).unwrap();
    let report = build(BuildIndexRequest {
        source_index: source.clone(),
        seforim_db: db.clone(),
        out: out.clone(),
        library_version: 38,
    })
    .unwrap();
    assert_eq!(
        (
            report.books,
            report.source_documents,
            report.rebuilt_documents
        ),
        (2, 4, 4)
    );
    assert!(report.ids_text_and_metadata_verified);
    assert_eq!(
        report.rebuilt_index_metadata["semantic_projection_version"],
        1
    );
    assert_eq!(
        std::fs::read(source.join("meta.json")).unwrap(),
        source_meta
    );
    assert_eq!(digest_file(&db).unwrap(), db_sha);
    assert_eq!(
        std::fs::read(source.join(crate::book_order::BOOK_ORDER_FILE)).unwrap(),
        std::fs::read(out.join(crate::book_order::BOOK_ORDER_FILE)).unwrap()
    );
    let engine = SearchEngine::new(out.to_str().unwrap());
    let doc = engine
        .get_document_by_id((3u64 << 32) + 2)
        .unwrap()
        .unwrap();
    assert_eq!(doc.title, "כותרת מן הקטלוג א");
    assert!(doc.text.contains("פירוש הערה"));
    assert!(engine
        .get_document_by_id((11u64 << 32) + 1)
        .unwrap()
        .is_some());
    assert!(engine
        .get_document_by_id((101u64 << 32) + 1)
        .unwrap()
        .is_none());
    let snapshot = engine.corpus_searcher();
    let field = snapshot
        .schema()
        .get_field(crate::note_spans::FIELD)
        .unwrap();
    let mut note_found = false;
    for (segment, reader) in snapshot.segment_readers().iter().enumerate() {
        for at in 0..reader.max_doc() {
            if reader.is_deleted(at) {
                continue;
            }
            let stored: TantivyDocument =
                crate::doc_store::load_document(&snapshot, DocAddress::new(segment as u32, at))
                    .unwrap();
            if let Some(value) = stored.get_first(field).and_then(|v| v.as_str()) {
                note_found |= value.contains("Note");
            }
        }
    }
    assert!(note_found, "fresh index must store note provenance");
    assert!(out.join("build-provenance.json").exists());
    assert!(build(BuildIndexRequest {
        source_index: source,
        seforim_db: db,
        out,
        library_version: 38
    })
    .unwrap_err()
    .to_string()
    .contains("output already exists"));
}

#[test]
fn fresh_rebuild_refuses_wrong_db_version_before_creating_output() {
    let _guard = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let (source, db) = fixture(dir.path());
    let out = dir.path().join("fresh");
    let error = build(BuildIndexRequest {
        source_index: source,
        seforim_db: db,
        out: out.clone(),
        library_version: 39,
    })
    .unwrap_err();
    assert!(error.to_string().contains("DB version 38"));
    assert!(!out.exists());
}

#[test]
fn sparse_number_seed_reserves_durably_and_rejects_invalid_mapping_atomically() {
    let dir = tempfile::TempDir::new().unwrap();
    for entries in [
        vec![("id:1".to_string(), 0)],
        vec![("id:1".to_string(), u32::MAX)],
        vec![(String::new(), 3)],
        vec![("id:1".to_string(), 3), ("id:2".to_string(), 3)],
        vec![("id:1".to_string(), 3), ("id:1".to_string(), 4)],
    ] {
        let mut numbers = crate::book_order::BookNumbers::new(dir.path());
        assert!(numbers.seed_for_rebuild(entries).is_err());
        assert!(!dir
            .path()
            .join(crate::book_order::BOOK_NUMBERS_FILE)
            .exists());
    }
    let mut numbers = crate::book_order::BookNumbers::new(dir.path());
    numbers
        .seed_for_rebuild(vec![("id:101".to_string(), 3), ("id:205".to_string(), 11)])
        .unwrap();
    assert_eq!(
        json(&dir.path().join(crate::book_order::BOOK_NUMBERS_FILE)).unwrap()["reserved"],
        12
    );
    assert_eq!(numbers.number_for("id:101").unwrap(), 3);
    assert_eq!(numbers.number_for("id:205").unwrap(), 11);
    assert_eq!(numbers.number_for("new:book").unwrap(), 12);
    assert!(numbers
        .seed_for_rebuild(vec![("id:3".to_string(), 20)])
        .is_err());
}

#[test]
fn fresh_rebuild_rejects_same_version_db_with_changed_text() {
    let _guard = crate::line_source::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::TempDir::new().unwrap();
    let (source, db) = fixture(dir.path());
    let out = dir.path().join("fresh");
    let connection = Connection::open(&db).unwrap();
    connection.execute("UPDATE line_content SET content='טקסט שונה מאותו ספר ומהדורה בלי עדכון חתימת המקור' WHERE id=4",[]).unwrap();
    drop(connection);
    let error = build(BuildIndexRequest {
        source_index: source,
        seforim_db: db,
        out: out.clone(),
        library_version: 38,
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("text or metadata fingerprints"),
        "{error:#}"
    );
    assert!(!out.join("build-provenance.json").exists());
}
