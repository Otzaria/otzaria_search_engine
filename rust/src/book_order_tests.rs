//! Book numbers, the order mapping and the book stamps, through the engine.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use tempfile::TempDir;

use crate::api::search_engine::*;
use crate::book_order::{BOOK_NUMBERS_FILE, BOOK_ORDER_FILE};

const WORD: &str = "שלום";

fn open(dir: &Path) -> SearchEngine {
    SearchEngine::new(dir.to_str().unwrap())
}

/// A book of `lines` lines, each holding [`WORD`].
fn add_book(engine: &mut SearchEngine, path: &str, lines: usize) {
    let text = (0..lines)
        .map(|n| format!("{WORD} שורה {n} של {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    engine
        .add_text_book(
            format!("ספר {path}"),
            "/root".to_string(),
            path.to_string(),
            text,
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap();
}

fn numbers(engine: &SearchEngine) -> HashMap<String, u32> {
    engine
        .get_book_stamps()
        .unwrap()
        .into_iter()
        .map(|book| (book.file_path, book.book_number))
        .collect()
}

fn place(path: &str, catalogue_order: u32, generation_order: u32) -> BookOrderEntry {
    BookOrderEntry {
        file_path: path.to_string(),
        catalogue_order,
        generation_order,
    }
}

/// The books of the hits of [`WORD`] in `order`, each once, in the order they first come.
fn books_in(engine: &SearchEngine, order: ResultsOrder) -> Vec<String> {
    let hits = engine
        .search_exact(WORD.to_string(), vec![], 100, 0, order, false, false, None)
        .unwrap();
    first_seen(hits.iter().map(|hit| hit.file_path.clone()))
}

fn first_seen(paths: impl Iterator<Item = String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for path in paths {
        if seen.last() != Some(&path) {
            assert!(!seen.contains(&path), "{path} comes in two runs");
            seen.push(path);
        }
    }
    seen
}

#[test]
fn book_numbers_survive_deletion_rewriting_and_reopening_and_are_never_reused() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for path in ["a", "b", "c"] {
        add_book(&mut engine, path, 2);
    }
    engine.commit().unwrap();
    let first = numbers(&engine);
    assert_eq!(
        first,
        [("a", 1), ("b", 2), ("c", 3)]
            .map(|(p, n)| (p.into(), n))
            .into()
    );

    // A book rewritten in place keeps its number, in the same commit or a later one.
    engine.delete_documents_by_file_path("a").unwrap();
    add_book(&mut engine, "a", 3);
    engine.commit().unwrap();
    engine.delete_documents_by_file_path("b").unwrap();
    engine.commit().unwrap();
    add_book(&mut engine, "b", 1);
    engine.commit().unwrap();
    assert_eq!(numbers(&engine), first);

    // A book gone from the index frees no number: the next book gets a new one.
    engine.delete_documents_by_file_path("c").unwrap();
    engine.commit().unwrap();
    add_book(&mut engine, "d", 1);
    engine.commit().unwrap();
    assert_eq!(numbers(&engine)["d"], 4);
    drop(engine);

    // Reopened, the numbers are read back from the index.
    let mut engine = open(dir.path());
    let reopened = numbers(&engine);
    assert_eq!(
        reopened,
        [("a", 1), ("b", 2), ("d", 4)]
            .map(|(p, n)| (p.into(), n))
            .into()
    );
    add_book(&mut engine, "a", 2);
    add_book(&mut engine, "c", 1);
    add_book(&mut engine, "e", 1);
    engine.commit().unwrap();
    let after = numbers(&engine);
    assert_eq!(after["a"], 1, "a book still in the index keeps its number");
    for path in ["c", "e"] {
        assert!(after[path] > 4, "{path} got {} again", after[path]);
    }
    assert_ne!(after["c"], after["e"]);
}

#[test]
fn a_number_is_reserved_on_disk_before_a_document_holds_it() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    add_book(&mut engine, "a", 1);
    engine.commit().unwrap();
    assert!(dir.path().join(BOOK_NUMBERS_FILE).is_file());
    add_book(&mut engine, "b", 1);
    // Never committed: no document of the index holds b's number.
    engine.rollback().unwrap();
    drop(engine);

    let mut engine = open(dir.path());
    add_book(&mut engine, "c", 1);
    engine.commit().unwrap();
    assert!(
        numbers(&engine)["c"] > 2,
        "the number b was given is not handed out again"
    );
}

#[test]
fn the_last_book_number_is_refused() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    engine.force_book_number_for_tests("last", u32::MAX - 1);
    let error = engine
        .add_text_book(
            "t".to_string(),
            "/root".to_string(),
            "after".to_string(),
            "שורה".to_string(),
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap_err();
    assert!(format!("{error}").contains("handed out"), "{error}");
}

#[test]
fn the_catalogue_and_the_generations_are_the_mapping_s_and_change_without_reindexing() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for path in ["a", "b", "c", "d"] {
        add_book(&mut engine, path, 3);
    }
    engine.commit().unwrap();
    // No mapping yet: the order books were first written in.
    assert_eq!(
        books_in(&engine, ResultsOrder::Catalogue),
        ["a", "b", "c", "d"]
    );
    assert_eq!(engine.book_order_revision(), 0);

    let stamps = engine.get_book_stamps().unwrap();
    let revision = engine
        .set_book_order(vec![place("c", 5, 1), place("a", 9, 0), place("b", 7, 1)])
        .unwrap();
    assert_eq!(revision, 1);
    // A book absent from the mapping comes after every book in it.
    assert_eq!(
        books_in(&engine, ResultsOrder::Catalogue),
        ["c", "b", "a", "d"]
    );
    // By generation, then the catalogue within one.
    assert_eq!(
        books_in(&engine, ResultsOrder::Generation),
        ["a", "c", "b", "d"]
    );

    // Moving a book is a new mapping, not a reindex: no document changed.
    engine
        .set_book_order(vec![place("d", 0, 0), place("a", 1, 2), place("c", 2, 2)])
        .unwrap();
    assert_eq!(engine.book_order_revision(), 2);
    assert_eq!(
        books_in(&engine, ResultsOrder::Catalogue),
        ["d", "a", "c", "b"]
    );
    assert_eq!(
        books_in(&engine, ResultsOrder::Generation),
        ["d", "a", "c", "b"]
    );
    assert_eq!(engine.get_book_stamps().unwrap(), stamps);

    // Lines of one book stay in line order.
    let lines: Vec<u64> = engine
        .search_exact(
            WORD.to_string(),
            vec![],
            100,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
        )
        .unwrap()
        .iter()
        .filter(|hit| hit.file_path == "a")
        .map(|hit| hit.id)
        .collect();
    assert_eq!(lines, [(1u64 << 32) + 1, (1 << 32) + 2, (1 << 32) + 3]);
}

