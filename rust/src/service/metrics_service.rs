use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SearchMetricsReport {
    pub total_searches: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub total_search_time_ms: u64,
}

#[derive(Default)]
pub struct MetricsService {
    total_searches: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,
    total_search_time_ms: AtomicU64,
}

impl MetricsService {
    pub fn record_search(&self, duration_ms: u64, cache_hit: bool) {
        self.total_searches.fetch_add(1, Ordering::Relaxed);
        self.total_search_time_ms.fetch_add(duration_ms, Ordering::Relaxed);
        if cache_hit {
            self.cache_hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.cache_misses.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn get_report(&self) -> SearchMetricsReport {
        SearchMetricsReport {
            total_searches: self.total_searches.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            cache_misses: self.cache_misses.load(Ordering::Relaxed),
            total_search_time_ms: self.total_search_time_ms.load(Ordering::Relaxed),
        }
    }
}
