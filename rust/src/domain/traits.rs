use crate::domain::error::SearchError;
use crate::domain::search_request::SearchRequest;
use crate::domain::search_result::{FusedCandidate, LexicalCandidate, SemanticCandidate};
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

/// Trait defining the contract for lexical search providers (e.g. Tantivy BM25).
#[async_trait]
pub trait LexicalSearcher: Send + Sync {
    async fn search(
        &self,
        request: &SearchRequest,
        cancel_token: &CancellationToken,
    ) -> Result<Vec<LexicalCandidate>, SearchError>;
}

/// Trait defining the contract for semantic vector search providers.
#[async_trait]
pub trait SemanticSearcher: Send + Sync {
    async fn generate_embedding(
        &self,
        text: &str,
        cancel_token: &CancellationToken,
    ) -> Result<Vec<f32>, SearchError>;

    async fn search_vectors(
        &self,
        query_vector: &[f32],
        top_k: usize,
        cancel_token: &CancellationToken,
    ) -> Result<Vec<SemanticCandidate>, SearchError>;
}

/// Trait defining the strategy for fusing BM25 scores and vector similarity scores.
pub trait RankingStrategy: Send + Sync {
    fn fuse_and_rank(
        &self,
        lexical: &[LexicalCandidate],
        semantic: &[SemanticCandidate],
        alpha: f32,
    ) -> Vec<FusedCandidate>;
}
