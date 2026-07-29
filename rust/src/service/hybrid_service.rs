use crate::domain::error::SearchError;
use crate::domain::search_request::{DomainSearchMode, SearchRequest};
use crate::domain::search_result::{DomainSearchResult, FusedCandidate};
use crate::domain::traits::{LexicalSearcher, RankingStrategy, SemanticSearcher};
use crate::infra::cache::CacheManager;
use crate::service::metrics_service::MetricsService;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

pub struct DefaultRankingStrategy;

impl RankingStrategy for DefaultRankingStrategy {
    fn fuse_and_rank(
        &self,
        lexical: &[crate::domain::search_result::LexicalCandidate],
        semantic: &[crate::domain::search_result::SemanticCandidate],
        alpha: f32,
    ) -> Vec<FusedCandidate> {
        let mut results = Vec::new();
        
        for cand in lexical {
            results.push(FusedCandidate {
                book_title: cand.book_title.clone(),
                line_number: cand.line_number,
                combined_score: cand.score * alpha,
                lexical_score: Some(cand.score),
                semantic_score: None,
                snippet: cand.snippet.clone(),
            });
        }

        for sem in semantic {
            if let Some(existing) = results.iter_mut().find(|r| r.book_title == sem.book_title && r.line_number == sem.line_number) {
                existing.semantic_score = Some(sem.similarity_score);
                existing.combined_score += sem.similarity_score * (1.0 - alpha);
            } else {
                results.push(FusedCandidate {
                    book_title: sem.book_title.clone(),
                    line_number: sem.line_number,
                    combined_score: sem.similarity_score * (1.0 - alpha),
                    lexical_score: None,
                    semantic_score: Some(sem.similarity_score),
                    snippet: sem.snippet.clone(),
                });
            }
        }

        results.sort_by(|a, b| b.combined_score.partial_cmp(&a.combined_score).unwrap_or(std::cmp::Ordering::Equal));
        results
    }
}

pub struct HybridService<L, S, R>
where
    L: LexicalSearcher,
    S: SemanticSearcher,
    R: RankingStrategy,
{
    lexical_searcher: Arc<L>,
    semantic_searcher: Option<Arc<S>>,
    ranking_strategy: R,
    cache_manager: Arc<CacheManager>,
    metrics_service: Arc<MetricsService>,
}

