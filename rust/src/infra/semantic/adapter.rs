#[cfg(feature = "semantic")]
use otzaria_semantic_search::api::hybrid_search::{
    OtzariaHybridEngine, SearchRequest as SemanticSearchRequest,
};
#[cfg(feature = "semantic")]
use otzaria_semantic_search::semantic::types::{
    HybridResultItem as SemanticResultItem, LexicalCandidate as SemanticLexicalCandidate,
};

use crate::domain::error::SearchError;
use crate::domain::search_result::{FusedCandidate, LexicalCandidate, SemanticCandidate};
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

    pub fn to_semantic_lexical_candidate(c: &LexicalCandidate) -> SemanticLexicalCandidate {
        SemanticLexicalCandidate {
            source_book_key: c.book_title.clone(),
            line_number: c.line_number,
            bm25_score: c.score,
            line_text: c.snippet.clone(),
        }
    }

    pub fn from_semantic_result_item(item: &SemanticResultItem) -> FusedCandidate {
        FusedCandidate {
            book_title: item.source_book_key.clone(),
            line_number: item.line_number,
            combined_score: item.fused_score,
            lexical_score: item.bm25_score,
            semantic_score: item.vector_score,
            snippet: item.snippet.clone(),
        }
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
        Ok(Vec::new())
    }
}
