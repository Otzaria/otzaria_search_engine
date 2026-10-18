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

#[test]
fn stale_writer_upsert_uses_the_readded_books_committed_number() {
    let dir = TempDir::new().unwrap();
    let mut writer = open(dir.path());
    add_book(&mut writer, "x", 1);
    writer.commit().unwrap();
    let mut stale = open(dir.path());
    stale.warm_book_order().unwrap();
    writer.delete_documents_by_file_path("x").unwrap();
    writer.commit().unwrap();
    drop(writer);
    let mut replacement = open(dir.path());
    add_book(&mut replacement, "x", 1);
    replacement.commit().unwrap();
    let current_number = number_of(&replacement, "x");
    drop(replacement);
    stale
        .upsert_document(
            0,
            "ספר x",
            "r",
            "/root",
            "שלום חדש",
            0,
            false,
            "x",
            None,
            None,
        )
        .unwrap();
    stale.commit().unwrap();
    let stamps = stale.get_book_stamps().unwrap();
    assert_eq!(stamps[0].documents, 1, "upsert must replace existing line rather than adding a second document; current prefix {current_number}, reported stamps {stamps:?}");
    assert_eq!(stamps[0].book_number, current_number);
}

#[test]
fn set_book_order_compares_with_the_current_disk_mapping() {
    let dir = TempDir::new().unwrap();
    let mut writer = open(dir.path());
    add_book(&mut writer, "a", 1);
    add_book(&mut writer, "b", 1);
    writer.commit().unwrap();
    writer
        .set_book_order(vec![place("a", 0, 0), place("b", 1, 0)])
        .unwrap();
    let stale = open(dir.path());
    assert_eq!(books_in(&stale, ResultsOrder::Catalogue), ["a", "b"]);
    writer
        .set_book_order(vec![place("b", 0, 0), place("a", 1, 0)])
        .unwrap();
    // Push the old mapping intentionally: success must mean this mapping is active.
    stale
        .set_book_order(vec![place("a", 0, 0), place("b", 1, 0)])
        .unwrap();
    assert_eq!(books_in(&stale, ResultsOrder::Catalogue), ["a", "b"]);
}

#[test]
fn stale_reader_sorts_a_readded_book_by_its_current_number() {
    let dir = TempDir::new().unwrap();
    let mut writer = open(dir.path());
    add_book(&mut writer, "a", 1);
    add_book(&mut writer, "x", 1);
    writer.commit().unwrap();
    writer
        .set_book_order(vec![place("x", 0, 0), place("a", 1, 0)])
        .unwrap();
    let stale = open(dir.path());
    stale.warm_book_order().unwrap();
    writer.delete_documents_by_file_path("x").unwrap();
    writer.commit().unwrap();
    drop(writer);
    let mut replacement = open(dir.path());
    add_book(&mut replacement, "x", 1);
    replacement.commit().unwrap();
    drop(replacement);
    stale.reload_reader_for_tests();
    assert_eq!(books_in(&stale, ResultsOrder::Catalogue), ["x", "a"]);
}

#[test]
fn numbers_from_old_and_new_search_snapshots_can_finish_in_either_order() {
    use crate::book_order::{numbers_in_segments, BookNumbers, OrderKind, OrderTables};

    let dir = TempDir::new().unwrap();
    let mut writer = open(dir.path());
    add_book(&mut writer, "x", 1);
    add_book(&mut writer, "a", 1);
    writer.commit().unwrap();
    let old = writer.searcher_for_tests();
    let old_number = number_of(&writer, "x");
    writer.delete_documents_by_file_path("x").unwrap();
    writer.commit().unwrap();
    drop(writer);
    let mut replacement = open(dir.path());
    add_book(&mut replacement, "x", 1);
    replacement.commit().unwrap();
    let new = replacement.searcher_for_tests();
    let new_number = number_of(&replacement, "x");
    assert!(new_number > old_number);

    for snapshots in [[&old, &new], [&new, &old]] {
        let mut numbers = BookNumbers::new(dir.path());
        // Both reads can begin before either one takes the mutex to absorb its result.
        let reads = snapshots.map(|searcher| {
            let unread = numbers.unread_segments(searcher);
            let found = numbers_in_segments(searcher, &unread).unwrap();
            (searcher, unread, found)
        });
        for (searcher, unread, found) in reads {
            numbers.absorb(searcher, &unread, found);
        }
        // Acquiring the writer reconciles against its latest committed snapshot,
        // independently of the order asynchronous search reads finished in.
        assert_eq!(&*numbers.paths()[&old_number], "x");
        assert_eq!(&*numbers.paths()[&new_number], "x");
        numbers.catch_up(&new).unwrap();
        assert_eq!(numbers.number_for("x").unwrap(), new_number);
        let tables = OrderTables::build(
            numbers.paths(),
            &HashMap::from([(
                "x".into(),
                crate::book_order::BookPlace {
                    catalogue_order: 0,
                    generation_order: 0,
                },
            )]),
        );
        let rank = |number| tables.key(OrderKind::Catalogue, u64::from(number) << 32);
        let a = number_of(&replacement, "a");
        assert!(rank(old_number) < rank(a));
        assert!(rank(new_number) < rank(a));
    }
}

