//! The ranked hits of recent searches, so that the next page of a search, or its
//! count, does not run the search again.
//!
//! A search is known by its built query and the index snapshot it ran on: the
//! query's `Debug` text is hashed as it is written, so equal queries share an entry
//! however their parameters were spelled, and a snapshot is its segments, in order,
//! with their deletes — a commit that changed the index can never be served an older
//! one, and a reload that changed nothing keeps the entries. A query whose `Debug`
//! text is too large to hash quickly is not cached.

use std::collections::HashMap;
use std::fmt::{self, Write as _};
use std::hash::{DefaultHasher, Hasher};
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use lru::LruCache;
use tantivy::query::Query;
use tantivy::{DocAddress, Searcher};

/// Searches remembered at once.
const ENTRIES: usize = 16;
/// Pages ranked ahead of the one asked for, so the next ones come from the cache.
const PREFETCH_PAGES: usize = 3;
/// The longest ranking kept per order (8 bytes a hit); deeper pages run the search.
const MAX_RANKED: usize = 20_000;
/// Beyond this much `Debug` text a query is not worth fingerprinting.
const MAX_FINGERPRINT_BYTES: usize = 4 << 20;

/// The index snapshot `searcher` reads: its segments in the order its addresses use them,
/// and their deletes. The reader's generation id changes on every reload, even one that
/// found nothing new.
pub(crate) fn snapshot_of(searcher: &Searcher) -> u64 {
    let mut hasher = DefaultHasher::new();
    for reader in searcher.segment_readers() {
        hasher.write(reader.segment_id().uuid_string().as_bytes());
        hasher.write_u64(reader.delete_opstamp().unwrap_or(u64::MAX));
    }
    hasher.finish()
}

/// One search: the index snapshot it ran on, the book order it ranked by and its query's
/// fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PageKey {
    snapshot: u64,
    cache_epoch: u64,
    order: u64,
    query: u128,
}

impl PageKey {
    pub(crate) fn with_epoch(mut self, epoch: u64) -> Self {
        self.cache_epoch = epoch;
        self
    }

    /// `None` when the query is too large to fingerprint. `order` is the epoch of the book
    /// order mapping (`book_order::next_epoch`).
    pub(crate) fn new(snapshot: u64, order: u64, query: &dyn Query) -> Option<Self> {
        let mut writer = FingerprintWriter {
            low: DefaultHasher::new(),
            high: DefaultHasher::new(),
            written: 0,
        };
        writer.high.write_u8(0xA5);
        write!(writer, "{query:?}").ok()?;
        Some(Self {
            snapshot,
            cache_epoch: 0,
            order,
            query: (u128::from(writer.high.finish()) << 64) | u128::from(writer.low.finish()),
        })
    }
}

struct FingerprintWriter {
    low: DefaultHasher,
    high: DefaultHasher,
    written: usize,
}

impl fmt::Write for FingerprintWriter {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.written += s.len();
        if self.written > MAX_FINGERPRINT_BYTES {
            return Err(fmt::Error);
        }
        self.low.write(s.as_bytes());
        self.high.write(s.as_bytes());
        Ok(())
    }
}

/// How many hits to rank for a page at `offset` of `limit` hits, so the following
/// pages are cached too; `None` when that is deeper than the cache keeps.
pub(crate) fn ranked_window(offset: u32, limit: u32) -> Option<usize> {
    let end = offset as usize + limit as usize;
    (limit > 0 && end <= MAX_RANKED)
        .then(|| (end + PREFETCH_PAGES * limit as usize).min(MAX_RANKED))
}

/// The best hits of one order, in order: every hit when `complete`.
struct Ranked {
    addresses: Vec<DocAddress>,
    complete: bool,
}

#[derive(Default)]
struct Entry {
    total: Option<u32>,
    book_counts: Option<Arc<HashMap<String, u32>>>,
    /// Indexed by the caller's order slot.
    ranked: [Option<Arc<Ranked>>; 3],
}

