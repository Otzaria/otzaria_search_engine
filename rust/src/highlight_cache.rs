//! Highlight term sets and display patterns already computed on the current index
//! snapshot: every page of a search asks for the same term sets again, and every
//! book opened from its results for the same display pattern.
//!
//! Keyed by the searcher generation, so a commit is never served an older snapshot.
//! Values are computed outside the lock; two callers racing on one key both compute.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use lru::LruCache;
use tantivy::schema::Field;

use crate::highlight_matcher::PreparedHighlightMatcher;

/// Per-word term sets kept: a few words for each of the last searches.
const TERM_SETS: usize = 64;
/// Display patterns kept.
const PATTERNS: usize = 16;

#[derive(Hash, PartialEq, Eq)]
struct TermSetKey {
    generation: u64,
    field: Field,
    pattern: String,
    cap: usize,
}

/// A display pattern without the API wrapper, so the matcher can be shared.
pub(crate) struct DisplayPattern {
    pub(crate) combined_pattern: String,
    pub(crate) word_patterns: Vec<String>,
    pub(crate) word_boundary_eligible: Vec<bool>,
    pub(crate) matcher: Arc<PreparedHighlightMatcher>,
}

type CachedPattern = Option<Arc<DisplayPattern>>;

pub(crate) struct HighlightCache {
    term_sets: Mutex<LruCache<TermSetKey, Arc<Vec<String>>>>,
    patterns: Mutex<LruCache<(u64, String), CachedPattern>>,
}

impl Default for HighlightCache {
    fn default() -> Self {
        let size = |n| NonZeroUsize::new(n).expect("cache size is non-zero");
        Self {
            term_sets: Mutex::new(LruCache::new(size(TERM_SETS))),
            patterns: Mutex::new(LruCache::new(size(PATTERNS))),
        }
    }
}

impl HighlightCache {
    /// The index terms `pattern` matches in `field`, at most `cap` of them.
    pub(crate) fn term_set(
        &self,
        generation: u64,
        field: Field,
        pattern: &str,
        cap: usize,
        compute: impl FnOnce() -> Result<Vec<String>>,
    ) -> Result<Arc<Vec<String>>> {
        let key = TermSetKey {
            generation,
            field,
            pattern: pattern.to_string(),
            cap,
        };
        if let Some(terms) = self.term_sets.lock().unwrap().get(&key) {
            return Ok(Arc::clone(terms));
        }
        let terms = Arc::new(compute()?);
        self.term_sets.lock().unwrap().put(key, Arc::clone(&terms));
        Ok(terms)
    }

    /// The display pattern for the request `key` describes completely.
    pub(crate) fn pattern(
        &self,
        generation: u64,
        key: String,
        compute: impl FnOnce() -> Result<Option<DisplayPattern>>,
    ) -> Result<CachedPattern> {
        let key = (generation, key);
        if let Some(pattern) = self.patterns.lock().unwrap().get(&key) {
            return Ok(pattern.clone());
        }
        let pattern = compute()?.map(Arc::new);
        self.patterns.lock().unwrap().put(key, pattern.clone());
        Ok(pattern)
    }

    pub(crate) fn clear(&self) {
        self.term_sets.lock().unwrap().clear();
        self.patterns.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn term_sets_are_computed_once_per_generation_field_pattern_and_cap() {
        let cache = HighlightCache::default();
        let field = Field::from_field_id(0);
        let mut runs = 0;
        let mut get = |generation, pattern: &str, cap| {
            cache
                .term_set(generation, field, pattern, cap, || {
                    runs += 1;
                    Ok(vec![format!("{pattern}{generation}")])
                })
                .unwrap()
        };
        assert_eq!(*get(1, "שלום", 10), vec!["שלום1".to_string()]);
        assert_eq!(*get(1, "שלום", 10), vec!["שלום1".to_string()]);
        get(1, "שלום", 20);
        get(2, "שלום", 10);
        get(1, "עולם", 10);
        assert_eq!(runs, 4);
    }

    #[test]
    fn a_failed_computation_is_not_cached() {
        let cache = HighlightCache::default();
        let field = Field::from_field_id(0);
        assert!(cache
            .term_set(1, field, "x", 1, || Err(anyhow::anyhow!("boom")))
            .is_err());
        let terms = cache.term_set(1, field, "x", 1, || Ok(vec!["y".into()]));
        assert_eq!(*terms.unwrap(), vec!["y".to_string()]);
        assert!(cache.pattern(1, "k".into(), || Ok(None)).unwrap().is_none());
        cache.clear();
        assert!(cache
            .pattern(1, "k".into(), || Err(anyhow::anyhow!("boom")))
            .is_err());
    }
}
