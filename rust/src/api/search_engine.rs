
use flutter_rust_bridge::frb;
use anyhow::Result;
use log::debug;
use tantivy::schema::Value;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tantivy::collector::{Count, FacetCollector, FacetCounts, TopDocs};
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
use crate::api::hebrew_tokenizer::HebrewTokenizer;

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
        schema_builder.add_text_field(
            "text",
            TextOptions::default()
                .set_indexing_options(
                    TextFieldIndexing::default()
                        .set_tokenizer("hebrew")
                        .set_index_option(IndexRecordOption::WithFreqsAndPositions),
                )
                .set_stored()
                .set_fast(None),
        );
        schema_builder.add_text_field("reference", STORED);
        schema_builder.add_text_field(
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
        index.tokenizers().register("hebrew", HebrewTokenizer);
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
                let collector_by_score = TopDocs::with_limit(limit as usize);
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

    /// Returns count per book title for all matching documents.
    /// Much more efficient than fetching full SearchResult structs via FFI since only
    /// a small HashMap<title, count> is transferred instead of 50k full documents.
    pub fn count_by_title(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<HashMap<String, u32>> {
        let index = &self.index;
        let title_field = self.schema.get_field("title")?;
        let query = Self::create_query(index, regex_terms, facets, slop, max_expansions)?;
        let searcher = index.reader()?.searcher();

        let collector = TopDocs::with_limit(500_000)
            .order_by_fast_field::<u64>("id", Order::Asc);
        let top_docs = searcher.search(&query, &collector)?;

        let mut counts: HashMap<String, u32> = HashMap::new();
        for (_, doc_address) in top_docs {
            if let Ok(doc) = searcher.doc::<TantivyDocument>(doc_address) {
                if let Some(title) = doc.get_first(title_field).and_then(|v| v.as_str()) {
                    *counts.entry(title.to_string()).or_insert(0) += 1;
                }
            }
        }

        Ok(counts)
    }

    /// Returns facet counts for all levels of the topics hierarchy.
    /// Uses FacetCollector which reads the column-oriented facet index —
    /// no stored field reads, dramatically faster than count_by_title.
    /// Result includes every facet path plus "/$title" entries for tree fallback lookup.
    pub fn count_by_facet(
        &mut self,
        regex_terms: Vec<String>,
        facets: Vec<String>,
        slop: u32,
        max_expansions: u32,
    ) -> Result<HashMap<String, u32>> {
        let index = &self.index;
        let query = Self::create_query(index, regex_terms, facets, slop, max_expansions)?;
        let searcher = index.reader()?.searcher();

        let mut facet_collector = FacetCollector::for_field("topics");
        facet_collector.add_facet("/");

        let facet_counts = searcher.search(&query, &facet_collector)?;

        let mut result: HashMap<String, u32> = HashMap::new();
        collect_facet_levels(&facet_counts, "/", &mut result);

        Ok(result)
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

/// Recursively collects facet counts at all levels of the hierarchy.
/// For each leaf facet (e.g. /תנ"ך/תורה/ספר בראשית) also adds a /$title entry
/// so the Dart tree can find book counts via both fullFacet and titleOnlyFacet lookups.
fn collect_facet_levels(
    facet_counts: &FacetCounts,
    parent_path: &str,
    result: &mut HashMap<String, u32>,
) {
    let children: Vec<(String, u32)> = facet_counts
        .get(parent_path)
        .map(|(facet, count): (&tantivy::schema::Facet, u64)| {
            (facet.to_path_string(), count as u32)
        })
        .collect();

    if parent_path == "/" {
        let total: u32 = children.iter().map(|(_, c)| *c).sum();
        if total > 0 {
            result.insert("/".to_string(), total);
        }
    }

    for (path, count) in &children {
        result.insert(path.clone(), *count);
        collect_facet_levels(facet_counts, path, result);
    }

    // Leaf facet: also add "/$title" for tree fallback lookup
    if children.is_empty() && parent_path != "/" {
        if let Some(title) = parent_path.rsplit('/').next() {
            if !title.is_empty() {
                let count = result.get(parent_path).copied().unwrap_or(0);
                result.entry(format!("/{}", title)).or_insert(count);
            }
        }
    }
}

#[cfg(test)]
mod tests {
}