#[test]
fn reading_an_old_commit_keeps_a_higher_local_uncommitted_allocation() {
    use crate::book_order::{numbers_in_segments, BookNumbers};

    let dir = TempDir::new().unwrap();
    let mut engine = open(dir.path());
    add_book(&mut engine, "x", 1);
    engine.commit().unwrap();
    let old = engine.searcher_for_tests();
    // Simulate a new writer allocating after the old x was deleted. The pending new
    // document is not visible in the old search snapshot whose read is finishing now.
    let mut numbers = BookNumbers::new(dir.path());
    let pending = numbers.number_for("x").unwrap();
    assert!(pending > number_of(&engine, "x"));
    let unread = numbers.unread_segments(&old);
    let found = numbers_in_segments(&old, &unread).unwrap();
    numbers.absorb(&old, &unread, found);
    assert_eq!(numbers.number_for("x").unwrap(), pending);
    // After releasing and reacquiring the writer, even an already-read live segment
    // overrides the prior allocation. Snapshot reads alone must not do that.
    assert!(numbers.unread_segments(&old).is_empty());
    numbers.catch_up(&old).unwrap();
    assert_eq!(numbers.number_for("x").unwrap(), number_of(&engine, "x"));
}

#[test]
fn simultaneous_mapping_writers_serialize_revisions_across_engine_instances() {
    use std::sync::{Arc, Barrier};

    let dir = TempDir::new().unwrap();
    const WRITERS: usize = 4;
    const UPDATES: usize = 8;
    let engines: Vec<_> = (0..WRITERS).map(|_| open(dir.path())).collect();
    let barrier = Arc::new(Barrier::new(WRITERS));
    let mut revisions = std::thread::scope(|scope| {
        let handles: Vec<_> = engines
            .iter()
            .enumerate()
            .map(|(writer, engine)| {
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    barrier.wait();
                    (0..UPDATES)
                        .map(|update| {
                            engine
                                .set_book_order(vec![place(
                                    "a",
                                    (writer * UPDATES + update) as u32,
                                    0,
                                )])
                                .unwrap()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    revisions.sort_unstable();
    assert_eq!(
        revisions,
        (1..=(WRITERS * UPDATES) as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        open(dir.path()).book_order_revision(),
        (WRITERS * UPDATES) as u64
    );
}

#[test]
fn simultaneous_identical_mappings_are_one_change_and_refreshes_do_not_undo_writes() {
    use std::sync::Barrier;

    let dir = TempDir::new().unwrap();
    let first = open(dir.path());
    let second = open(dir.path());
    let barrier = Barrier::new(2);
    let write = |engine: &SearchEngine| {
        barrier.wait();
        engine.set_book_order(vec![place("a", 1, 0)]).unwrap()
    };
    std::thread::scope(|scope| {
        let a = scope.spawn(|| write(&first));
        let b = scope.spawn(|| write(&second));
        assert_eq!((a.join().unwrap(), b.join().unwrap()), (1, 1));
    });
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for n in 2..=40 {
                assert_eq!(
                    first.set_book_order(vec![place("a", n, 0)]).unwrap(),
                    u64::from(n)
                );
            }
        });
        scope.spawn(|| {
            for _ in 0..80 {
                first.book_order_revision();
                second.book_order_revision();
            }
        });
    });
    assert_eq!(first.set_book_order(vec![place("a", 40, 0)]).unwrap(), 40);
    assert_eq!(second.book_order_revision(), 40);
}

#[test]
fn mapping_updates_are_serialized_between_processes() {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Command, Stdio};

    const CHILD_DIR: &str = "OTZARIA_BOOK_ORDER_TEST_DIR";
    const CHILD_ID: &str = "OTZARIA_BOOK_ORDER_TEST_WRITER";
    const UPDATES: u32 = 8;
    if let (Ok(dir), Ok(id)) = (std::env::var(CHILD_DIR), std::env::var(CHILD_ID)) {
        let engine = open(Path::new(&dir));
        println!("ORDER_WRITER_READY");
        std::io::stdout().flush().unwrap();
        // Both processes have loaded the original revision before the parent releases us.
        std::io::stdin().read_exact(&mut [0]).unwrap();
        let id: u32 = id.parse().unwrap();
        let revisions: Vec<_> = (0..UPDATES)
            .map(|n| {
                engine
                    .set_book_order(vec![place("a", id * UPDATES + n, 0)])
                    .unwrap()
            })
            .collect();
        println!(
            "ORDER_REVISIONS={}",
            serde_json::to_string(&revisions).unwrap()
        );
        return;
    }

    let dir = TempDir::new().unwrap();
    drop(open(dir.path()));
    let mut children: Vec<_> = (0..2)
        .map(|id| {
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "book_order_tests::mapping_updates_are_serialized_between_processes",
                    "--nocapture",
                ])
                .env(CHILD_DIR, dir.path())
                .env(CHILD_ID, id.to_string())
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap();
            let mut output = BufReader::new(child.stdout.take().unwrap());
            let mut line = String::new();
            loop {
                line.clear();
                assert_ne!(
                    output.read_line(&mut line).unwrap(),
                    0,
                    "writer exited before ready"
                );
                if line.trim() == "ORDER_WRITER_READY" {
                    break;
                }
            }
            (child, output)
        })
        .collect();
    for (child, _) in &mut children {
        child.stdin.take().unwrap().write_all(&[1]).unwrap();
    }
    let mut revisions = Vec::<u64>::new();
    for (mut child, mut output) in children {
        let mut text = String::new();
        output.read_to_string(&mut text).unwrap();
        assert!(child.wait().unwrap().success(), "{text}");
        let json = text
            .lines()
            .find_map(|line| line.strip_prefix("ORDER_REVISIONS="))
            .unwrap();
        revisions.extend(serde_json::from_str::<Vec<u64>>(json).unwrap());
    }
    revisions.sort_unstable();
    assert_eq!(revisions, (1..=u64::from(2 * UPDATES)).collect::<Vec<_>>());
    assert_eq!(
        open(dir.path()).book_order_revision(),
        u64::from(2 * UPDATES)
    );
}

