//! Highlight term sets and display patterns already computed on the current index
//! snapshot: every page of a search asks for the same term sets again, and every
//! book opened from its results for the same display pattern.
//!
//! Keyed by the searcher generation, so a commit is never served an older snapshot.
//! Concurrent requests share one computation per key; unrelated keys remain independent.

use std::collections::HashMap;
use std::hash::Hash;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

use anyhow::Result;
use lru::LruCache;
use tantivy::schema::Field;

use crate::highlight_matcher::PreparedHighlightMatcher;

/// Per-word term sets kept: a few words for each of the last searches.
const TERM_SETS: usize = 64;
/// Display patterns kept.
const PATTERNS: usize = 16;

#[derive(Clone, Hash, PartialEq, Eq)]
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

struct RetainedPattern {
    combined_pattern: String,
    word_patterns: Vec<String>,
    word_boundary_eligible: Vec<bool>,
    matcher: Weak<PreparedHighlightMatcher>,
}

impl RetainedPattern {
    fn new(pattern: &DisplayPattern) -> Self {
        Self {
            combined_pattern: pattern.combined_pattern.clone(),
            word_patterns: pattern.word_patterns.clone(),
            word_boundary_eligible: pattern.word_boundary_eligible.clone(),
            matcher: Arc::downgrade(&pattern.matcher),
        }
    }

    fn restore(entry: &Option<Self>) -> Option<CachedPattern> {
        match entry {
            None => Some(None),
            Some(pattern) => Some(Some(Arc::new(DisplayPattern {
                combined_pattern: pattern.combined_pattern.clone(),
                word_patterns: pattern.word_patterns.clone(),
                word_boundary_eligible: pattern.word_boundary_eligible.clone(),
                matcher: pattern.matcher.upgrade()?,
            }))),
        }
    }

    fn bytes(entry: &Option<Self>) -> usize {
        entry.as_ref().map_or(0, |pattern| {
            pattern.combined_pattern.capacity()
                + pattern.word_patterns.capacity() * std::mem::size_of::<String>()
                + pattern
                    .word_patterns
                    .iter()
                    .map(String::capacity)
                    .sum::<usize>()
                + pattern.word_boundary_eligible.capacity()
        })
    }
}

pub(crate) struct HighlightCache {
    term_sets: Mutex<LruCache<TermSetKey, Arc<Vec<String>>>>,
    patterns: Mutex<LruCache<(u64, String), Option<RetainedPattern>>>,
    epoch: AtomicU64,
    term_work: WorkGate<TermSetKey>,
    pattern_work: WorkGate<(u64, String)>,
}

