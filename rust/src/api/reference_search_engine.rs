use crate::api::search_engine::ResultsOrder;
use anyhow::Result;
use flutter_rust_bridge::frb;
use log::{debug, warn};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::index::Index;
use tantivy::query::{Query, QueryParser};
use tantivy::schema::Value as TantivyValue;
use tantivy::schema::*;
use tantivy::{doc, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Term};

// ── Public data types ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ReferenceSearchResult {
    pub title: String,
    pub reference: String,
    pub short_ref: String,
    pub id: u64,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
}

pub struct ReferenceDocumentInput {
    pub id: u64,
    pub title: String,
    pub reference: String,
    pub short_ref: String,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
}

// ── ReferenceSearchEngine ──────────────────────────────────────────────────────

pub struct ReferenceSearchEngine {
    schema: Schema,
    index: Index,
    index_writer: IndexWriter,
    index_reader: IndexReader,
}

impl ReferenceSearchEngine {
    #[frb(sync)]
    pub fn new(path: &str) -> Self {
        debug!("new path={}", path);
        let mut schema_builder = Schema::builder();
        schema_builder.add_text_field("reference", TEXT | STORED);
        schema_builder.add_text_field("shortRef", TEXT);
        schema_builder.add_text_field("title", TEXT | STORED);
        // INDEXED is required for delete_term / upsert by id to work.
        schema_builder.add_u64_field("id", STORED | FAST | INDEXED);
        schema_builder.add_u64_field("segment", STORED);
        schema_builder.add_bool_field("isPdf", STORED);
        schema_builder.add_text_field("filePath", TEXT | STORED);

        let schema = schema_builder.build();
        let index = Self::open_or_recreate_index(path, &schema);
        let index_reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::OnCommitWithDelay)
            .try_into()
            .expect("Failed to create index reader");
        let index_writer = index
            .writer(50_000_000)
            .expect("Failed to create index writer");