/// What a cached search answers for one page.
pub(crate) struct CachedPage {
    pub(crate) addresses: Vec<DocAddress>,
    pub(crate) total: Option<u32>,
    pub(crate) book_counts: Option<Arc<HashMap<String, u32>>>,
}

pub(crate) struct PageCache(Mutex<LruCache<PageKey, Entry>>, AtomicU64);

impl Default for PageCache {
    fn default() -> Self {
        Self(
            Mutex::new(LruCache::new(
                NonZeroUsize::new(ENTRIES).expect("cache size is non-zero"),
            )),
            AtomicU64::default(),
        )
    }
}

impl PageCache {
    pub(crate) fn epoch(&self) -> u64 {
        self.1.load(Ordering::Acquire)
    }

    /// The hits `offset..offset + limit` of order `slot`, when this search ranked
    /// that far — and its total and per-book counts, whichever it holds.
    pub(crate) fn page(
        &self,
        key: &PageKey,
        slot: usize,
        offset: u32,
        limit: u32,
    ) -> Option<CachedPage> {
        let mut cache = self.0.lock().unwrap();
        let entry = cache.get(key)?;
        let ranked = entry.ranked[slot].as_ref()?;
        let end = offset as usize + limit as usize;
        if end > ranked.addresses.len() && !ranked.complete {
            return None;
        }
        let len = ranked.addresses.len();
        Some(CachedPage {
            addresses: ranked.addresses[(offset as usize).min(len)..end.min(len)].to_vec(),
            total: entry.total,
            book_counts: entry.book_counts.clone(),
        })
    }

    pub(crate) fn total(&self, key: &PageKey) -> Option<u32> {
        self.0.lock().unwrap().get(key)?.total
    }

    pub(crate) fn book_counts(&self, key: &PageKey) -> Option<Arc<HashMap<String, u32>>> {
        self.0.lock().unwrap().get(key)?.book_counts.clone()
    }

    /// Records the best `addresses` of order `slot`, ranked with room for `window`
    /// hits: fewer than that means they are all the hits there are.
    pub(crate) fn store_ranked(
        &self,
        key: PageKey,
        slot: usize,
        addresses: Vec<DocAddress>,
        window: usize,
    ) {
        let complete = addresses.len() < window;
        let mut cache = self.0.lock().unwrap();
        if key.cache_epoch != self.epoch() {
            return;
        }
        let entry = cache.get_or_insert_mut(key, Entry::default);
        if complete {
            entry.total.get_or_insert(addresses.len() as u32);
        }
        let longer = entry.ranked[slot]
            .as_ref()
            .is_none_or(|old| !old.complete && old.addresses.len() < addresses.len());
        if longer {
            entry.ranked[slot] = Some(Arc::new(Ranked {
                addresses,
                complete,
            }));
        }
    }

    pub(crate) fn store_total(&self, key: PageKey, total: u32) {
        let mut cache = self.0.lock().unwrap();
        if key.cache_epoch != self.epoch() {
            return;
        }
        cache.get_or_insert_mut(key, Entry::default).total = Some(total);
    }

    pub(crate) fn store_book_counts(&self, key: PageKey, counts: Arc<HashMap<String, u32>>) {
        let mut cache = self.0.lock().unwrap();
        if key.cache_epoch != self.epoch() {
            return;
        }
        cache.get_or_insert_mut(key, Entry::default).book_counts = Some(counts);
    }

