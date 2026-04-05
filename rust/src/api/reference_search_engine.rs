use flutter_rust_bridge::frb;
use crate::api::search_engine::ResultsOrder;
use anyhow::Result;
use futures::stream::{Stream, StreamExt};
use log::debug;
use serde_json::{json, Value};
use std::borrow::{Borrow, BorrowMut};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::index::Index;
use tantivy::query::{self, BooleanQuery, Occur, QueryParser, TermQuery, TermSetQuery};
use tantivy::query::{PhraseQuery, Query};
use tantivy::schema::Value as TantivyValue;
use tantivy::{
    doc, tokenizer, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher,
};
use tantivy::{schema::*, Directory};

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

pub struct ReferenceSearchEngine {
    path: String,
    schema: Schema,
    index: Index,
    index_writer: IndexWriter,
    index_reader: IndexReader,
}

impl ReferenceSearchEngine {
    #[frb(sync)]
    pub fn new(path: &str) -> Self {
        debug!("new path={}", path,);
        let mut schema_builder = Schema::builder();
        let reference = schema_builder.add_text_field("reference", TEXT | STORED);
        let short_ref = schema_builder.add_text_field("shortRef", TEXT );
        let title = schema_builder.add_text_field("title", TEXT | STORED);
        let id = schema_builder.add_u64_field("id", STORED | FAST);
        let segment = schema_builder.add_u64_field("segment", STORED);
        let is_pdf = schema_builder.add_bool_field("isPdf", STORED);
        let file_path = schema_builder.add_text_field("filePath", TEXT | STORED);
        let schema = schema_builder.build();
        let mmap_directory = MmapDirectory::open(path).expect("unable to open mmap directory");
        let index = Index::open_or_create(mmap_directory, schema.clone());
        let index = index.expect("Failed to create index").clone();
        let index_reader = index.reader().expect("Failed to create index reader");
        let index_writer = index
            .writer(50_000_000)
            .expect("Failed to create index writer");

        ReferenceSearchEngine {
            path: path.to_string(),
            index: index,
            schema: schema,
            index_writer: index_writer,
            index_reader: index_reader,
        }
    }

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
        let title = self.schema.get_field("title").unwrap();
        let reference = self.schema.get_field("reference").unwrap();
        let short_ref = self.schema.get_field("shortRef").unwrap();
        let id = self.schema.get_field("id").unwrap();
        let segment = self.schema.get_field("segment").unwrap();
        let is_pdf = self.schema.get_field("isPdf").unwrap();
        let file_path = self.schema.get_field("filePath").unwrap();

        self.index_writer.add_document(doc!(
            title => _title,
            reference => _reference,
            short_ref => _short_ref,
            id => _id,
            segment => _segment,
            is_pdf => _is_pdf,
            file_path => _file_path
        ))?;

        Ok(())
    }

    pub fn commit(&mut self) -> Result<()> {
        self.index_writer.commit()?;
        Ok(())
    }

    pub fn create_search_query(
        index: &Index,
        search_term: &str,
        fuzzy: bool,
    ) -> Result<Box<dyn Query>> {
        let schema = index.schema();
        let reference_field = schema.get_field("reference").unwrap();
        let short_ref_field = schema.get_field("shortRef").unwrap();

        // Create the main reference search query (search both reference and shortRef)
        let mut reference_query = QueryParser::for_index(&index, vec![reference_field, short_ref_field]);
        reference_query.set_conjunction_by_default();

        // In case of fuzzy search, set the fuzziness
        if fuzzy {
            reference_query.set_field_fuzzy(reference_field, false, 1, false);
            reference_query.set_field_fuzzy(short_ref_field, false, 1, false);
        }

        // Parse the search term
        let reference_query = reference_query.parse_query_lenient(search_term).0;

        Ok(Box::new(reference_query))
    }

    pub fn count(
        &mut self,
        query: &str,
        fuzzy: bool,
    ) -> Result<u32> {
        let index = &self.index;
        let search_query = Self::create_search_query(index, query, fuzzy).unwrap();

        let searcher = index.reader()?.searcher();
        let count = searcher.search(&search_query, &Count).unwrap() as u32;
        Ok(count)
    }
    
    pub fn search(
        &mut self,
        query: &str,
        limit: u32,
        fuzzy: bool,
        order: ResultsOrder,
    ) -> Result<Vec<ReferenceSearchResult>> {
        let index = &self.index;
        let schema = &self.schema;
        let query = Self::create_search_query(index, query, fuzzy)?;
        let searcher = index.reader()?.searcher();

        let mut results = Vec::<ReferenceSearchResult>::new();
        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        let short_ref_field = schema.get_field("shortRef")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;

        let top_docs: Vec<DocAddress> = match order {
            ResultsOrder::Catalogue => {
                // sort by id (ascending)
                let collector_by_id =
                    TopDocs::with_limit(limit as usize).order_by_fast_field::<u64>("id", Order::Asc);
                let top_docs_by_id = searcher.search(&query, &collector_by_id).unwrap();
                top_docs_by_id
                    .into_iter()
                    .map(|(_, doc_address)| doc_address)
                    .collect()
            }
            ResultsOrder::Relevance => {
                let collector_by_score = TopDocs::with_limit(limit as usize).order_by_score();
                let top_docs_by_score = searcher.search(&query, &collector_by_score).unwrap();
                top_docs_by_score
                    .into_iter()
                    .map(|(_, doc_address)| doc_address)
                    .collect()
            }
        };

        for doc_address in top_docs {
            match searcher.doc::<TantivyDocument>(doc_address) {
                Ok(retrieved_doc) => {
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

                    let result = ReferenceSearchResult {
                        title,
                        reference,
                        short_ref,
                        id,
                        segment,
                        is_pdf,
                        file_path,
                    };
                    results.push(result);
                }
                Err(_) => continue,
            }
        }
        Ok(results)
    }

    pub fn clear(&self) -> Result<()> {
        self.index_writer.delete_all_documents()?;
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reference_search() -> Result<()> {
        // Create a temporary directory for testing
        let temp_dir = tempfile::Builder::new().prefix("reference_search_engine_test").tempdir().unwrap();
        let temp_path = temp_dir.path().to_str().unwrap();

        // Create a new ReferenceSearchEngine instance
        let mut search_engine = ReferenceSearchEngine::new(temp_path);

        // Add some documents with different references
        search_engine.add_document(1, "Document 1", "Reference 1", "R 1", 1, false, "/path/to/doc1").unwrap();
        search_engine.add_document(2, "Document 2", "Reference 2","R 2" ,2, false, "/path/to/doc2").unwrap();
        search_engine.add_document(3, "Document 3", "Another Reference", " A R",3, false, "/path/to/doc3").unwrap();

        // Commit the changes
        search_engine.commit().unwrap();

        // Test basic search
        let results = search_engine.search("Reference", 10, false, ResultsOrder::Catalogue).unwrap();
        assert_eq!(results.len(), 3);

        // Test more specific search
        let results = search_engine.search("Reference 1", 10, false, ResultsOrder::Catalogue).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].reference, "Reference 1");

        // Test fuzzy search
        let results = search_engine.search("Referenc", 10, true, ResultsOrder::Catalogue).unwrap();
        assert!(results.len() > 0);

        // Test count
        let count = search_engine.count("Reference", false).unwrap();
        assert_eq!(count, 3);

        // Test count with more specific query
        let count = search_engine.count("Reference 1", false).unwrap();
        assert_eq!(count, 1);

        Ok(())
    }
}