        ReferenceSearchEngine {
            schema,
            index,
            index_writer,
            index_reader,
        }
    }

    /// Opens the index at `path`, or — when the on-disk schema predates the
    /// current one (e.g. `id` was not INDEXED before delete/upsert-by-id were
    /// added) — wipes and recreates it. The reference index is a derived cache
    /// that the app repopulates when it finds it empty, so recreating beats
    /// panicking on every startup with an old index.
    fn open_or_recreate_index(path: &str, schema: &Schema) -> Index {
        let mmap_directory = MmapDirectory::open(path).expect("unable to open mmap directory");
        match Index::open_or_create(mmap_directory, schema.clone()) {
            Ok(index) => index,
            Err(tantivy::TantivyError::SchemaError(err)) => {
                warn!("reference index schema mismatch ({err}); recreating the index");
                std::fs::remove_dir_all(path).expect("unable to clear reference index directory");
                std::fs::create_dir_all(path)
                    .expect("unable to recreate reference index directory");
                let mmap_directory =
                    MmapDirectory::open(path).expect("unable to open mmap directory");
                Index::open_or_create(mmap_directory, schema.clone())
                    .expect("Failed to create index")
            }
            Err(err) => panic!("Failed to open reference index: {err}"),
        }
    }

    // ── Write API ──────────────────────────────────────────────────────────────

    /// Add a single document. Does not commit.
    pub fn add_document(
        &mut self,
        _id: u64,
        _title: &str,
        _reference: &str,
        _short_ref: &str,
        _segment: u64,
        _is_pdf: bool,
        _file_path: &str,
    ) -> Result<()> {
        let (title_f, reference_f, short_ref_f, id_f, segment_f, is_pdf_f, file_path_f) =
            self.all_fields()?;
        self.index_writer.add_document(doc!(
            title_f     => _title,
            reference_f => _reference,
            short_ref_f => _short_ref,
            id_f        => _id,
            segment_f   => _segment,
            is_pdf_f    => _is_pdf,
            file_path_f => _file_path
        ))?;
        Ok(())
    }

    /// Add many documents in a single FFI call. Does not commit.
    pub fn add_documents_batch(&mut self, docs: Vec<ReferenceDocumentInput>) -> Result<()> {
        let (title_f, reference_f, short_ref_f, id_f, segment_f, is_pdf_f, file_path_f) =
            self.all_fields()?;
        for doc in docs {
            self.index_writer.add_document(doc!(
                title_f     => doc.title,
                reference_f => doc.reference,
                short_ref_f => doc.short_ref,
                id_f        => doc.id,
                segment_f   => doc.segment,
                is_pdf_f    => doc.is_pdf,
                file_path_f => doc.file_path
            ))?;
        }
        Ok(())
    }

    /// Delete then re-insert a single document by id. Does not commit.
    pub fn upsert_document(
        &mut self,
        _id: u64,
        _title: &str,
        _reference: &str,
        _short_ref: &str,
        _segment: u64,
        _is_pdf: bool,
        _file_path: &str,
    ) -> Result<()> {
        self.delete_document_by_id(_id)?;
        self.add_document(
            _id, _title, _reference, _short_ref, _segment, _is_pdf, _file_path,
        )
    }

    /// Upsert many documents in a single FFI call. Does not commit.
    pub fn upsert_documents_batch(&mut self, docs: Vec<ReferenceDocumentInput>) -> Result<()> {
        let (title_f, reference_f, short_ref_f, id_f, segment_f, is_pdf_f, file_path_f) =
            self.all_fields()?;
        for doc in docs {
            self.index_writer
                .delete_term(Term::from_field_u64(id_f, doc.id));
            self.index_writer.add_document(doc!(
                title_f     => doc.title,
                reference_f => doc.reference,
                short_ref_f => doc.short_ref,
                id_f        => doc.id,
                segment_f   => doc.segment,
                is_pdf_f    => doc.is_pdf,
                file_path_f => doc.file_path
            ))?;
        }
        Ok(())
    }

    /// Delete a document by its numeric id. Does not commit.
    pub fn delete_document_by_id(&mut self, id: u64) -> Result<()> {
        let id_f = self.schema.get_field("id").unwrap();
        self.index_writer
            .delete_term(Term::from_field_u64(id_f, id));
        Ok(())
    }

    /// Delete all documents. Does not commit.
    pub fn clear(&self) -> Result<()> {
        self.index_writer.delete_all_documents()?;
        Ok(())
    }

    /// Flush pending writes to disk and refresh the reader.
    pub fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        self.index_reader.reload()?;
        Ok(())
    }

    /// Discard all pending writes since the last commit.
    pub fn rollback(&mut self) -> Result<()> {
        self.index_writer.rollback()?;
        Ok(())
    }

    // ── Search API ─────────────────────────────────────────────────────────────

    pub fn search(
        &mut self,
        query: &str,
        limit: u32,
        fuzzy: bool,
        order: ResultsOrder,
    ) -> Result<Vec<ReferenceSearchResult>> {
        let search_query = Self::build_query(&self.index, query, fuzzy)?;
        let searcher = self.index_reader.searcher();
        let schema = &self.schema;

        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        let short_ref_field = schema.get_field("shortRef")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;

        let addresses: Vec<DocAddress> = match order {
            ResultsOrder::Catalogue => {
                let collector = TopDocs::with_limit(limit as usize)
                    .order_by_fast_field::<u64>("id", Order::Asc);
                searcher
                    .search(&*search_query, &collector)?
                    .into_iter()
                    .map(|(_, addr)| addr)
                    .collect()
            }
            ResultsOrder::Relevance => {
                let collector = TopDocs::with_limit(limit as usize).order_by_score();
                searcher
                    .search(&*search_query, &collector)?
                    .into_iter()
                    .map(|(_, addr)| addr)
                    .collect()
            }
        };

        let mut results = Vec::with_capacity(addresses.len());
        for doc_address in addresses {
            let retrieved_doc = match searcher.doc::<TantivyDocument>(doc_address) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let title = retrieved_doc
                .get_first(title_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let reference = retrieved_doc
                .get_first(reference_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let short_ref = retrieved_doc
                .get_first(short_ref_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let id = retrieved_doc
                .get_first(id_field)
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let segment = retrieved_doc
                .get_first(segment_field)
                .and_then(|v| v.as_u64())
                .unwrap_or_default();
            let is_pdf = retrieved_doc
                .get_first(is_pdf_field)
                .and_then(|v| v.as_bool())
                .unwrap_or_default();
            let file_path = retrieved_doc
                .get_first(file_path_field)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            results.push(ReferenceSearchResult {
                title,
                reference,
                short_ref,
                id,
                segment,
                is_pdf,
                file_path,
            });
        }
        Ok(results)
    }

    pub fn count(&mut self, query: &str, fuzzy: bool) -> Result<u32> {
        let search_query = Self::build_query(&self.index, query, fuzzy)?;
        let searcher = self.index_reader.searcher();
        Ok(searcher.search(&*search_query, &Count)? as u32)
    }

    // ── Private helpers ────────────────────────────────────────────────────────

    fn all_fields(&self) -> Result<(Field, Field, Field, Field, Field, Field, Field)> {
        Ok((
            self.schema.get_field("title")?,
            self.schema.get_field("reference")?,
            self.schema.get_field("shortRef")?,
            self.schema.get_field("id")?,
            self.schema.get_field("segment")?,
            self.schema.get_field("isPdf")?,
            self.schema.get_field("filePath")?,
        ))
    }

    fn build_query(index: &Index, search_term: &str, fuzzy: bool) -> Result<Box<dyn Query>> {
        let schema = index.schema();
        let reference_field = schema.get_field("reference").unwrap();
        let short_ref_field = schema.get_field("shortRef").unwrap();

        let mut qp = QueryParser::for_index(index, vec![reference_field, short_ref_field]);
        qp.set_conjunction_by_default();
        if fuzzy {
            qp.set_field_fuzzy(reference_field, false, 1, false);
            qp.set_field_fuzzy(short_ref_field, false, 1, false);
        }

        Ok(Box::new(qp.parse_query_lenient(search_term).0))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reference_search() -> Result<()> {
        let temp_dir = tempfile::Builder::new()
            .prefix("reference_search_engine_test")
            .tempdir()
            .unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();
        let mut engine = ReferenceSearchEngine::new(temp_path);

        engine
            .add_document(
                1,
                "Document 1",
                "Reference 1",
                "R 1",
                1,
                false,
                "/path/to/doc1",
            )
            .unwrap();
        engine
            .add_document(
                2,
                "Document 2",
                "Reference 2",
                "R 2",
                2,
                false,
                "/path/to/doc2",
            )
            .unwrap();
        engine
            .add_document(
                3,
                "Document 3",
                "Another Reference",
                "A R",
                3,
                false,
                "/path/to/doc3",
            )
            .unwrap();
        engine.commit().unwrap();

        let results = engine
            .search("Reference", 10, false, ResultsOrder::Catalogue)
            .unwrap();
        assert_eq!(results.len(), 3);

        let results = engine
            .search("Reference 1", 10, false, ResultsOrder::Catalogue)
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reference, "Reference 1");

        let results = engine
            .search("Referenc", 10, true, ResultsOrder::Catalogue)
            .unwrap();
        assert!(!results.is_empty());

        let count = engine.count("Reference", false).unwrap();
        assert_eq!(count, 3);

        let count = engine.count("Reference 1", false).unwrap();
        assert_eq!(count, 1);

        Ok(())
    }

    #[test]
    fn test_new_recreates_index_with_old_schema() -> Result<()> {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let path = temp_dir.path().to_str().unwrap();

        // Build an index with the pre-upsert schema (`id` without INDEXED).
        {
            let mut schema_builder = Schema::builder();
            schema_builder.add_text_field("reference", TEXT | STORED);
            schema_builder.add_text_field("shortRef", TEXT);
            schema_builder.add_text_field("title", TEXT | STORED);
            schema_builder.add_u64_field("id", STORED | FAST);
            schema_builder.add_u64_field("segment", STORED);
            schema_builder.add_bool_field("isPdf", STORED);
            schema_builder.add_text_field("filePath", TEXT | STORED);
            let old_schema = schema_builder.build();
            let dir = MmapDirectory::open(path).unwrap();
            Index::open_or_create(dir, old_schema).unwrap();
        }

        // Must not panic: the incompatible index is recreated empty and usable.
        let mut engine = ReferenceSearchEngine::new(path);
        assert_eq!(engine.count("Ref", false).unwrap(), 0);

        engine
            .add_document(1, "Doc 1", "Ref 1", "R1", 1, false, "/a")
            .unwrap();
        engine.commit().unwrap();
        assert_eq!(engine.count("Ref", false).unwrap(), 1);
        Ok(())
    }

    #[test]
    fn test_delete_document_by_id() -> Result<()> {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mut engine = ReferenceSearchEngine::new(temp_dir.path().to_str().unwrap());

        engine
            .add_document(1, "Doc 1", "Ref 1", "R1", 1, false, "/a")
            .unwrap();
        engine
            .add_document(2, "Doc 2", "Ref 2", "R2", 2, false, "/b")
            .unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.count("Ref", false).unwrap(), 2);

        engine.delete_document_by_id(1).unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.count("Ref", false).unwrap(), 1);
        Ok(())
    }

    #[test]
    fn test_upsert_document() -> Result<()> {
        let temp_dir = tempfile::TempDir::new().unwrap();
        let mut engine = ReferenceSearchEngine::new(temp_dir.path().to_str().unwrap());

        engine
            .add_document(1, "Doc 1", "Old Reference", "OR", 1, false, "/a")
            .unwrap();
        engine.commit().unwrap();

        engine
            .upsert_document(1, "Doc 1", "New Reference", "NR", 1, false, "/a")
            .unwrap();
        engine.commit().unwrap();

        assert_eq!(engine.count("Old", false).unwrap(), 0);
        assert_eq!(engine.count("New", false).unwrap(), 1);
        Ok(())
    }
}
