use crate::domain::search_request::DomainSearchMode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LexicalCandidate {
    pub doc_id: u64,
    pub book_title: String,
    pub line_number: u32,
    pub score: f32,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticCandidate {
    pub vector_id: u64,
    pub book_title: String,
    pub line_number: u32,
    pub similarity_score: f32,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FusedCandidate {
    pub book_title: String,
    pub line_number: u32,
    pub combined_score: f32,
    pub lexical_score: Option<f32>,
    pub semantic_score: Option<f32>,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DomainSearchResult {
    pub items: Vec<FusedCandidate>,
    pub total_hits: usize,
    pub executed_mode: DomainSearchMode,
    pub fallback_reason: Option<String>,
    pub execution_time_ms: f64,
}
