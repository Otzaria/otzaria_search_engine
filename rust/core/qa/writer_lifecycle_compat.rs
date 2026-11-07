use otzaria_search_core::api::search_engine::{SearchEngine, TextStorage};

#[test]
fn writer_close_and_read_only_are_independent_permanent_guards() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let mut writer = SearchEngine::new(path);
    writer.set_economy_indexing(true).unwrap();
    writer
        .add_text_book(
            "device".into(),
            "/devices".into(),
            "id:1".into(),
            "מחשב חדש".into(),
            None,
            TextStorage::InIndex,
            None,
            None,
        )
        .unwrap();
    writer.commit().unwrap();
    let mut reader = SearchEngine::open_read_only(path).unwrap();
    reader.close_index_writer();
    reader.close_index_writer();
    assert!(reader
        .open_index_writer()
        .unwrap_err()
        .to_string()
        .contains("read-only"));
    assert!(reader
        .set_book_order(vec![])
        .unwrap_err()
        .to_string()
        .contains("read-only"));
    writer.close_index_writer();
    writer.close_index_writer();
    assert!(writer
        .open_index_writer()
        .unwrap_err()
        .to_string()
        .contains("permanently closed"));
    assert!(writer
        .commit()
        .unwrap_err()
        .to_string()
        .contains("permanently closed"));
    assert!(writer
        .add_text_book(
            "queued".into(),
            "/devices".into(),
            "id:2".into(),
            "מחשב ישן".into(),
            None,
            TextStorage::InIndex,
            None,
            None
        )
        .is_err());
    let mut replacement_writer = SearchEngine::new(path);
    replacement_writer.open_index_writer().unwrap();
    assert_eq!(reader.get_document_count(), 1);
    reader.refresh_reader().unwrap();
    assert_eq!(reader.get_document_count(), 1);
}
