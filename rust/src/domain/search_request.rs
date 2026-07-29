use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DomainSearchMode {
    Exact,
    Advanced,
    Fuzzy,
    Hybrid,
    SemanticOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DomainGroupingMode {
    SameSection,
    IdenticalText,
}

/// Unified domain request representation across all search modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub facets: Vec<String>,
    pub limit: usize,
    pub offset: usize,
    pub search_mode: DomainSearchMode,
    pub alpha: Option<f32>,
    pub grouping: Option<DomainGroupingMode>,
    pub distance: u32,
    pub match_nikud: bool,
    pub match_taamim: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            facets: Vec::new(),
            limit: 20,
            offset: 0,
            search_mode: DomainSearchMode::Exact,
            alpha: Some(0.5),
            grouping: None,
            distance: 0,
            match_nikud: false,
            match_taamim: false,
        }
    }
}
