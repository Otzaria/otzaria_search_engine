//! Synthetic regressions for Otzaria/otzaria#3292. These deliberately do not claim to
//! reproduce an unavailable corpus release; both HTML and line_note fixtures are here.

use super::*;
use crate::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource, LibraryBookStatus,
};
use tempfile::TempDir;

const BOOK: &str = "/notes-fixture.txt";
const RAW: &str =
    "גוף<sup class=\"footnote-marker\">12</sup><i class=\"footnote\">מפורשת ניכרת</i> סוף";
const NOTE: &str = "<otzaria-note>";
const END_NOTE: &str = "</otzaria-note>";
const MARKER: &str = "<otzaria-note-marker>";

fn engine() -> (SearchEngine, TempDir) {
    let dir = TempDir::new().unwrap();
    (SearchEngine::new(dir.path().to_str().unwrap()), dir)
}

fn add_book(engine: &mut SearchEngine, book: &str, raw: &str, storage: TextStorage) {
    assert_eq!(
        engine
            .add_text_book_placed(
                "ספר בדיקה".into(),
                "/notes".into(),
                book.into(),
                0,
                0,
                raw.into(),
                None,
                storage,
                None,
            )
            .unwrap() as usize,
        raw.split('\n').count()
    );
    engine.commit().unwrap();
}

fn exact(engine: &SearchEngine, query: &str) -> Vec<SearchResult> {
    engine
        .search_exact(
            query.into(),
            vec![],
            20,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            None,
            None,
        )
        .unwrap()
}

fn assert_note(hit: &SearchResult, word: &str) {
    assert_eq!(hit.text_status, TextStatus::Ok);
    assert!(
        hit.text.contains(NOTE),
        "note boundary absent: {}",
        hit.text
    );
    assert!(hit.text.contains(END_NOTE), "note not closed: {}", hit.text);
    let note = hit
        .text
        .split_once(NOTE)
        .unwrap()
        .1
        .split_once(END_NOTE)
        .unwrap()
        .0;
    assert!(note.contains(word), "word outside its note: {}", hit.text);
    assert!(
        note.contains("<font color=red>"),
        "note match lost its highlight: {}",
        hit.text
    );
}

#[test]
fn note_provenance_stored_book_keeps_marker_body_and_highlight_distinct() {
    let (mut engine, _dir) = engine();
    add_book(&mut engine, BOOK, RAW, TextStorage::InIndex);
    let hits = exact(&engine, "מפורשת");
    assert_eq!(hits.len(), 1);
    assert_note(&hits[0], "מפורשת");
    assert!(hits[0]
        .text
        .contains("<otzaria-note-marker>12</otzaria-note-marker>"));
    assert!(hits[0]
        .text
        .contains("<otzaria-note><font color=red>מפורשת</font> ניכרת</otzaria-note>"));
    let document = engine.get_document_by_id(hits[0].id).unwrap().unwrap();
    assert_eq!(document.text, "גוף 12 מפורשת ניכרת סוף");
    assert!(
        !document.text.contains("otzaria-note"),
        "raw document API stays unannotated"
    );
}

#[test]
fn note_provenance_every_text_ingestion_path_preserves_the_same_snippet() {
    let mut outputs = Vec::new();
    for mode in 0..5 {
        let (mut engine, _dir) = engine();
        match mode {
            0 => add_book(&mut engine, BOOK, RAW, TextStorage::InIndex),
            1 => {
                engine.set_economy_indexing(true).unwrap();
                add_book(&mut engine, BOOK, RAW, TextStorage::InIndex);
            }
            2 => {
                engine
                    .add_text_book_bytes_placed(
                        "ספר בדיקה".into(),
                        "/notes".into(),
                        BOOK.into(),
                        0,
                        0,
                        RAW.as_bytes().to_vec(),
                        None,
                        TextStorage::InIndex,
                        None,
                    )
                    .unwrap();
                engine.commit().unwrap();
            }
            3 => {
                engine
                    .add_document(
                        0,
                        "ספר בדיקה",
                        "",
                        "/notes",
                        RAW,
                        0,
                        false,
                        BOOK,
                        None,
                        None,
                    )
                    .unwrap();
                engine.commit().unwrap();
            }
            _ => {
                let input = SearchEngine::single_input(
                    0,
                    "ספר בדיקה",
                    "",
                    "/notes",
                    RAW,
                    0,
                    false,
                    BOOK,
                    None,
                    None,
                );
                engine.add_documents_batch(vec![input]).unwrap();
                engine.commit().unwrap();
            }
        }
        let hits = exact(&engine, "מפורשת");
        assert_eq!(hits.len(), 1, "ingestion path {mode}");
        assert_note(&hits[0], "מפורשת");
        outputs.push(hits[0].text.clone());
    }
    assert!(
        outputs.windows(2).all(|pair| pair[0] == pair[1]),
        "{outputs:?}"
    );
}

