
use flutter_rust_bridge::frb;
#[frb(sync)] // Synchronous mode for simplicity of the demo
pub fn test_bindings(name: String) -> String {
    format!("Hello, {name}!")
}
use crate::frb_generated::StreamSink;
use anyhow::{Error, Result};
use futures::stream::{Stream, StreamExt};
use log::debug;
use serde_json::{json, Value};
use std::borrow::{Borrow, BorrowMut};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
pub use tantivy::index::Index;
use tantivy::query::{self, BooleanQuery, Occur, QueryParser, TermQuery, TermSetQuery};
pub use tantivy::query::{PhraseQuery, Query};
use tantivy::{
    doc, tokenizer, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher,
    SnippetGenerator,
};
use tantivy::{schema::*, Directory};

#[derive(Clone)]
pub struct SearchResult {
    pub title: String,
    pub reference: String,
    pub text: String,
    pub id: u64,
    pub segment: u64,
    pub is_pdf: bool,
    pub file_path: String,
}

pub struct SearchEngine {
    path: String,
    schema: Schema,
    index: Index,
    index_writer: IndexWriter,
    index_reader: IndexReader,
}

impl SearchEngine {
    pub fn new(path: &str) -> Self {
        debug!("new path={}", path,);
        let schema_builder = Schema::builder();
        let mut schema_builder = Schema::builder();
        let text = schema_builder.add_text_field("text", TEXT | STORED | FAST);
        let reference = schema_builder.add_text_field("reference",  STORED );
        let title = schema_builder.add_text_field(
            "title",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("raw")
                        .set_fieldnorms(false),
                )
                .set_stored(),
        );
        let id = schema_builder.add_u64_field("id", STORED | FAST);
        let segment = schema_builder.add_u64_field("segment", STORED);
        let isPdf = schema_builder.add_bool_field("isPdf", STORED);
        let file_path = schema_builder.add_text_field("filePath", TEXT | STORED);
        let topics = schema_builder.add_facet_field("topics", FacetOptions::default());
        let schema = schema_builder.build();
        let mmap_directory = MmapDirectory::open(path).expect("unable to open mmap directory");
        let index = Index::open_or_create(mmap_directory, schema.clone());
        let index = index.expect("Failed to create index").clone();
        let index_reader = index.reader().expect("Failed to create index reader");
        let index_writer = index
            .writer(50_000_000)
            .expect("Failed to create index writer");

