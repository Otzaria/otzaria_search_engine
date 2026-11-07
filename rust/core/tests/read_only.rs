//! Host contracts: opening is fallible, searches preserve the writer's snapshot,
//! and a read-only handle cannot reach any public lexical mutation path.
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use otzaria_search_core::api::library_indexing::IndexingCancellationToken;
use otzaria_search_core::api::search_engine::{
    BookOrderEntry, ResultsOrder, SearchEngine, SearchResult, SemanticArtifactInput,
    SemanticCancellationToken, SemanticErrorKind, SemanticVectorsInstallInput, TextStorage,
};
use tempfile::TempDir;

fn fixture() -> (TempDir, SearchEngine) {
    let dir = TempDir::new().unwrap();
    let mut writer = SearchEngine::new(dir.path().to_str().unwrap());
    writer.set_economy_indexing(true).unwrap();
    for (path, text) in [("book-a", "מחשב מהיר\nמסך כחול"), ("book-b", "מחשב קטן")]
    {
        writer
            .add_text_book(
                path.into(),
                "/fixture".into(),
                path.into(),
                text.into(),
                None,
                TextStorage::InIndex,
                None,
                None,
            )
            .unwrap();
    }
    writer.commit().unwrap();
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join("otzaria_index_meta.json")).unwrap())
            .unwrap();
    assert_eq!(metadata["schema_version"], 11);
    (dir, writer)
}

fn results(engine: &SearchEngine, order: ResultsOrder) -> Vec<SearchResult> {
    engine
        .search_and_count_exact(
            "מחשב".into(),
            vec![],
            20,
            0,
            order,
            false,
            false,
            None,
            None,
        )
        .unwrap()
        .results
}

fn projection(results: Vec<SearchResult>) -> Vec<(u64, String, String)> {
    results
        .into_iter()
        .map(|r| (r.id, r.file_path, r.text))
        .collect()
}

fn files(path: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            assert!(path.is_file(), "unexpected fixture directory {path:?}");
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect()
}