impl Default for HighlightCache {
    fn default() -> Self {
        let size = |n| NonZeroUsize::new(n).expect("cache size is non-zero");
        Self {
            term_sets: Mutex::new(LruCache::new(size(TERM_SETS))),
            patterns: Mutex::new(LruCache::new(size(PATTERNS))),
            epoch: AtomicU64::default(),
            term_work: WorkGate::default(),
            pattern_work: WorkGate::default(),
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
        let epoch = self.epoch.load(Ordering::Acquire);
        let key = TermSetKey {
            generation,
            field,
            pattern: pattern.to_string(),
            cap,
        };
        if let Some(terms) = self.term_sets.lock().unwrap().get(&key) {
            return Ok(Arc::clone(terms));
        }
        let _work = self.term_work.enter(key.clone())?;
        if let Some(terms) = self.term_sets.lock().unwrap().get(&key) {
            return Ok(Arc::clone(terms));
        }
        let terms = Arc::new(compute()?);
        crate::lexical_cancellation::check_current()?;
        let bytes = |terms: &Arc<Vec<String>>| {
            terms.capacity() * std::mem::size_of::<String>()
                + terms.iter().map(String::capacity).sum::<usize>()
        };
        let mut cache = self.term_sets.lock().unwrap();
        if self.epoch.load(Ordering::Acquire) == epoch
            && key.pattern.capacity() + bytes(&terms) <= 8 << 20
        {
            cache.put(key, Arc::clone(&terms));
            while cache
                .iter()
                .map(|(key, terms)| key.pattern.capacity() + bytes(terms))
                .sum::<usize>()
                > 8 << 20
            {
                cache.pop_lru();
            }
        }
        Ok(terms)
    }

    /// The display pattern for the request `key` describes completely.
    pub(crate) fn pattern(
        &self,
        generation: u64,
        key: String,
        compute: impl FnOnce() -> Result<Option<DisplayPattern>>,
    ) -> Result<CachedPattern> {
        let epoch = self.epoch.load(Ordering::Acquire);
        let key = (generation, key);
        if let Some(pattern) = self
            .patterns
            .lock()
            .unwrap()
            .get(&key)
            .and_then(RetainedPattern::restore)
        {
            return Ok(pattern);
        }
        let _work = self.pattern_work.enter(key.clone())?;
        if let Some(pattern) = self
            .patterns
            .lock()
            .unwrap()
            .get(&key)
            .and_then(RetainedPattern::restore)
        {
            return Ok(pattern);
        }
        let pattern = compute()?.map(Arc::new);
        crate::lexical_cancellation::check_current()?;
        let retained = pattern.as_deref().map(RetainedPattern::new);
        let mut cache = self.patterns.lock().unwrap();
        // Text payload only: automata are weak references owned by their consumers.
        if self.epoch.load(Ordering::Acquire) == epoch
            && key.1.capacity() + RetainedPattern::bytes(&retained) <= 4 << 20
        {
            cache.put(key, retained);
            while cache
                .iter()
                .map(|(key, pattern)| key.1.capacity() + RetainedPattern::bytes(pattern))
                .sum::<usize>()
                > 4 << 20
            {
                cache.pop_lru();
            }
        }
        Ok(pattern)
    }

    pub(crate) fn clear(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.term_sets.lock().unwrap().clear();
        self.patterns.lock().unwrap().clear();
        self.term_work.clear_idle();
        self.pattern_work.clear_idle();
    }
}

#[derive(Default)]
struct Work {
    busy: Mutex<bool>,
    changed: Condvar,
}

pub(crate) struct WorkGate<K>(Mutex<HashMap<K, Arc<Work>>>);

impl<K> Default for WorkGate<K> {
    fn default() -> Self {
        Self(Mutex::new(HashMap::new()))
    }
}

impl<K: Eq + Hash> WorkGate<K> {
    pub(crate) fn clear_idle(&self) {
        self.0
            .lock()
            .unwrap()
            .retain(|_, work| Arc::strong_count(work) > 1);
    }