impl<L, S, R> HybridService<L, S, R>
where
    L: LexicalSearcher,
    S: SemanticSearcher,
    R: RankingStrategy,
{
    pub fn new(
        lexical_searcher: Arc<L>,
        semantic_searcher: Option<Arc<S>>,
        ranking_strategy: R,
        cache_capacity: usize,
    ) -> Self {
        Self {
            lexical_searcher,
            semantic_searcher,
            ranking_strategy,
            cache_manager: Arc::new(CacheManager::new(cache_capacity, Duration::from_secs(300))),
            metrics_service: Arc::new(MetricsService::default()),
        }
    }

    fn compute_cache_key(&self, request: &SearchRequest) -> u64 {
        let mut hasher = DefaultHasher::new();
        request.query.hash(&mut hasher);
        request.facets.hash(&mut hasher);
        (request.search_mode as u8).hash(&mut hasher);
        request.limit.hash(&mut hasher);
        request.offset.hash(&mut hasher);
        hasher.finish()
    }

    #[instrument(skip(self, request, cancel_token), fields(query = %request.query))]
    pub async fn search(
        &self,
        request: SearchRequest,
        cancel_token: CancellationToken,
    ) -> Result<DomainSearchResult, SearchError> {
        let start = Instant::now();
        let cache_key = self.compute_cache_key(&request);

        if let Some(cached) = self.cache_manager.get_query(cache_key) {
            self.metrics_service.record_search(start.elapsed().as_millis() as u64, true);
            info!("Query cache hit for query: {}", request.query);
            return Ok(cached);
        }

        if cancel_token.is_cancelled() {
            return Err(SearchError::Cancelled);
        }

        let alpha = request.alpha.unwrap_or(0.5);

        let result = match request.search_mode {
            DomainSearchMode::Exact | DomainSearchMode::Advanced | DomainSearchMode::Fuzzy => {
                let lexical_candidates = self.lexical_searcher.search(&request, &cancel_token).await?;
                let items: Vec<FusedCandidate> = lexical_candidates
                    .into_iter()
                    .map(|c| FusedCandidate {
                        book_title: c.book_title,
                        line_number: c.line_number,
                        combined_score: c.score,
                        lexical_score: Some(c.score),
                        semantic_score: None,
                        snippet: c.snippet,
                    })
                    .collect();

                let total_hits = items.len();
                DomainSearchResult {
                    items,
                    total_hits,
                    executed_mode: request.search_mode,
                    fallback_reason: None,
                    execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                }
            }

            DomainSearchMode::Hybrid => {
                let lexical_future = self.lexical_searcher.search(&request, &cancel_token);

                if let Some(ref sem_searcher) = self.semantic_searcher {
                    let sem_future = async {
                        if let Some(cached_vec) = self.cache_manager.get_embedding(&request.query) {
                            sem_searcher.search_vectors(&cached_vec, request.limit, &cancel_token).await
                        } else {
                            let vec = sem_searcher.generate_embedding(&request.query, &cancel_token).await?;
                            self.cache_manager.insert_embedding(request.query.clone(), vec.clone());
                            sem_searcher.search_vectors(&vec, request.limit, &cancel_token).await
                        }
                    };

                    let (lexical_res, semantic_res) = tokio::join!(lexical_future, sem_future);
                    let lexical_candidates = lexical_res?;
                    
                    match semantic_res {
                        Ok(semantic_candidates) => {
                            let fused = self.ranking_strategy.fuse_and_rank(&lexical_candidates, &semantic_candidates, alpha);
                            let total_hits = fused.len();
                            DomainSearchResult {
                                items: fused,
                                total_hits,
                                executed_mode: DomainSearchMode::Hybrid,
                                fallback_reason: None,
                                execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                            }
                        }
                        Err(err) => {
                            info!("Semantic path failed, falling back gracefully to LexicalOnly: {}", err);
                            let items: Vec<FusedCandidate> = lexical_candidates
                                .into_iter()
                                .map(|c| FusedCandidate {
                                    book_title: c.book_title,
                                    line_number: c.line_number,
                                    combined_score: c.score,
                                    lexical_score: Some(c.score),
                                    semantic_score: None,
                                    snippet: c.snippet,
                                })
                                .collect();
                            let total_hits = items.len();
                            DomainSearchResult {
                                items,
                                total_hits,
                                executed_mode: DomainSearchMode::Exact,
                                fallback_reason: Some(format!("Semantic search unavailable: {}", err)),
                                execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                            }
                        }
                    }
                } else {
                    let lexical_candidates = lexical_future.await?;
                    let items: Vec<FusedCandidate> = lexical_candidates
                        .into_iter()
                        .map(|c| FusedCandidate {
                            book_title: c.book_title,
                            line_number: c.line_number,
                            combined_score: c.score,
                            lexical_score: Some(c.score),
                            semantic_score: None,
                            snippet: c.snippet,
                        })
                        .collect();
                    let total_hits = items.len();
                    DomainSearchResult {
                        items,
                        total_hits,
                        executed_mode: DomainSearchMode::Exact,
                        fallback_reason: Some("Semantic search sidecar not configured".to_string()),
                        execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                    }
                }
            }

            DomainSearchMode::SemanticOnly => {
                if let Some(ref sem_searcher) = self.semantic_searcher {
                    let vec = sem_searcher.generate_embedding(&request.query, &cancel_token).await?;
                    let semantic_candidates = sem_searcher.search_vectors(&vec, request.limit, &cancel_token).await?;
                    let items: Vec<FusedCandidate> = semantic_candidates
                        .into_iter()
                        .map(|s| FusedCandidate {
                            book_title: s.book_title,
                            line_number: s.line_number,
                            combined_score: s.similarity_score,
                            lexical_score: None,
                            semantic_score: Some(s.similarity_score),
                            snippet: s.snippet,
                        })
                        .collect();
                    let total_hits = items.len();
                    DomainSearchResult {
                        items,
                        total_hits,
                        executed_mode: DomainSearchMode::SemanticOnly,
                        fallback_reason: None,
                        execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                    }
                } else {
                    DomainSearchResult {
                        items: Vec::new(),
                        total_hits: 0,
                        executed_mode: DomainSearchMode::SemanticOnly,
                        fallback_reason: Some("Semantic search engine is disabled or missing model".to_string()),
                        execution_time_ms: start.elapsed().as_secs_f64() * 1000.0,
                    }
                }
            }
        };

        self.cache_manager.insert_query(cache_key, result.clone());
        self.metrics_service.record_search(start.elapsed().as_millis() as u64, false);
        Ok(result)
    }
}
