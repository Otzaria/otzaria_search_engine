use otzaria_search_core::api::search_engine::{ResultsOrder, SearchEngine, TextStorage};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

#[path = "support/contract.rs"]
mod contract;

fn copy_index(destination: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/canonical-7.0.1/index");
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        assert!(entry.file_type().unwrap().is_file());
        fs::copy(entry.path(), destination.join(entry.file_name())).unwrap();
    }
}

fn files(path: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            (
                path.file_name().unwrap().to_string_lossy().into_owned(),
                fs::read(path).unwrap(),
            )
        })
        .collect()
}

#[test]
fn reads_an_index_built_by_the_unmodified_canonical_7_0_1_engine() {
    let directory = tempfile::tempdir().unwrap();
    copy_index(directory.path());
    let before = files(directory.path());
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/canonical-7.0.1/expected.json")).unwrap();
    let reader = SearchEngine::open_read_only(directory.path().to_str().unwrap()).unwrap();
    assert_eq!(contract::contract(&reader), expected);
    drop(reader);
    assert_eq!(
        files(directory.path()),
        before,
        "read-only search changed an existing index"
    );
}

#[test]
fn dropping_the_reader_allows_a_directory_replacement_and_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let active = directory.path().join("active");
    let replacement = directory.path().join("replacement");
    copy_index(&active);
    let old = SearchEngine::open_read_only(active.to_str().unwrap()).unwrap();
    assert_eq!(old.get_document_count(), 9);
    drop(old);

    // Prepare the replacement before releasing/publishing it, just as a host does.
    fs::create_dir_all(&replacement).unwrap();
    let mut writer = SearchEngine::new(replacement.to_str().unwrap());
    writer.set_economy_indexing(true).unwrap();
    writer
        .add_text_book(
            "Replacement".into(),
            "/devices".into(),
            "id:909".into(),
            "מחשב חדש".into(),
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap();
    writer.commit().unwrap();
    drop(writer);

    fs::rename(&active, directory.path().join("previous")).unwrap();
    fs::rename(&replacement, &active).unwrap();
    let current = SearchEngine::open_read_only(active.to_str().unwrap()).unwrap();
    assert_eq!(current.get_document_count(), 1);
    let results = current
        .search_exact(
            "מחשב".into(),
            vec![],
            20,
            0,
            ResultsOrder::Catalogue,
            false,
            false,
            None,
            None,
        )
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file_path, "id:909");
    assert!(results[0].text.contains("חדש"));
}