#[test]
fn note_provenance_repeated_body_words_are_never_reclassified_as_notes() {
    let (mut engine, _dir) = engine();
    add_book(
        &mut engine,
        BOOK,
        "זהה <i class='footnote'>זהה</i> זהה <i class='footnote'>זהה</i>",
        TextStorage::InIndex,
    );
    let hits = exact(&engine, "זהה");
    assert_eq!(hits.len(), 1);
    let html = &hits[0].text;
    assert_eq!(html.matches(NOTE).count(), 2, "{html}");
    assert!(html.starts_with("<font color=red>זהה</font> "), "{html}");
    assert_eq!(
        html.matches("<otzaria-note><font color=red>זהה</font></otzaria-note>")
            .count(),
        2
    );
}

#[test]
fn note_provenance_a_cropped_late_match_keeps_its_actual_note_offset() {
    let (mut engine, _dir) = engine();
    let raw = format!(
        "{}<i class='footnote'>{} מופעיחיד {}</i> סוף",
        "מילים בגוף ".repeat(100),
        "בתוך ההערה ".repeat(80),
        "המשך הערה ".repeat(80)
    );
    add_book(&mut engine, BOOK, &raw, TextStorage::InIndex);
    let hits = exact(&engine, "מופעיחיד");
    assert_eq!(hits.len(), 1);
    assert_note(&hits[0], "מופעיחיד");
    assert!(
        hits[0].text.len() < raw.len() / 2,
        "fixture must really crop"
    );
    assert!(
        hits[0].text.starts_with(NOTE),
        "cropped content still begins inside the note: {}",
        hits[0].text
    );
    assert!(hits[0].text.ends_with(END_NOTE));
}

#[test]
fn note_provenance_vocalized_query_and_vocalized_display_keep_the_marks() {
    let (mut engine, _dir) = engine();
    let raw = "גוף <i class='footnote'>בָּרָא מִלָּה</i> סוף";
    add_book(&mut engine, BOOK, raw, TextStorage::InIndex);
    let matched = engine
        .search_exact(
            "בָּרָא".into(),
            vec![],
            10,
            0,
            ResultsOrder::Relevance,
            true,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(matched.len(), 1);
    assert_note(&matched[0], "בָּרָא");
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_vocalized_snippets(false);
        }
    }
    let _reset = Reset;
    set_vocalized_snippets(true);
    let shown = exact(&engine, "ברא");
    assert_eq!(shown.len(), 1);
    assert_note(&shown[0], "בָּרָא");
    assert!(shown[0]
        .text
        .contains("<otzaria-note><font color=red>בָּרָא</font> מִלָּה</otzaria-note>"));
}