#[test]
fn a_new_mapping_is_never_answered_from_the_page_cache() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for path in ["a", "b"] {
        add_book(&mut engine, path, 2);
    }
    engine.commit().unwrap();
    let page = |engine: &SearchEngine| {
        let page = engine
            .search_and_count_exact(
                WORD.to_string(),
                vec![],
                1,
                0,
                ResultsOrder::Catalogue,
                false,
                false,
                None,
            )
            .unwrap();
        (page.total_count, page.results[0].file_path.clone())
    };
    // Twice, so the second comes from the cache.
    assert_eq!(page(&engine), (4, "a".to_string()));
    assert_eq!(page(&engine), (4, "a".to_string()));
    engine
        .set_book_order(vec![place("b", 0, 0), place("a", 1, 0)])
        .unwrap();
    assert_eq!(page(&engine), (4, "b".to_string()));
}

#[test]
fn grouped_and_regex_searches_sort_by_the_mapping() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for path in ["a", "b", "c"] {
        add_book(&mut engine, path, 2);
    }
    engine.commit().unwrap();
    engine
        .set_book_order(vec![place("c", 0, 1), place("b", 1, 0), place("a", 2, 1)])
        .unwrap();

    for (order, expected) in [
        (ResultsOrder::Catalogue, ["c", "b", "a"]),
        (ResultsOrder::Generation, ["b", "c", "a"]),
    ] {
        let grouped = engine
            .search_and_count_exact(
                WORD.to_string(),
                vec![],
                100,
                0,
                order,
                false,
                false,
                Some(ResultGrouping::SameSection),
            )
            .unwrap();
        assert_eq!(
            first_seen(grouped.results.iter().map(|hit| hit.file_path.clone())),
            expected
        );
    }

    let regex = |raw: &str, order: ResultsOrder| {
        let mut events = Vec::new();
        engine
            .user_regex_stream_into(
                raw,
                &[],
                100,
                0,
                &order,
                10,
                &AtomicBool::new(false),
                &mut |u| {
                    events.push(u);
                    true
                },
            )
            .unwrap();
        first_seen(
            events
                .iter()
                .flat_map(|event| event.results.iter())
                .map(|hit| hit.file_path.clone()),
        )
    };
    // Term and word-sequence patterns sort as asked; a line pattern has the catalogue's.
    assert_eq!(regex("/של.ם/", ResultsOrder::Catalogue), ["c", "b", "a"]);
    assert_eq!(regex("/של.ם/", ResultsOrder::Generation), ["b", "c", "a"]);
    assert_eq!(
        regex(r"/שלום\s+שורה/", ResultsOrder::Generation),
        ["b", "c", "a"]
    );
    assert_eq!(
        regex(r"/שלום\W+שורה/", ResultsOrder::Catalogue),
        ["c", "b", "a"]
    );

    // The line search's cache knows the mapping it ranked by.
    engine
        .set_book_order(vec![place("a", 0, 0), place("b", 1, 0), place("c", 2, 0)])
        .unwrap();
    assert_eq!(
        regex(r"/שלום\W+שורה/", ResultsOrder::Catalogue),
        ["a", "b", "c"]
    );
}