    #[cfg(test)]
    pub(crate) fn retained_keys(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub(crate) fn enter(&self, key: K) -> Result<Working> {
        let work = {
            let mut active = self.0.lock().unwrap();
            active.retain(|_, work| Arc::strong_count(work) > 1);
            Arc::clone(active.entry(key).or_default())
        };
        let mut busy = work.busy.lock().unwrap();
        while *busy {
            crate::lexical_cancellation::check_current()?;
            busy = work
                .changed
                .wait_timeout(busy, Duration::from_millis(10))
                .unwrap()
                .0;
        }
        crate::lexical_cancellation::check_current()?;
        *busy = true;
        drop(busy);
        Ok(Working(work))
    }
}

pub(crate) struct Working(Arc<Work>);

impl Drop for Working {
    fn drop(&mut self) {
        *self.0.busy.lock().unwrap() = false;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clearing_idle_work_releases_the_key_payload() {
        let gate = WorkGate::default();
        let key = Arc::new("x".repeat(1 << 20));
        let weak = Arc::downgrade(&key);
        drop(gate.enter(key.clone()).unwrap());
        drop(key);
        assert!(weak.upgrade().is_some());
        gate.clear_idle();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn clearing_highlights_releases_idle_term_and_pattern_work_keys() {
        let cache = HighlightCache::default();
        cache
            .term_set(1, Field::from_field_id(0), "term", 1, || {
                Ok(vec!["x".into()])
            })
            .unwrap();
        cache.pattern(1, "pattern".into(), || Ok(None)).unwrap();
        assert_eq!(cache.term_work.retained_keys(), 1);
        assert_eq!(cache.pattern_work.retained_keys(), 1);
        cache.clear();
        assert_eq!(cache.term_work.retained_keys(), 0);
        assert_eq!(cache.pattern_work.retained_keys(), 0);
    }

    #[test]
    fn clearing_work_keeps_active_and_waiting_requests_on_the_same_gate() {
        let gate = WorkGate::default();
        let first = gate.enter("active").unwrap();
        let original = Arc::downgrade(&first.0);
        gate.clear_idle();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let next = gate.enter("active").unwrap();
                sender
                    .send(Arc::ptr_eq(&next.0, &original.upgrade().unwrap()))
                    .unwrap();
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while Arc::strong_count(&first.0) < 3 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "waiter did not enter the gate"
                );
                std::thread::yield_now();
            }
            assert!(matches!(
                receiver.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            gate.clear_idle();
            drop(first);
            assert!(receiver.recv_timeout(Duration::from_secs(10)).unwrap());
        });
        gate.clear_idle();
        assert_eq!(gate.retained_keys(), 0);
    }

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
    #[test]
    fn concurrent_term_requests_compute_once_and_share_the_same_value() {
        let cache = HighlightCache::default();
        let starts = std::sync::Barrier::new(8);
        let runs = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        starts.wait();
                        cache
                            .term_set(1, Field::from_field_id(0), "שלום", 100, || {
                                runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                std::thread::sleep(Duration::from_millis(30));
                                Ok(vec!["שלום".into()])
                            })
                            .unwrap()
                    })
                })
                .collect();
            let values: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
            assert!(values.iter().all(|value| Arc::ptr_eq(value, &values[0])));
        });
        assert_eq!(runs.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn a_large_term_set_is_returned_without_retaining_it() {
        let cache = HighlightCache::default();
        let value = cache
            .term_set(1, Field::from_field_id(0), "x", 100, || {
                Ok(vec!["x".repeat(9 << 20)])
            })
            .unwrap();
        assert_eq!(value[0].len(), 9 << 20);
        assert_eq!(cache.term_sets.lock().unwrap().len(), 0);
    }
    fn display_pattern() -> DisplayPattern {
        DisplayPattern {
            combined_pattern: "שלום".into(),
            word_patterns: vec!["שלום".into()],
            word_boundary_eligible: vec![true],
            matcher: Arc::new(
                PreparedHighlightMatcher::new(
                    vec![vec![(String::new(), "שלום".into(), String::new())]],
                    crate::gap_phrase::Proximity::Ordered(vec![]),
                    vec![true],
                )
                .unwrap(),
            ),
        }
    }

    #[test]
    fn cached_patterns_do_not_retain_the_matcher_after_its_last_consumer() {
        let cache = HighlightCache::default();
        let value = cache
            .pattern(1, "k".into(), || Ok(Some(display_pattern())))
            .unwrap()
            .unwrap();
        let weak = Arc::downgrade(&value.matcher);
        let again = cache
            .pattern(1, "k".into(), || panic!("matcher should be shared"))
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&value.matcher, &again.matcher));
        drop(value);
        assert!(weak.upgrade().is_some());
        drop(again);
        assert!(weak.upgrade().is_none());
        assert!(cache
            .pattern(1, "k".into(), || Ok(Some(display_pattern())))
            .unwrap()
            .is_some());
    }

    #[test]
    fn clearing_during_computation_does_not_refill_evicted_payloads() {
        let cache = HighlightCache::default();
        let terms = cache
            .term_set(1, Field::from_field_id(0), "x", 1, || {
                cache.clear();
                Ok(vec!["x".into()])
            })
            .unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(cache.term_sets.lock().unwrap().len(), 0);
        let pattern = cache
            .pattern(1, "k".into(), || {
                cache.clear();
                Ok(Some(display_pattern()))
            })
            .unwrap();
        assert!(pattern.is_some());
        assert_eq!(cache.patterns.lock().unwrap().len(), 0);
    }

    #[test]
    fn concurrent_patterns_share_one_matcher_without_holding_the_cache_lock() {
        let cache = HighlightCache::default();
        let starts = std::sync::Barrier::new(4);
        let runs = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|scope| {
            let jobs: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        starts.wait();
                        cache
                            .pattern(1, "k".into(), || {
                                runs.fetch_add(1, Ordering::Relaxed);
                                assert!(cache.patterns.try_lock().is_ok());
                                std::thread::sleep(Duration::from_millis(20));
                                Ok(Some(display_pattern()))
                            })
                            .unwrap()
                            .unwrap()
                    })
                })
                .collect();
            let values: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
            assert!(values
                .iter()
                .all(|value| Arc::ptr_eq(&value.matcher, &values[0].matcher)));
        });
        assert_eq!(runs.load(Ordering::Relaxed), 1);
    }
}