#[test]
fn note_provenance_normalized_vocalized_inputs_borrow_only_matching_html() {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            set_vocalized_snippets(false);
        }
    }
    let _reset = Reset;
    let raw = "גוף <i class='footnote'>בָּרָא מִלָּה footnote</i> סוף";
    let (plain, vocalized) = hebrew_query::normalize_line_for_indexing(raw);
    let vocalized = vocalized.unwrap();
    for mode in 0..3 {
        let (mut engine, _dir) = engine();
        if mode == 0 {
            engine
                .add_document(
                    0,
                    "ספר בדיקה",
                    "",
                    "/notes",
                    raw,
                    0,
                    false,
                    BOOK,
                    None,
                    None,
                )
                .unwrap();
        } else {
            let mut input = SearchEngine::single_input(
                0,
                "ספר בדיקה",
                "",
                "/notes",
                raw,
                0,
                false,
                BOOK,
                None,
                None,
            );
            if mode == 1 {
                input.text_vocalized = Some(vocalized.clone());
            } else {
                input.text = plain.clone();
            }
            engine.add_documents_batch(vec![input]).unwrap();
        }
        engine.commit().unwrap();
        let matched = engine
            .search_exact(
                "בָּרָא".into(),
                vec![],
                10,
                0,
                ResultsOrder::Relevance,
                true,
                false,
                None,
                None,
            )
            .unwrap();
        assert_eq!(matched.len(), 1, "ingestion path {mode}");
        assert_note(&matched[0], "בָּרָא");
        set_vocalized_snippets(false);
        let unvocalized = exact(&engine, "ברא");
        assert_eq!(unvocalized.len(), 1);
        assert_note(&unvocalized[0], "ברא");
        set_vocalized_snippets(true);
        let shown = exact(&engine, "ברא");
        assert_eq!(shown.len(), 1);
        assert_note(&shown[0], "בָּרָא");
    }

    let different = "גוף מִלָּה בָּרָא footnote סוף";
    let notes = NoteSpans::from_renderings(raw, different, &plain, Some(different));
    assert!(notes.display(different.into(), true).spans.is_empty());
}

#[test]
fn note_provenance_a_natural_ellipsis_at_the_cut_is_not_a_synthetic_one() {
    let raw = "<i class='footnote'>אב…גד</i>";
    let (plain, vocalized) = hebrew_query::normalize_line_for_indexing(raw);
    let text = NoteSpans::from_html(raw, &plain, vocalized.as_deref()).display(plain, false);
    assert_eq!(
        bounded_display_snippet(&text, "אב".len() as u32),
        "<otzaria-note>אב</otzaria-note>…"
    );
    assert_eq!(
        bounded_display_snippet(&text, "אב…".len() as u32),
        "<otzaria-note>אב…</otzaria-note>…"
    );
    assert_eq!(
        bounded_display_snippet(&text, text.len() as u32),
        "<otzaria-note>אב…גד</otzaria-note>"
    );
    assert_eq!(bounded_display_snippet(&text, 0), "…");
}

#[cfg(feature = "semantic-integration")]
#[test]
fn note_provenance_semantic_ellipsis_keeps_explicit_crop_boundaries() {
    let raw = "<i class='footnote'>…אב…גד…</i>";
    let (plain, vocalized) = hebrew_query::normalize_line_for_indexing(raw);
    let text = NoteSpans::from_html(raw, &plain, vocalized.as_deref()).display(plain, false);
    let (window, html) =
        crate::semantic_highlight::marked_snippet_at(&text, "…".len().."…אב".len(), "אב".len())
            .unwrap();
    assert_eq!(window, "…".len().."…אב".len());
    let html = crate::semantic_highlight::with_ellipsis(
        text.annotate(html, window.start, "<mark>", "</mark>"),
        window.start > 0,
        window.end < text.len(),
    );
    assert_eq!(html, "…<otzaria-note><mark>אב</mark></otzaria-note>…");
}

#[test]
fn note_provenance_cross_line_phrase_carries_notes_from_both_lines() {
    let (mut engine, _dir) = engine();
    let raw = "גוף <i class='footnote'>מילהראשונה</i>\n<i class='footnote'>מילהשנייה</i> סוף";
    add_book(&mut engine, BOOK, raw, TextStorage::InIndex);
    let hits = exact(&engine, "מילהראשונה מילהשנייה");
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].continues_to_next_line,
        "fixture must use cross-line matching"
    );
    assert_eq!(hits[0].text.matches(NOTE).count(), 2, "{}", hits[0].text);
    assert!(hits[0].text.contains("<otzaria-note><font color=red>מילהראשונה</font></otzaria-note><br><otzaria-note><font color=red>מילהשנייה</font></otzaria-note>"), "{}", hits[0].text);
}