#[test]
fn the_mapping_lives_beside_the_index() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for path in ["a", "b"] {
        add_book(&mut engine, path, 1);
    }
    engine.commit().unwrap();
    let mapping = vec![place("b", 0, 0), place("a", 1, 0)];
    assert_eq!(engine.set_book_order(mapping.clone()).unwrap(), 1);
    // The same mapping again changes nothing.
    assert_eq!(engine.set_book_order(mapping).unwrap(), 1);
    // A book given twice is refused, and the mapping kept.
    assert!(engine
        .set_book_order(vec![place("a", 0, 0), place("a", 1, 0)])
        .is_err());
    assert_eq!(engine.book_order_revision(), 1);
    assert!(dir.path().join(BOOK_ORDER_FILE).is_file());
    drop(engine);

    let engine = open(dir.path());
    assert_eq!(engine.book_order_revision(), 1);
    assert_eq!(books_in(&engine, ResultsOrder::Catalogue), ["b", "a"]);
    drop(engine);

    // An engine that writes nothing (the MCP's) sorts by it too.
    let index = tantivy::Index::open_in_dir(dir.path()).unwrap();
    let reader = SearchEngine::from_read_only_index(index, dir.path()).unwrap();
    assert_eq!(books_in(&reader, ResultsOrder::Catalogue), ["b", "a"]);
}

#[test]
fn every_write_path_records_the_book_s_stamps() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    let facets = || Some(vec!["/era/ב".to_string(), "/era/א".to_string()]);
    let text = "<h1>ספר</h1>\nשורה";
    engine
        .add_text_book(
            "ספר".to_string(),
            "/root".to_string(),
            "text".to_string(),
            text.to_string(),
            facets(),
            TextStorage::InIndex,
            None,
            Some(11),
        )
        .unwrap();
    engine
        .add_text_book_bytes(
            "בתים".to_string(),
            "/root".to_string(),
            "bytes".to_string(),
            text.as_bytes().to_vec(),
            None,
            TextStorage::InIndex,
            None,
            Some(12),
        )
        .unwrap();
    engine
        .add_pdf_book(
            "סרוק".to_string(),
            "/pdf".to_string(),
            "scan.pdf".to_string(),
            vec![PdfPageInput {
                reference: "עמוד 1".to_string(),
                text: "שורה של ממש בעמוד".to_string(),
                page_index: 0,
            }],
            facets(),
        )
        .unwrap();
    // An empty-book marker: its id is the engine's.
    engine
        .add_documents_batch(vec![DocumentInput {
            ordinal: 0,
            title: "ריק".to_string(),
            reference: String::new(),
            topics: "/root".to_string(),
            text: String::new(),
            segment: 0,
            is_pdf: false,
            file_path: "marker".to_string(),
            content_hash: Some(21),
            text_hash: Some(22),
            source_hash: Some(23),
            text_vocalized: None,
            section_ordinal: None,
            extra_facets: None,
            text_storage: None,
        }])
        .unwrap();
    engine
        .add_document(
            0, "בודד", "r", "/root", "שורה", 0, false, "single", None, None,
        )
        .unwrap();
    engine.commit().unwrap();

    let stamps: HashMap<String, BookStamps> = engine
        .get_book_stamps()
        .unwrap()
        .into_iter()
        .map(|book| (book.file_path.clone(), book))
        .collect();
    let metadata = |title: &str, topics: &str, facets: Option<Vec<String>>| {
        compute_book_metadata_fingerprint(title.to_string(), topics.to_string(), facets)
    };
    let expect = |path: &str, number, documents, content, text_hash, metadata, source| {
        assert_eq!(
            stamps[path],
            BookStamps {
                file_path: path.to_string(),
                book_number: number,
                documents,
                content_hash: content,
                text_hash,
                metadata_hash: metadata,
                source_hash: source,
            }
        );
    };
    let content = |title: &str, facets| {
        compute_book_fingerprint(
            text.to_string(),
            title.to_string(),
            "/root".to_string(),
            facets,
        )
    };
    let text_hash = compute_content_fingerprint(text.to_string());
    expect(
        "text",
        1,
        2,
        content("ספר", facets()),
        text_hash,
        metadata("ספר", "/root", facets()),
        11,
    );
    expect(
        "bytes",
        2,
        2,
        content("בתים", None),
        text_hash,
        metadata("בתים", "/root", None),
        12,
    );
    expect(
        "scan.pdf",
        3,
        1,
        0,
        0,
        metadata("סרוק", "/pdf", facets()),
        0,
    );
    expect("marker", 4, 1, 21, 22, metadata("ריק", "/root", None), 23);
    expect("single", 5, 1, 0, 0, metadata("בודד", "/root", None), 0);
    assert_eq!(
        engine
            .get_document_by_id((4u64 << 32) + 1)
            .unwrap()
            .unwrap()
            .file_path,
        "marker"
    );
    // The facets' order is not the book's metadata.
    assert_eq!(
        metadata("ספר", "/root", facets()),
        metadata(
            "ספר",
            "/root",
            Some(vec![
                "/era/א".to_string(),
                "/era/ב".to_string(),
                "/era/א".to_string()
            ])
        )
    );
    assert_ne!(
        metadata("ספר", "/root", None),
        metadata("ספר", "/root2", None)
    );
}

