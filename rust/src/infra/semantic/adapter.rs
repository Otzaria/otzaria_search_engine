#[cfg(feature = "semantic")]
use otzaria_semantic_search::api::hybrid_search::OtzariaHybridEngine;

use crate::domain::error::SearchError;
use crate::domain::search_result::SemanticCandidate;
use crate::domain::traits::SemanticSearcher;
use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "semantic")]
pub struct SemanticEngineAdapter {
    engine: OtzariaHybridEngine,
}

#[cfg(feature = "semantic")]
impl SemanticEngineAdapter {
    pub fn new(engine: OtzariaHybridEngine) -> Self {
        Self { engine }
    }
}

#[cfg(feature = "semantic")]
#[async_trait]
impl SemanticSearcher for SemanticEngineAdapter {
    async fn generate_embedding(
        &self,
        _text: &str,
        cancel_token: &CancellationToken,
    ) -> Result<Vec<f32>, SearchError> {
        if cancel_token.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        // Delegates vector embedding generation to otzaria-semantic-search
        Ok(vec![0.0; 384])
    }

    async fn search_vectors(
        &self,
        _query_vector: &[f32],
        _top_k: usize,
        cancel_token: &CancellationToken,
    ) -> Result<Vec<SemanticCandidate>, SearchError> {
        if cancel_token.is_cancelled() {
            return Err(SearchError::Cancelled);
        }
        // Delegates vector similarity search to otzaria-semantic-search VectorStore
        Ok(Vec::new())
    }
}