#[test]
fn note_provenance_ordinary_italics_and_superscripts_are_unchanged() {
    let (mut engine, _dir) = engine();
    add_book(
        &mut engine,
        BOOK,
        "גוף <i>מפורשת</i> <sup>12</sup> <i class='footnotes'>ניכרת</i>",
        TextStorage::InIndex,
    );
    let hits = exact(&engine, "מפורשת");
    assert_eq!(hits.len(), 1);
    assert!(!hits[0].text.contains(NOTE));
    assert!(!hits[0].text.contains(MARKER));
    assert!(hits[0].text.contains("<font color=red>מפורשת</font>"));
}

#[test]
fn note_provenance_schema_11_only_appends_a_stored_unindexed_field() {
    let old = schema_of_version(10).unwrap();
    let new = current_schema();
    assert_eq!(INDEX_SCHEMA_VERSION, 11);
    for (field, entry) in old.fields() {
        assert_eq!(new.get_field_entry(field), entry, "{}", entry.name());
    }
    assert_eq!(new.fields().count(), old.fields().count() + 1);
    let entry = new.get_field_entry(new.get_field("noteSpans").unwrap());
    assert!(
        entry.is_stored() && !entry.is_indexed() && !entry.is_fast() && !entry.has_fieldnorms()
    );
    assert_eq!(
        LINE_TEXT_VERSION, 4,
        "embedding and normalized text recipe unchanged"
    );
}

#[test]
fn note_provenance_schema_10_opens_and_accepts_writes_without_being_upgraded() {
    let dir = TempDir::new().unwrap();
    Index::create_in_dir(dir.path(), schema_of_version(10).unwrap()).unwrap();
    write_index_metadata(
        dir.path(),
        &index_metadata(10, Some(ChunkKeyRecipe::current())),
    )
    .unwrap();
    let metadata_before = fs::read(index_metadata_path(dir.path())).unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    add_book(&mut engine, BOOK, RAW, TextStorage::InIndex);
    assert!(engine.schema.get_field("noteSpans").is_err());
    let hits = exact(&engine, "מפורשת");
    assert_eq!(hits.len(), 1);
    assert!(
        !hits[0].text.contains(NOTE),
        "schema10 did not store note provenance"
    );
    assert_eq!(
        fs::read(index_metadata_path(dir.path())).unwrap(),
        metadata_before
    );
    drop(engine);
    let compatibility = check_index_compatibility(dir.path().to_string_lossy().into_owned());
    assert!(compatibility.compatible);
    assert_eq!(compatibility.found_schema_version, Some(10));
    let reopened = SearchEngine::new(dir.path().to_str().unwrap());
    let again = exact(&reopened, "מפורשת");
    assert_eq!(again[0].id, hits[0].id);
    assert_eq!(again[0].text, hits[0].text);
}

