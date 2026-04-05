
use flutter_rust_bridge::frb;
use anyhow::Result;
use log::debug;
use tantivy::schema::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tantivy::collector::{Collector, Count, SegmentCollector, TopDocs};
use tantivy::{DocId, SegmentOrdinal, SegmentReader};
use tantivy::directory::MmapDirectory;
pub use tantivy::index::Index;
use tantivy::query::{self, BooleanQuery, Occur, QueryParser, RegexQuery, TermQuery, TermSetQuery};
pub use tantivy::query::{PhraseQuery, RegexPhraseQuery, Query};
use tantivy::tokenizer::{SimpleTokenizer, TextAnalyzer, RemoveLongFilter, LowerCaser};
use tantivy::{
    doc, snippet, tokenizer, DocAddress, IndexReader, IndexWriter, Order, ReloadPolicy, Score, Searcher
};
use tantivy::snippet::SnippetGenerator;
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
    schema: Schema,
    index: Index,
    index_writer: IndexWriter,
}

impl SearchEngine {
    #[frb(sync)]
    pub fn new(path: &str) -> Self {
        debug!("new path={}", path,);
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
        let file_path = schema_builder.add_text_field("filePath", STRING | FAST | STORED);
        let topics = schema_builder.add_facet_field("topics", FacetOptions::default());
        let schema = schema_builder.build();
        let mmap_directory = MmapDirectory::open(path).expect("unable to open mmap directory");
        let index = Index::open_or_create(mmap_directory, schema.clone());
        let index = index.expect("Failed to create index").clone();
        
        // Register the custom Hebrew tokenizer
               let index_writer = index
            .writer(50_000_000)
            .expect("Failed to create index writer");

        SearchEngine {
            index: index,
            schema: schema,
            index_writer: index_writer,
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
   

    pub fn create_query(
        index: &Index,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<Box<dyn Query>> {
        let schema = index.schema();
        let text_field = 
            schema.get_field("text").unwrap();
       
        let topics_field = schema.get_field("topics").unwrap();

        let main_query: Box<dyn Query> = if regex_terms.len() == 1 {
            // Single term: use RegexQuery
            Box::new(RegexQuery::from_pattern(&regex_terms[0], text_field)?)
        } else {
            // Multiple terms: use RegexPhraseQuery
            let mut phrase_query = RegexPhraseQuery::new(text_field, regex_terms);
            phrase_query.set_slop(slop);
            phrase_query.set_max_expansions(max_expansions);
            Box::new(phrase_query)
        };

        // Create facet filtering query
        let facet_terms: Vec<Term> = facets
            .iter()
            .map(|facet| Term::from_facet(topics_field, &Facet::from_text(facet).unwrap()))
            .collect();
        let facets_query = TermSetQuery::new(facet_terms);

        // Combine the regex query and facet filter
        let bool_query = BooleanQuery::new(vec![
            (Occur::Must, main_query),
            (Occur::Must, Box::new(facets_query) as Box<dyn Query>),
        ]);

        Ok(Box::new(bool_query))
    }

    pub fn search(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        limit: u32,
        slop: u32,
        max_expansions: u32,
        order: ResultsOrder,
    ) -> Result<Vec<SearchResult>> {
        let index = &self.index;
        let schema = &self.schema;
        let query = Self::create_query(index, regex_terms, facets, slop, max_expansions)?;
        let searcher = index.reader()?.searcher();

        let mut results = Vec::<SearchResult>::new();
        let title_field = schema.get_field("title")?;
        let reference_field = schema.get_field("reference")?;
        let text_field = schema.get_field("text")?;
        let id_field = schema.get_field("id")?;
        let segment_field = schema.get_field("segment")?;
        let is_pdf_field = schema.get_field("isPdf")?;
        let file_path_field = schema.get_field("filePath")?;

        // Use the appropriate text field for snippet generation
        let snippet_text_field =text_field ;
        let mut snippet_generator = SnippetGenerator::create(&searcher, &*query, snippet_text_field)?;
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
            ResultsOrder::Relevance => {
                let collector_by_score = TopDocs::with_limit(limit as usize).order_by_score();
                let top_docs_by_score = searcher.search(&query, &collector_by_score).unwrap();
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
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let reference = retrieved_doc
                        .get_first(reference_field)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    
                    let text = retrieved_doc
                        .get_first(text_field)
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    
               
                    
                    let mut snippet = snippet_generator.snippet(&text);
                    snippet.set_snippet_prefix_postfix("<font color=red>", "</font>");
                    
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
                    
                    let snippet_text = snippet.to_html();
                    let result_text = if snippet_text.is_empty() {
                        text
                    } else {
                        snippet_text
                    };

                    let result = SearchResult {
                        title,
                        reference,
                        text: result_text,
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

    pub fn count(
        &mut self,
        regex_terms: Vec<String>,
        facets: &Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<u32> {
        let index = &self.index;
        let query = Self::create_query(index, regex_terms, facets.clone(), slop, max_expansions)?;
        let searcher = index.reader()?.searcher();
        let count = searcher.search(&query, &Count).unwrap() as u32;
        Ok(count)
    }

    pub fn count_by_book(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<HashMap<String, u32>> {
        let index = &self.index;
        let query = Self::create_query(index, regex_terms, facets, slop, max_expansions)?;
        let searcher = index.reader()?.searcher();
        let counts = searcher.search(&query, &BookCountCollector)?;
        Ok(counts)
    }

    pub fn clear(&self)->Result<()>{
        self.index_writer.delete_all_documents()?;
       Ok(())
    }

    pub fn remove_documents_by_title(&mut self, title: &str) -> Result<()> {
        let title_field = self.schema.get_field("title")?;
        let title_term = Term::from_field_text(title_field, title);
        self.index_writer.delete_term(title_term);
        Ok(())
    }

   
}

pub enum ResultsOrder{
    Catalogue, Relevance
}

/// Collector that counts matching documents grouped by the `filePath` fast field.
/// Operates entirely on the column-oriented fast field — no stored-document reads.
/// Per-segment counts are collected using term ordinals (integers), decoded to strings
/// only once in `harvest()`, then merged across segments in `merge_fruits()`.
struct BookCountCollector;

struct BookCountSegmentCollector {
    str_col: Option<tantivy::columnar::StrColumn>,
    // term_ord -> count within this segment
    counts: HashMap<u64, u32>,
}

impl Collector for BookCountCollector {
    type Fruit = HashMap<String, u32>;
    type Child = BookCountSegmentCollector;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<BookCountSegmentCollector> {
        let str_col = reader.fast_fields().str("filePath")?;
        Ok(BookCountSegmentCollector { str_col, counts: HashMap::new() })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, per_segment: Vec<tantivy::Result<HashMap<String, u32>>>) -> tantivy::Result<HashMap<String, u32>> {
        let mut merged: HashMap<String, u32> = HashMap::new();
        for seg_result in per_segment {
            for (path, count) in seg_result? {
                *merged.entry(path).or_insert(0) += count;
            }
        }
        Ok(merged)
    }
}

impl SegmentCollector for BookCountSegmentCollector {
    type Fruit = tantivy::Result<HashMap<String, u32>>;

    fn collect(&mut self, doc_id: DocId, _score: Score) {
        if let Some(col) = &self.str_col {
            // STRING field: each doc has exactly one term ordinal
            if let Some(term_ord) = col.term_ords(doc_id).next() {
                *self.counts.entry(term_ord).or_insert(0) += 1;
            }
        }
    }

    fn harvest(self) -> tantivy::Result<HashMap<String, u32>> {
        let Some(col) = self.str_col else { return Ok(HashMap::new()) };
        let mut result = HashMap::with_capacity(self.counts.len());
        let mut buf = String::new();
        for (term_ord, count) in self.counts {
            buf.clear();
            if col.ord_to_str(term_ord, &mut buf)? {
                result.insert(buf.clone(), count);
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_engine() -> (SearchEngine, TempDir) {
        let dir = TempDir::new().unwrap();
        let engine = SearchEngine::new(dir.path().to_str().unwrap());
        (engine, dir)
    }

    fn add(engine: &mut SearchEngine, id: u64, text: &str, file_path: &str) {
        engine
            .add_document(id, "title", "ref", "/root", text, 0, false, file_path)
            .unwrap();
    }

    #[test]
    fn test_count_by_book_basic() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
        assert_eq!(counts.len(), 2);
    }

    #[test]
    fn test_count_by_book_empty_result() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book(vec!["ביי".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        assert!(counts.is_empty());
    }

    #[test]
    fn test_count_by_book_no_cross_contamination() {
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        add(&mut engine, 2, "שלום ביי", "/books/b.txt");
        engine.commit().unwrap();

        let counts = engine
            .count_by_book(vec!["עולם".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        assert_eq!(counts.get("/books/a.txt").copied(), Some(1));
        assert_eq!(counts.get("/books/b.txt"), None);
    }

    #[test]
    fn test_count_by_book_multi_segment() {
        // Each commit() creates a new segment; same filePath must merge correctly
        let (mut engine, _dir) = make_engine();
        add(&mut engine, 1, "שלום עולם", "/books/a.txt");
        engine.commit().unwrap();  // segment 1

        add(&mut engine, 2, "שלום רב", "/books/a.txt");
        add(&mut engine, 3, "שלום חבר", "/books/b.txt");
        engine.commit().unwrap();  // segment 2

        let counts = engine
            .count_by_book(vec!["שלום".to_string()], vec!["/root".to_string()], 0, 100)
            .unwrap();

        // a.txt has hits in both segments; must be summed correctly
        assert_eq!(counts.get("/books/a.txt").copied(), Some(2));
        assert_eq!(counts.get("/books/b.txt").copied(), Some(1));
        assert_eq!(counts.len(), 2);
    }
}