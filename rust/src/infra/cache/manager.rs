use crate::domain::search_result::{DomainSearchResult, FusedCandidate, SemanticCandidate};
use dashmap::DashMap;
use lru::LruCache;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct CacheMetrics {
    pub hits: u64,
    pub misses: u64,
}

pub struct CacheManager {
    query_cache: DashMap<u64, (DomainSearchResult, Instant)>,
    embedding_cache: Mutex<LruCache<String, Vec<f32>>>,
    vector_cache: Mutex<LruCache<u64, Vec<SemanticCandidate>>>,
    fusion_cache: Mutex<LruCache<u64, Vec<FusedCandidate>>>,
    ttl: Duration,
}

impl CacheManager {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        let cap = NonZeroUsize::new(capacity).unwrap_or(NonZeroUsize::new(1000).unwrap());
        Self {
            query_cache: DashMap::new(),
            embedding_cache: Mutex::new(LruCache::new(cap)),
            vector_cache: Mutex::new(LruCache::new(cap)),
            fusion_cache: Mutex::new(LruCache::new(cap)),
            ttl,
        }
    }

    pub fn get_query(&self, key: u64) -> Option<DomainSearchResult> {
        if let Some(entry) = self.query_cache.get(&key) {
            let (result, created_at) = entry.value();
            if created_at.elapsed() < self.ttl {
                return Some(result.clone());
            }
        }
        None
    }

    pub fn insert_query(&self, key: u64, result: DomainSearchResult) {
        self.query_cache.insert(key, (result, Instant::now()));
    }

    pub fn get_embedding(&self, text: &str) -> Option<Vec<f32>> {
        if let Ok(mut cache) = self.embedding_cache.lock() {
            return cache.get(text).cloned();
        }
        None
    }

    pub fn insert_embedding(&self, text: String, embedding: Vec<f32>) {
        if let Ok(mut cache) = self.embedding_cache.lock() {
            cache.put(text, embedding);
        }
    }

    pub fn clear(&self) {
        self.query_cache.clear();
        if let Ok(mut cache) = self.embedding_cache.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.vector_cache.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.fusion_cache.lock() {
            cache.clear();
        }
    }
}