        SearchEngine {
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
        _topics: &str,
        _text: &str,
        _segment: u64,
        _is_pdf: bool,
        _file_path: &str,
    ) -> Result<()> {
        let title = self.schema.get_field("title").unwrap();
        let reference = self.schema.get_field("reference").unwrap();
        let text = self.schema.get_field("text").unwrap();
        let id = self.schema.get_field("id").unwrap();
        let segment = self.schema.get_field("segment").unwrap();
        let is_pdf = self.schema.get_field("isPdf").unwrap();
        let file_path = self.schema.get_field("filePath").unwrap();
        let topics= self.schema.get_field("topics").unwrap();
        let _topics = Facet::from_text(_topics)?;

        self.index_writer.add_document(doc!(
        title => _title,
        reference => _reference,
        text => _text,
        id => _id,
        segment => _segment,
        is_pdf => _is_pdf,
        file_path => _file_path,
        topics => _topics
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
        facets: Vec<String>,
        fuzzy: bool,
    ) -> Result<Box<dyn Query>> {
        let schema = index.schema();
        let text_field = schema.get_field("text").unwrap();
        let topics_field = schema.get_field("topics").unwrap();

        // Create the main text search query
        let mut text_query = QueryParser::for_index(&index, vec![text_field]);
        text_query.set_conjunction_by_default();

        // in case of fuzzy search, set the fuzziness
        if fuzzy {
            text_query.set_field_fuzzy(text_field, false, 1, false);
        }

        // Parse the search term
        let text_query = text_query.parse_query_lenient(search_term).0;

        // Create a TermSetQuery for exact matching of book titles
        let facet_terms: Vec<Term> = facets
            .iter()
            .map(|facet| Term::from_facet(topics_field, &Facet::from_text(facet).unwrap()))
            .collect();
        let facets_query = TermSetQuery::new(facet_terms);

        // Combine the text search and title filter
        let bool_query = BooleanQuery::new(vec![
            (Occur::Must, Box::new(text_query) as Box<dyn Query>),
            (Occur::Must, Box::new(facets_query) as Box<dyn Query>),
        ]);
        Ok(Box::new(bool_query))
    }

    pub fn count(
        &mut self,
        query: &str,
        facets: &Vec<String>,     
        fuzzy: bool,) -> Result<u32> {
        let index = &self.index;
        let schema = index.schema();
        let search_query = Self::create_search_query(index, query, facets.clone(), fuzzy).unwrap();
        let topics_field = schema.get_field("topics").unwrap();

        let facet_terms: Vec<Term> = facets
        .iter()
        .map(|facet| Term::from_facet(topics_field, &Facet::from_text(facet).unwrap()))
        .collect();
    let facets_query = TermSetQuery::new(facet_terms);

       let query = BooleanQuery::new(
            vec![
                (Occur::Must, Box::new(search_query) as Box<dyn Query>),
                (Occur::Must, Box::new(facets_query) as Box<dyn Query>),
            ]
        );

        let searcher = index.reader()?.searcher();
        let count = searcher.search(&query, &Count).unwrap() as u32;
        Ok(count)
        }
    
    
    pub fn search(
        &mut self,
        query: &str,
        facets: Vec<String>,
        limit: u32,
        fuzzy: bool,
        order: ResultsOrder,
    ) -> Result<Vec<SearchResult>> {
        let index = &self.index;
        let schema = &self.schema;
        let query = Self::create_search_query(index, query, facets, fuzzy)?;
        let searcher = index.reader()?.searcher();

        let mut results = Vec::<SearchResult>::new();
        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        let text_field = schema.get_field("text")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;
        let mut snippet_generator = SnippetGenerator::create(&searcher, &*query, text_field)?;
        snippet_generator.set_max_num_chars(800);

        let top_docs: Vec<DocAddress> = match order {
            ResultsOrder::Catalogue => {

            // sort by id (ascending)
            let collector_by_id =
                TopDocs::with_limit(limit as usize).order_by_fast_field::<u64>("id", Order::Asc);
            let top_docs_by_id = searcher.search(&query, &collector_by_id).unwrap();
            top_docs_by_id
                .into_iter()
                .map(|(id, doc_address)| (doc_address))
                .collect()
        }
            ResultsOrder::Relevance =>{
            let coleector_by_score = TopDocs::with_limit(limit as usize);
            let top_docs_by_score = searcher.search(&query, &coleector_by_score).unwrap();
            top_docs_by_score
                .into_iter()
                .map(|(score, doc_address)| (doc_address))
                .collect()

        }
        };

        for doc_address in top_docs {
            match searcher.doc::<TantivyDocument>(doc_address) {
                Ok(retrieved_doc) => {
                    let title = retrieved_doc
                        .get_first(title_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let reference = retrieved_doc
                        .get_first(reference_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    
                    let text = retrieved_doc
                        .get_first(text_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let mut snippet = snippet_generator.snippet(&text);
                    snippet.set_snippet_prefix_postfix("<font color=red>", "</font>");
                    let id = retrieved_doc
                        .get_first(id_field)
                        .and_then(|v| match v {
                            OwnedValue::U64(y) => Some(*y),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let segment = retrieved_doc
                        .get_first(segment_field)
                        .and_then(|v| match v {
                            OwnedValue::U64(y) => Some(*y),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let is_pdf = retrieved_doc
                        .get_first(is_pdf_field)
                        .and_then(|v| match v {
                            OwnedValue::Bool(y) => Some(*y),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let file_path = retrieved_doc
                        .get_first(file_path_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let text = {
                        if fuzzy {
                            text
                        } else {
                            snippet.to_html()
                        }
                    };

                    let result = SearchResult {
                        title,
                        reference,
                        text,
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
    pub fn search_stream(
        &mut self,
        query: &str,
        sink: StreamSink<Vec<SearchResult>>,
        facets: Vec<String>,
        limit: u32,
        fuzzy: bool,
    ) -> Result<()> {
        let index = &self.index;
        let schema = &self.schema;
        let query = Self::create_search_query(index, query, facets, fuzzy).unwrap();
        let searcher = index.reader().unwrap().searcher();
        let top_docs = searcher
            .search(&query, &TopDocs::with_limit(limit as usize))
            .unwrap();
        let mut results = Vec::<SearchResult>::new();
        let title_field = schema.get_field("title").unwrap();
        let reference_field = schema.get_field("reference").unwrap();
        let text_field = schema.get_field("text").unwrap();
        let id_field = schema.get_field("id").unwrap();
        let segment_field = schema.get_field("segment").unwrap();
        let is_pdf_field = schema.get_field("isPdf").unwrap();
        let file_path_field = schema.get_field("filePath").unwrap();

        for (_score, doc_address) in top_docs {
            match searcher.doc::<TantivyDocument>(doc_address) {
                Ok(retrieved_doc) => {
                    let title = retrieved_doc
                        .get_first(title_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let reference = retrieved_doc
                        .get_first(reference_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let text = retrieved_doc
                        .get_first(text_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let id = retrieved_doc
                        .get_first(id_field)
                        .and_then(|v| match v {
                            OwnedValue::U64(y) => Some(*y),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let segment = retrieved_doc
                        .get_first(segment_field)
                        .and_then(|v| match v {
                            OwnedValue::U64(y) => Some(*y),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let is_pdf = retrieved_doc
                        .get_first(is_pdf_field)
                        .and_then(|v| match v {
                            OwnedValue::Bool(y) => Some(*y),
                            _ => None,
                        })
                        .unwrap_or_default();
                    let file_path = retrieved_doc
                        .get_first(file_path_field)
                        .and_then(|v| match v {
                            OwnedValue::Str(s) => Some(s.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();

                    let result = SearchResult {
                        title,
                        reference,
                        text,
                        id,
                        segment,
                        is_pdf,
                        file_path,
                    };
                    results.push(result);
                    sink.add(results.clone());
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }

    pub fn clear(&self)->Result<()>{
        self.index_writer.delete_all_documents()?;
       Ok(())
    }
}

pub enum ResultsOrder{
    Catalogue, Relevance
}

#[cfg(test)]
#[test]

fn test_facet_search()->Result<()>{

    // Create a temporary directory for testing

    use std::str::FromStr;
    let temp_dir = tempfile::Builder::new().prefix("search_engine_test").tempdir().unwrap();
    let temp_path = temp_dir.path().to_str().unwrap();

    // Create a new SearchEngine instance
    let mut search_engine = SearchEngine::new(temp_path);

    // Add some documents with different topics
    search_engine.add_document(1, "Document 1", "Ref 1", "/topic1/subtopic1", "This is the text of document 1", 1, false, "/path/to/doc1").unwrap();
    search_engine.add_document(2, "Document 2", "Ref 2", "/topic1/subtopic2", "This is the text of document 2", 2, false, "/path/to/doc2").unwrap();
    search_engine.add_document(3, "Document 3", "Ref 3", "/topic2/subtopic1", "This is the text of document 3", 3, false, "/path/to/doc3").unwrap();

    // Commit the changes
    search_engine.commit().unwrap();
    // Test facet search for topic1
    let count = search_engine.count("text",  &vec!["/topic1".to_string()], false).unwrap();
    assert_eq!(count, 2);

    // Test facet search for topic2
    let count = search_engine.count("text",  &vec!["/topic2".to_string()], false).unwrap();
    assert_eq!(count, 1);

    // Test facet search for a subtopic
    let count = search_engine.count("text",   &vec!["/topic1".to_string()], false).unwrap();
    assert_eq!(count, 1);

     // Test facet search for a subtopic
     let count = search_engine.count("text",   &vec!["/topic1".to_string()], false).unwrap();
     assert_eq!(count, 2);

    // Test facet search for a non-existent topic
    let count = search_engine.count("text",  &vec!["/nonexistent".to_string()], false).unwrap();
    assert_eq!(count, 0);
    
    Ok(())
}