    pub(crate) fn clear(&self) {
        let mut cache = self.0.lock().unwrap();
        self.1.fetch_add(1, Ordering::AcqRel);
        cache.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::query::{AllQuery, EmptyQuery, TermQuery};
    use tantivy::schema::{Field, IndexRecordOption};
    use tantivy::Term;

    fn addresses(n: u32) -> Vec<DocAddress> {
        (0..n).map(|doc| DocAddress::new(0, doc)).collect()
    }

    fn term(text: &str) -> TermQuery {
        TermQuery::new(
            Term::from_field_text(Field::from_field_id(0), text),
            IndexRecordOption::Basic,
        )
    }

    #[test]
    fn keys_tell_queries_and_generations_apart() {
        let a = PageKey::new(1, 0, &term("שלום")).unwrap();
        assert_eq!(a, PageKey::new(1, 0, &term("שלום")).unwrap());
        assert_ne!(a, PageKey::new(1, 0, &term("שלומ")).unwrap());
        assert_ne!(a, PageKey::new(2, 0, &term("שלום")).unwrap());
        // Another book order ranks the same hits differently.
        assert_ne!(a, PageKey::new(1, 1, &term("שלום")).unwrap());
        assert_ne!(
            PageKey::new(1, 0, &AllQuery).unwrap(),
            PageKey::new(1, 0, &EmptyQuery).unwrap()
        );
    }

    #[test]
    fn window_ranks_ahead_and_stops_at_the_cap() {
        assert_eq!(ranked_window(0, 100), Some(400));
        assert_eq!(ranked_window(100, 100), Some(500));
        assert_eq!(ranked_window(0, 0), None);
        assert_eq!(ranked_window(19_950, 100), None);
        assert_eq!(ranked_window(19_900, 100), Some(MAX_RANKED));
    }

    #[test]
    fn serves_pages_inside_the_ranking_only() {
        let cache = PageCache::default();
        let key = PageKey::new(1, 0, &AllQuery).unwrap();
        cache.store_ranked(key, 0, addresses(400), 400);
        let page = cache.page(&key, 0, 100, 100).unwrap();
        assert_eq!(page.addresses, addresses(200)[100..].to_vec());
        assert_eq!(page.total, None);
        assert!(cache.page(&key, 0, 350, 100).is_none());
        assert!(cache.page(&key, 1, 0, 100).is_none());
    }

    #[test]
    fn a_complete_ranking_serves_every_page_and_the_total() {
        let cache = PageCache::default();
        let key = PageKey::new(1, 0, &AllQuery).unwrap();
        cache.store_ranked(key, 2, addresses(150), 400);
        assert_eq!(cache.total(&key), Some(150));
        assert_eq!(
            cache.page(&key, 2, 100, 100).unwrap().addresses,
            addresses(150)[100..].to_vec()
        );
        assert!(cache.page(&key, 2, 500, 100).unwrap().addresses.is_empty());
    }

    #[test]
    fn a_longer_ranking_replaces_a_shorter_one() {
        let cache = PageCache::default();
        let key = PageKey::new(1, 0, &AllQuery).unwrap();
        cache.store_ranked(key, 0, addresses(400), 400);
        cache.store_ranked(key, 0, addresses(800), 800);
        assert!(cache.page(&key, 0, 600, 100).is_some());
        cache.store_ranked(key, 0, addresses(200), 200);
        assert!(cache.page(&key, 0, 600, 100).is_some());
        cache.store_total(key, 900);
        assert_eq!(cache.page(&key, 0, 0, 10).unwrap().total, Some(900));
        cache.clear();
        assert!(cache.page(&key, 0, 0, 10).is_none());
    }
    #[test]
    fn results_collected_before_clear_cannot_refill_the_cache() {
        let cache = PageCache::default();
        let key = PageKey::new(1, 0, &AllQuery)
            .unwrap()
            .with_epoch(cache.epoch());
        cache.clear();
        cache.store_ranked(key, 0, addresses(20), 40);
        cache.store_total(key, 20);
        cache.store_book_counts(key, Arc::new(HashMap::from([("book".into(), 20)])));
        assert_eq!(cache.len(), 0);
        let current = key.with_epoch(cache.epoch());
        cache.store_ranked(current, 0, addresses(20), 40);
        assert_eq!(cache.len(), 1);
    }
}