#[test]
fn a_later_commit_can_reintroduce_a_lower_historical_number() {
    let dir = TempDir::new().unwrap();
    let mut first = open(dir.path());
    add_book(&mut first, "x", 1);
    first.commit().unwrap();
    let original = number_of(&first, "x");
    let mut old = open(dir.path());
    old.warm_book_order().unwrap();
    first.delete_documents_by_file_path("x").unwrap();
    first.commit().unwrap();
    drop(first);

    let mut replacement = open(dir.path());
    add_book(&mut replacement, "x", 1);
    replacement.commit().unwrap();
    let higher = number_of(&replacement, "x");
    assert!(higher > original);
    let mut observer = open(dir.path());
    observer.warm_book_order().unwrap();
    replacement.delete_documents_by_file_path("x").unwrap();
    replacement.commit().unwrap();
    drop(replacement);

    // old never saw the intermediate higher number. Its historical number is still
    // reserved for x, and it can reuse it when no live x exists in the latest commit.
    add_book(&mut old, "x", 1);
    old.commit().unwrap();
    assert_eq!(number_of(&old, "x"), original);
    drop(old);
    observer
        .upsert_document(
            0,
            "ספר x",
            "r",
            "/root",
            "שלום חדש",
            0,
            false,
            "x",
            None,
            None,
        )
        .unwrap();
    observer.commit().unwrap();
    let stamps = observer.get_book_stamps().unwrap();
    assert_eq!(stamps[0].book_number, original);
    assert_eq!(stamps[0].documents, 1);
}

#[test]
fn failed_takeover_synchronization_is_retried_before_a_number_is_used() {
    let dir = TempDir::new().unwrap();
    let mut writer = open(dir.path());
    add_book(&mut writer, "x", 1);
    writer.commit().unwrap();
    let mut stale = open(dir.path());
    stale.warm_book_order().unwrap();
    writer.delete_documents_by_file_path("x").unwrap();
    writer.commit().unwrap();
    drop(writer);
    let mut replacement = open(dir.path());
    add_book(&mut replacement, "x", 1);
    replacement.commit().unwrap();
    let current = number_of(&replacement, "x");
    drop(replacement);
    stale.open_index_writer().unwrap();

    let meta = dir.path().join("meta.json");
    let backup = dir.path().join("meta.saved");
    std::fs::rename(&meta, &backup).unwrap();
    let failed = stale.book_number("x");
    std::fs::rename(&backup, &meta).unwrap();
    assert!(
        failed.is_err(),
        "a failed reader reload cannot hand out a stale number"
    );
    assert_eq!(stale.book_number("x").unwrap(), current);
}