/// Same raw source indexed under both schemas: no changes to ids, text, postings,
/// deduplication hashes, fingerprints, chunk keys, or the scores of lexical queries.
#[test]
fn note_provenance_changes_display_only_across_schema_versions() {
    let mut engines = Vec::new();
    let mut dirs = Vec::new();
    let raw = format!("{RAW}\nגוף מפורשת סוף\nמְ<sup>נַ</sup>שֶּׁה <i class='footnote'>בָּרָא</i>");
    for version in [10, 11] {
        let dir = TempDir::new().unwrap();
        Index::create_in_dir(dir.path(), schema_of_version(version).unwrap()).unwrap();
        write_index_metadata(
            dir.path(),
            &index_metadata(version, Some(ChunkKeyRecipe::current())),
        )
        .unwrap();
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        add_book(&mut engine, BOOK, &raw, TextStorage::InIndex);
        engines.push(engine);
        dirs.push(dir);
    }
    let snapshot = |engine: &SearchEngine| {
        let searcher = engine.index_reader.searcher();
        let schema = &engine.schema;
        let address = searcher
            .search(&AllQuery, &TopDocs::with_limit(10).order_by_score())
            .unwrap();
        let mut rows = Vec::new();
        for (_, address) in address {
            let doc = load_document(&searcher, address).unwrap();
            let columns = searcher.segment_reader(address.segment_ord).fast_fields();
            let id = doc
                .get_first(schema.get_field("id").unwrap())
                .unwrap()
                .as_u64()
                .unwrap();
            let line = engine.get_document_by_id(id).unwrap().unwrap().text;
            let columns = [
                "lineHash",
                "contentHash",
                "textHash",
                "chunkKey",
                "lineFirst",
                "lineLast",
            ]
            .map(|name| columns.u64(name).unwrap().first(address.doc_id));
            rows.push((id, line, columns));
        }
        rows.sort_by_key(|row| row.0);
        rows
    };
    assert_eq!(snapshot(&engines[0]), snapshot(&engines[1]));
    for query in ["מפורשת", "גוף מפורשת", "מנשה", "ברא"] {
        let hits = |engine: &SearchEngine| {
            exact(engine, query)
                .into_iter()
                .map(|hit| (hit.id, hit.segment, hit.continues_to_next_line))
                .collect::<Vec<_>>()
        };
        assert_eq!(hits(&engines[0]), hits(&engines[1]), "{query}");
        let scores = |engine: &SearchEngine| {
            let (query, _) = engine
                .build_exact_query(query, &[], &VocalizedFlags::new(false, false))
                .unwrap();
            let searcher = engine.index_reader.searcher();
            let mut scores = searcher
                .search(&*query, &TopDocs::with_limit(20).order_by_score())
                .unwrap()
                .into_iter()
                .map(|(score, address)| {
                    let document = load_document(&searcher, address).unwrap();
                    (
                        document
                            .get_first(engine.schema.get_field("id").unwrap())
                            .unwrap()
                            .as_u64()
                            .unwrap(),
                        score.to_bits(),
                    )
                })
                .collect::<Vec<_>>();
            scores.sort_unstable();
            scores
        };
        assert_eq!(scores(&engines[0]), scores(&engines[1]), "{query}");
    }
}

fn line_note_fixture(dir: &TempDir) -> (rusqlite::Connection, String) {
    let path = dir.path().join("notes.db");
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE book(id INTEGER PRIMARY KEY, title TEXT);
        INSERT INTO book VALUES(1, 'בדיקה');
        CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
        CREATE INDEX lines_by_book ON line(bookId, lineIndex);
        CREATE TABLE line_note(lineId INTEGER, noteIndex INTEGER, charPos INTEGER, hasMarker INTEGER, body TEXT);").unwrap();
    db.execute(
        "INSERT INTO line VALUES(1,1,0,?1)",
        ["גוף<sup class=\"footnote-marker\">12</sup> סוף"],
    )
    .unwrap();
    db.execute(
        "INSERT INTO line_note VALUES(1,0,-1,1,?1)",
        ["מפורשת ניכרת"],
    )
    .unwrap();
    configure_line_source(path.to_string_lossy().into_owned()).unwrap();
    (db, RAW.to_string())
}

#[test]
fn note_provenance_library_line_note_native_path_matches_stored_html() {
    let _guard = crate::external_text_tests::guard();
    let db_dir = TempDir::new().unwrap();
    let (_db, raw) = line_note_fixture(&db_dir);
    let (mut engine, _dir) = engine();
    engine.open_index_writer().unwrap();
    let indexed = engine
        .add_library_books(
            vec![LibraryBookInput {
                source: LibraryBookSource::Library,
                attached_slug: None,
                book_id: 1,
                db_title: Some("בדיקה".into()),
                title: "ספר בדיקה".into(),
                topics: "/notes".into(),
                file_path: "id:1".into(),
                extra_facets: None,
                source_hash: None,
                text_storage: TextStorage::LibraryDb,
            }],
            &IndexingCancellationToken::new(),
        )
        .unwrap();
    engine.commit().unwrap();
    assert_eq!(indexed.len(), 1);
    assert_eq!(indexed[0].book_id, 1);
    assert_eq!(indexed[0].status, LibraryBookStatus::Indexed);
    assert_eq!(indexed[0].docs, 1);
    assert_eq!(indexed[0].text_storage, TextStorage::InIndex);
    // The native library path restores line_note bodies before indexing and stores
    // the restored text. Search-time LibraryDb reads do not restore those bodies.
    let searcher = engine.index_reader.searcher();
    for (_, address) in searcher
        .search(&AllQuery, &TopDocs::with_limit(10).order_by_score())
        .unwrap()
    {
        let doc = load_document(&searcher, address).unwrap();
        assert!(
            doc.get_first(engine.schema.get_field(TEXT_STORED_FIELD).unwrap())
                .is_some(),
            "rehydrated line_note text must actually be stored"
        );
    }
    let hits = exact(&engine, "מפורשת");
    assert_eq!(hits.len(), 1);
    assert_note(&hits[0], "מפורשת");
    assert!(hits[0]
        .text
        .contains("<otzaria-note-marker>12</otzaria-note-marker>"));
    let (mut stored, _stored_dir) = self::engine();
    add_book(&mut stored, "id:1", &raw, TextStorage::InIndex);
    assert_eq!(hits[0].text, exact(&stored, "מפורשת")[0].text);
}