#[test]
fn opening_preserves_ids_order_snippets_and_every_index_file() {
    let (dir, writer) = fixture();
    let expected = projection(results(&writer, ResultsOrder::Catalogue));
    assert_eq!(expected.len(), 2);
    assert!(expected
        .iter()
        .all(|r| r.2.contains("<font color=red>מחשב</font>")));
    // Windows prevents even reading the writer's byte-range-locked file. Take
    // complete byte snapshots with that handle closed, then reopen it to keep
    // the reader/writer coexistence contract exercised on every platform.
    drop(writer);
    let before = files(dir.path());
    let writer = SearchEngine::new(dir.path().to_str().unwrap());
    // The writer remains open: a read-only instance must not contend for its lock.
    let reader = SearchEngine::open_read_only(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(
        projection(results(&reader, ResultsOrder::Catalogue)),
        expected
    );
    drop(reader);
    assert_eq!(writer.get_document_count(), 3);
    drop(writer);
    assert_eq!(files(dir.path()), before);
}

#[test]
fn schema_11_read_only_search_retains_note_provenance_and_highlights() {
    let (dir, mut writer) = fixture();
    writer
        .add_text_book(
            "Device with note".into(),
            "/fixture".into(),
            "id:303".into(),
            "מחשב<sup class=\"footnote-marker\">12</sup><i class=\"footnote\">מואר היטב</i> קטן"
                .into(),
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap();
    writer.commit().unwrap();
    let query = |engine: &SearchEngine| {
        projection(
            engine
                .search_exact(
                    "מואר".into(),
                    vec![],
                    20,
                    0,
                    ResultsOrder::Catalogue,
                    false,
                    false,
                    None,
                    None,
                )
                .unwrap(),
        )
    };
    let expected = query(&writer);
    assert_eq!(expected.len(), 1);
    assert!(expected[0]
        .2
        .contains("<otzaria-note-marker>12</otzaria-note-marker>"));
    assert!(expected[0]
        .2
        .contains("<otzaria-note><font color=red>מואר</font> היטב</otzaria-note>"));
    // As above, retain all lock files in the byte comparison without reading a
    // writer's locked bytes on Windows. Search still runs beside a live writer.
    drop(writer);
    let before = files(dir.path());
    let writer = SearchEngine::new(dir.path().to_str().unwrap());
    let reader = SearchEngine::open_read_only(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(query(&reader), expected);
    drop(reader);
    assert_eq!(writer.get_document_count(), 4);
    drop(writer);
    assert_eq!(files(dir.path()), before);
}

#[test]
fn every_lexical_mutation_is_rejected_before_work_or_lock_acquisition() {
    let (dir, writer) = fixture();
    drop(writer);
    let mut reader = SearchEngine::open_read_only(dir.path().to_str().unwrap()).unwrap();
    let before = files(dir.path());
    let mut refused = Vec::new();
    macro_rules! reject {
        ($name:literal, $call:expr) => {{
            let error = $call.err().expect($name);
            assert!(
                error.to_string().contains("read-only"),
                "{}: {error:#}",
                $name
            );
            refused.push($name);
        }};
    }
    reject!("open", reader.open_index_writer());
    reject!("commit", reader.commit());
    reject!("rollback", reader.rollback());
    reject!("clear", reader.clear());
    reject!("delete id", reader.delete_document_by_id(1));
    reject!(
        "delete file",
        reader.delete_documents_by_file_path("book-a")
    );
    reject!(
        "delete files",
        reader.delete_documents_by_file_paths(vec![])
    );
    reject!("delete title", reader.remove_documents_by_title("book-a"));
    reject!(
        "document",
        reader.add_document(0, "x", "", "/x", "x", 0, false, "x", None, None)
    );
    reject!(
        "upsert document",
        reader.upsert_document(0, "x", "", "/x", "x", 0, false, "x", None, None)
    );
    reject!("batch", reader.add_documents_batch(vec![]));
    reject!("upsert batch", reader.upsert_documents_batch(vec![]));
    reject!(
        "book",
        reader.add_text_book(
            "x".into(),
            "/x".into(),
            "x".into(),
            String::new(),
            None,
            TextStorage::InIndex,
            None,
            None
        )
    );
    reject!(
        "book bytes",
        reader.add_text_book_bytes(
            "x".into(),
            "/x".into(),
            "x".into(),
            vec![],
            None,
            TextStorage::InIndex,
            None,
            None
        )
    );
    reject!(
        "pdf",
        reader.add_pdf_book("x".into(), "/x".into(), "x".into(), vec![], None)
    );
    reject!(
        "pdf bytes",
        reader.add_pdf_book_bytes(
            "x".into(),
            "/x".into(),
            "x".into(),
            vec![],
            vec![1],
            vec![],
            None
        )
    );
    reject!(
        "library",
        reader.add_library_books(vec![], &IndexingCancellationToken::new())
    );
    reject!("bulk", reader.set_bulk_indexing(true));
    reject!("economy", reader.set_economy_indexing(true));
    reject!("order", reader.set_book_order(vec![]));
    reject!("optimize", reader.optimize());
    assert_eq!(refused.len(), 21);
    assert_eq!(files(dir.path()), before);
    assert_eq!(results(&reader, ResultsOrder::Catalogue).len(), 2);
    // A denied write never took the writer lock, even while the reader stays alive.
    let mut other_writer = SearchEngine::new(dir.path().to_str().unwrap());
    other_writer.set_economy_indexing(true).unwrap();
    other_writer.open_index_writer().unwrap();
}

#[test]
fn read_only_handles_refuse_sidecar_mutation_even_without_an_enabled_backend() {
    let (dir, _writer) = fixture();
    let reader = SearchEngine::open_read_only(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(
        reader
            .reset_semantic_index()
            .err()
            .expect("read-only mutation must fail")
            .kind,
        SemanticErrorKind::ReadOnlySession
    );
    assert_eq!(
        reader
            .remove_semantic_books(vec![])
            .err()
            .expect("read-only mutation must fail")
            .kind,
        SemanticErrorKind::ReadOnlySession
    );
    assert_eq!(
        reader
            .semantic_index_books(vec![])
            .err()
            .expect("read-only mutation must fail")
            .kind,
        SemanticErrorKind::ReadOnlySession
    );
    let error = reader
        .compact_semantic_vectors(
            dir.path()
                .join("must-not-exist")
                .to_string_lossy()
                .into_owned(),
            None,
            None,
            &SemanticCancellationToken::new(),
        )
        .err()
        .expect("read-only compaction must fail");
    assert_eq!(error.kind, SemanticErrorKind::ReadOnlySession);
    let error = reader
        .open_semantic_artifact(SemanticArtifactInput {
            vectors_dir: dir
                .path()
                .join("must-not-exist")
                .to_string_lossy()
                .into_owned(),
            model_path: "absent-model".into(),
            model_identity_json: "invalid identity".into(),
            onnx_runtime_path: None,
            scan_threads: None,
        })
        .err()
        .expect("read-only artifact open must not recover sidecar files");
    assert_eq!(error.kind, SemanticErrorKind::ReadOnlySession);
    let error = reader
        .install_semantic_vectors(
            SemanticVectorsInstallInput {
                vectors_dir: dir
                    .path()
                    .join("must-not-exist")
                    .to_string_lossy()
                    .into_owned(),
                segment_path: "absent-segment".into(),
                manifest_json: "invalid manifest".into(),
                published_manifest_sha256: None,
                model_identity_json: "invalid identity".into(),
            },
            &SemanticCancellationToken::new(),
        )
        .err()
        .expect("read-only installation must fail before validating files");
    assert_eq!(error.kind, SemanticErrorKind::ReadOnlySession);
    let error = reader
        .verify_semantic_vectors(
            dir.path()
                .join("must-not-exist")
                .to_string_lossy()
                .into_owned(),
            &SemanticCancellationToken::new(),
        )
        .err()
        .expect("read-only verification must not persist damage markers");
    assert_eq!(error.kind, SemanticErrorKind::ReadOnlySession);
    assert!(!dir.path().join("must-not-exist").exists());
}

#[test]
fn explicit_reader_refresh_observes_a_commit_and_invalidates_cached_counts() {
    let (dir, mut writer) = fixture();
    let mut reader = SearchEngine::open_read_only(dir.path().to_str().unwrap()).unwrap();
    let before = projection(results(&reader, ResultsOrder::Catalogue));
    writer
        .add_text_book(
            "book-c".into(),
            "/fixture".into(),
            "book-c".into(),
            "מחשב נוסף".into(),
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap();
    writer.commit().unwrap();
    assert_eq!(
        projection(results(&reader, ResultsOrder::Catalogue)),
        before
    );
    reader.refresh_reader().unwrap();
    let after = projection(results(&reader, ResultsOrder::Catalogue));
    assert_eq!(after.len(), 3);
    assert_eq!(&after[..2], before.as_slice());
    assert_eq!(reader.get_document_count(), 4);
}

#[test]
fn concurrent_searches_and_external_order_changes_preserve_stable_ids() {
    let (dir, writer) = fixture();
    let reader = Arc::new(SearchEngine::open_read_only(dir.path().to_str().unwrap()).unwrap());
    let original = projection(results(&reader, ResultsOrder::Catalogue));
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let reader = Arc::clone(&reader);
            let expected = original.clone();
            scope.spawn(move || {
                for _ in 0..16 {
                    assert_eq!(
                        projection(results(&reader, ResultsOrder::Catalogue)),
                        expected
                    );
                }
            });
        }
    });
    writer
        .set_book_order(vec![
            BookOrderEntry {
                file_path: "book-b".into(),
                catalogue_order: 0,
                generation_order: 1,
            },
            BookOrderEntry {
                file_path: "book-a".into(),
                catalogue_order: 1,
                generation_order: 0,
            },
        ])
        .unwrap();
    let mut reversed = original.clone();
    reversed.reverse();
    assert_eq!(
        projection(results(&reader, ResultsOrder::Catalogue)),
        reversed
    );
    assert_eq!(
        projection(results(&reader, ResultsOrder::Generation)),
        original
    );
}

#[test]
fn missing_or_incompatible_indexes_are_refused_without_creation_or_upgrade() {
    let missing = TempDir::new().unwrap().path().join("missing");
    assert!(SearchEngine::open_read_only(missing.to_str().unwrap()).is_err());
    assert!(!missing.exists());
    let (dir, writer) = fixture();
    drop(writer);
    let metadata = dir.path().join("otzaria_index_meta.json");
    let original = fs::read(&metadata).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    // Do not make a legacy/future index writable or silently rebuild it. Claiming
    // schema 10 over this schema-11 fixture must also fail the stored-schema
    // comparison; an actual independent schema-10 index is covered separately.
    for version in [7, 8, 9, 10, 12] {
        value["schema_version"] = version.into();
        fs::write(&metadata, serde_json::to_vec(&value).unwrap()).unwrap();
        let before = files(dir.path());
        assert!(SearchEngine::open_read_only(dir.path().to_str().unwrap()).is_err());
        assert_eq!(files(dir.path()), before);
    }
    fs::remove_file(&metadata).unwrap();
    let before = files(dir.path());
    assert!(SearchEngine::open_read_only(dir.path().to_str().unwrap()).is_err());
    assert_eq!(files(dir.path()), before);
}