#[test]
fn stamps_that_disagree_prove_nothing() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    let marker = |ordinal, source| DocumentInput {
        ordinal,
        title: "ספר".to_string(),
        reference: String::new(),
        topics: "/root".to_string(),
        text: "שורה".to_string(),
        segment: 0,
        is_pdf: false,
        file_path: "book".to_string(),
        content_hash: None,
        text_hash: None,
        source_hash: Some(source),
        text_vocalized: None,
        section_ordinal: None,
        extra_facets: None,
        text_storage: None,
    };
    engine.add_documents_batch(vec![marker(0, 1)]).unwrap();
    engine.add_documents_batch(vec![marker(1, 2)]).unwrap();
    engine.commit().unwrap();
    let stamps = engine.get_book_stamps().unwrap();
    assert_eq!(stamps.len(), 1);
    assert_eq!(stamps[0].source_hash, 0);
    assert_ne!(stamps[0].metadata_hash, 0, "the metadata agrees");
    assert_eq!(stamps[0].documents, 2);
}

/// The book number of `path` in `engine`'s index.
fn number_of(engine: &SearchEngine, path: &str) -> u32 {
    numbers(engine)[path]
}

#[test]
fn engines_that_take_turns_with_the_writer_never_share_a_number() {
    let dir = TempDir::new().unwrap();
    let mut first = open(dir.path());
    let mut second = open(dir.path());
    assert!(first.holds_writer_for_tests());
    assert!(!second.holds_writer_for_tests());
    // The second engine reads the (empty) numbers before the first one writes.
    second.warm_book_order().unwrap();

    add_book(&mut first, "x", 1);
    first.commit().unwrap();
    drop(first);
    add_book(&mut second, "y", 1);
    second.commit().unwrap();
    second.reload_reader_for_tests();
    assert_ne!(number_of(&second, "x"), number_of(&second, "y"));

    // A number handed out and never committed is not handed out again either.
    let third = open(dir.path());
    drop(second);
    let fourth = open(dir.path());
    assert!(!third.holds_writer_for_tests() || !fourth.holds_writer_for_tests());
    let (mut writer, mut other) = if third.holds_writer_for_tests() {
        (third, fourth)
    } else {
        (fourth, third)
    };
    add_book(&mut writer, "z", 1);
    let lost = writer.book_number_for_tests("z");
    drop(writer);
    add_book(&mut other, "w", 1);
    other.commit().unwrap();
    other.reload_reader_for_tests();
    let taken: Vec<u32> = numbers(&other).into_values().collect();
    assert_eq!(taken.len(), 3, "{taken:?}");
    assert!(!taken.contains(&lost), "{lost} handed out twice: {taken:?}");
    let mut sorted = taken.clone();
    sorted.dedup();
    assert_eq!(sorted.len(), 3);
}

