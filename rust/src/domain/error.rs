use thiserror::Error;

/// Central error hierarchy for search and indexing operations.
#[derive(Error, Debug, Clone)]
pub enum SearchError {
    #[error("Lexical search error: {0}")]
    Lexical(String),

    #[error("Semantic search error: {0}")]
    Semantic(String),

    #[error("Embedding generation failed: {0}")]
    Embedding(String),

    #[error("Vector store error: {0}")]
    VectorStore(String),

    #[error("Search operation was cancelled")]
    Cancelled,

    #[error("Search operation timed out after {0} ms")]
    Timeout(u64),

    #[error("Invalid search request: {0}")]
    InvalidRequest(String),

    #[error("Native runtime panic intercepted: {0}")]
    NativePanic(String),

    #[error("IO or storage error: {0}")]
    Storage(String),

    #[error("Configuration error: {0}")]
    Configuration(String),
}