#[test]
fn note_provenance_stale_library_html_is_not_annotated_as_the_old_layout() {
    let _guard = crate::external_text_tests::guard();
    let db_dir = TempDir::new().unwrap();
    let (db, raw) = line_note_fixture(&db_dir);
    // LibraryDb reads the row as stored. Inline HTML is a valid source for this
    // path; the split line_note representation uses InIndex, as tested above.
    db.execute("DELETE FROM line_note", []).unwrap();
    db.execute("UPDATE line SET content=?1 WHERE id=1", [&raw])
        .unwrap();
    let (mut engine, _dir) = engine();
    add_book(&mut engine, "id:1", &raw, TextStorage::LibraryDb);
    let searcher = engine.index_reader.searcher();
    for (_, address) in searcher
        .search(&AllQuery, &TopDocs::with_limit(10).order_by_score())
        .unwrap()
    {
        let doc = load_document(&searcher, address).unwrap();
        assert!(
            doc.get_first(engine.schema.get_field(TEXT_STORED_FIELD).unwrap())
                .is_none(),
            "fixture must actually read LibraryDb"
        );
    }
    let fresh = exact(&engine, "מפורשת");
    assert_eq!(fresh.len(), 1);
    assert_note(&fresh[0], "מפורשת");
    assert!(fresh[0]
        .text
        .contains("<otzaria-note-marker>12</otzaria-note-marker>"));
    // Same searchable words, moved out of their note: lineCheck, not only lineHash,
    // must reject the old provenance before anything gets decorated.
    let changed = "גוף 12 מפורשת ניכרת סוף";
    assert_eq!(
        hebrew_query::normalize_text_for_indexing(&raw),
        hebrew_query::normalize_text_for_indexing(changed)
    );
    assert_ne!(
        crate::line_source::line_check(&raw),
        crate::line_source::line_check(changed)
    );
    db.execute("UPDATE line SET content=?1 WHERE id=1", [changed])
        .unwrap();
    let hits = exact(&engine, "מפורשת");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].text_status, TextStatus::Stale);
    assert!(!hits[0].text.contains(NOTE) && !hits[0].text.contains(MARKER));
    assert!(!hits[0].text.contains("<font"));
}

#[test]
fn note_provenance_semantic_lexical_fallback_does_not_escape_note_markup() {
    let (mut engine, _dir) = engine();
    add_book(&mut engine, BOOK, RAW, TextStorage::InIndex);
    let response = engine
        .search_semantic(
            "מפורשת".into(),
            vec![],
            10,
            0,
            SemanticLexicalMode::Exact,
            0,
            SemanticRetrievalMode::Hybrid,
            None,
            false,
            false,
            None,
            &SemanticCancellationToken::new(),
        )
        .unwrap();
    assert_eq!(response.results.len(), 1);
    let hit = &response.results[0];
    assert!(hit.is_highlighted);
    assert!(
        hit.snippet_html
            .contains("<otzaria-note><font color=red>מפורשת</font>"),
        "{}",
        hit.snippet_html
    );
    assert!(!hit.snippet_html.contains("&lt;otzaria-note"));
}