#[test]
fn a_reader_follows_the_books_and_the_mapping_another_engine_writes() {
    let dir = TempDir::new().unwrap();
    let mut writer = open(dir.path());
    for path in ["a", "b"] {
        add_book(&mut writer, path, 2);
    }
    writer.commit().unwrap();
    writer
        .set_book_order(vec![place("b", 0, 0), place("a", 1, 0)])
        .unwrap();
    let reader = open(dir.path());
    assert!(!reader.holds_writer_for_tests());
    assert_eq!(books_in(&reader, ResultsOrder::Catalogue), ["b", "a"]);

    add_book(&mut writer, "c", 2);
    writer.commit().unwrap();
    writer
        .set_book_order(vec![place("c", 0, 0), place("b", 1, 0), place("a", 2, 0)])
        .unwrap();
    reader.reload_reader_for_tests();
    assert_eq!(books_in(&reader, ResultsOrder::Catalogue), ["c", "b", "a"]);
    assert_eq!(reader.book_order_revision(), writer.book_order_revision());

    // A book only the reader's new segment holds, absent from the mapping, sorts last.
    add_book(&mut writer, "d", 1);
    writer.commit().unwrap();
    reader.reload_reader_for_tests();
    assert_eq!(
        books_in(&reader, ResultsOrder::Catalogue),
        ["c", "b", "a", "d"]
    );
}

#[test]
fn warming_prepares_the_order_without_a_search() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for path in ["a", "b"] {
        add_book(&mut engine, path, 1);
    }
    engine.commit().unwrap();
    engine
        .set_book_order(vec![place("b", 0, 0), place("a", 1, 0)])
        .unwrap();
    drop(engine);
    let reader = open(dir.path());
    reader.warm_book_order().unwrap();
    let (_, warmed) = reader.order_tables_for_tests();
    let (_, again) = reader.order_tables_for_tests();
    assert!(
        std::sync::Arc::ptr_eq(&warmed, &again),
        "the warmed tables are reused"
    );
    assert_eq!(books_in(&reader, ResultsOrder::Catalogue), ["b", "a"]);
}

#[test]
fn lines_written_one_at_a_time_stamp_their_book_once() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for ordinal in 0..4 {
        engine
            .add_document(
                ordinal, "ספר", "r", "/root", "שורה", 0, false, "book", None, None,
            )
            .unwrap();
    }
    engine
        .upsert_document(
            2,
            "ספר",
            "r",
            "/root",
            "שורה חדשה",
            0,
            false,
            "book",
            None,
            None,
        )
        .unwrap();
    engine.commit().unwrap();
    let searcher = engine.searcher_for_tests();
    let stamped: usize = searcher
        .segment_readers()
        .iter()
        .map(|reader| {
            let column = reader.fast_fields().u64("metadataHash").unwrap();
            reader
                .doc_ids_alive()
                .filter(|&doc| column.first(doc).is_some())
                .count()
        })
        .sum();
    assert_eq!(stamped, 1);
    let stamps = engine.get_book_stamps().unwrap();
    assert_eq!(stamps[0].documents, 4);
    assert_eq!(
        stamps[0].metadata_hash,
        compute_book_metadata_fingerprint("ספר".to_string(), "/root".to_string(), None)
    );
}

#[test]
fn equal_relevance_in_a_grouped_search_goes_by_the_catalogue() {
    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    for path in ["a", "b", "c"] {
        engine
            .add_text_book(
                "ספר".to_string(),
                "/root".to_string(),
                path.to_string(),
                format!("{WORD} עולם"),
                None,
                TextStorage::InIndex,
                None,
                None,
            )
            .unwrap();
    }
    engine.commit().unwrap();
    engine
        .set_book_order(vec![place("c", 0, 0), place("a", 1, 0), place("b", 2, 0)])
        .unwrap();
    let grouped = engine
        .search_and_count_exact(
            WORD.to_string(),
            vec![],
            10,
            0,
            ResultsOrder::Relevance,
            false,
            false,
            Some(ResultGrouping::SameSection),
        )
        .unwrap();
    assert_eq!(
        first_seen(grouped.results.iter().map(|hit| hit.file_path.clone())),
        ["c", "a", "b"]
    );
}
