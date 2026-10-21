//! Cancelling a lexical search by the request id its caller gave it.
//!
//! The application searches as the user types, so a search is often obsolete before
//! it ends, and a heavy one holds a core while the next search waits. A caller tags
//! the calls of one search (its stream, count and per-book count) with one request
//! id, and [`LexicalCancellations::cancel`] stops all of them. A search looks at its
//! flag before it runs, at each segment, every [`CHECK_EVERY`] documents within one,
//! and before it returns or streams anything. A cancelled count fails with
//! [`LexicalSearchCancelled`]; a cancelled stream ends without another event.
//!
//! A cancel that arrives before the call it cancels is remembered, so the call stops
//! at once when it starts; [`REMEMBERED`] such ids are kept.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tantivy::query::{EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, SegmentReader, TantivyError, TERMINATED};

/// Documents a scorer passes between two looks at its flag.
const CHECK_EVERY: u32 = 4_096;
/// Cancelled ids with no call running that are still remembered.
const REMEMBERED: usize = 1_024;

/// The error of a cancelled lexical search. Its message is
/// [`LexicalSearchCancelled::MESSAGE`], which the application matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LexicalSearchCancelled;

impl LexicalSearchCancelled {
    pub const MESSAGE: &'static str = "lexical search cancelled";

    pub(crate) fn is(err: &anyhow::Error) -> bool {
        err.downcast_ref::<Self>().is_some()
    }
}

impl fmt::Display for LexicalSearchCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::MESSAGE)
    }
}

impl std::error::Error for LexicalSearchCancelled {}

struct Entry {
    flag: Arc<AtomicBool>,
    running: usize,
}

#[derive(Default)]
struct Registry {
    entries: HashMap<u32, Entry>,
    /// Cancelled ids with no call running, oldest first.
    remembered: VecDeque<u32>,
}

impl Registry {
    fn remember(&mut self, id: u32) {
        self.remembered.push_back(id);
        while self.remembered.len() > REMEMBERED {
            if let Some(old) = self.remembered.pop_front() {
                if self.entries.get(&old).is_some_and(|e| e.running == 0) {
                    self.entries.remove(&old);
                }
            }
        }
    }
}

/// The request ids of running lexical searches.
#[derive(Default)]
pub(crate) struct LexicalCancellations(Mutex<Registry>);

impl LexicalCancellations {
    /// Starts one call of request `id`; no id, no cancellation.
    pub(crate) fn begin(&self, id: Option<u32>) -> LexicalRequest<'_> {
        let Some(id) = id else {
            return LexicalRequest {
                flag: None,
                registry: self,
                id: None,
            };
        };
        let mut registry = self.0.lock().unwrap();
        let entry = registry.entries.entry(id).or_insert_with(|| Entry {
            flag: Arc::default(),
            running: 0,
        });
        entry.running += 1;
        LexicalRequest {
            flag: Some(Arc::clone(&entry.flag)),
            registry: self,
            id: Some(id),
        }
    }

    /// Stops every call of request `id`, running or still to come.
    pub(crate) fn cancel(&self, id: u32) {
        let mut registry = self.0.lock().unwrap();
        let entry = registry.entries.entry(id).or_insert_with(|| Entry {
            flag: Arc::default(),
            running: 0,
        });
        let first = !entry.flag.swap(true, Ordering::Relaxed);
        if first && entry.running == 0 {
            registry.remember(id);
        }
    }

    fn end(&self, id: u32) {
        let mut registry = self.0.lock().unwrap();
        let Some(entry) = registry.entries.get_mut(&id) else {
            return;
        };
        entry.running -= 1;
        if entry.running > 0 {
            return;
        }
        if entry.flag.load(Ordering::Relaxed) {
            registry.remember(id);
        } else {
            registry.entries.remove(&id);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap().entries.len()
    }
}

/// One call of a request; ends it when dropped.
pub(crate) struct LexicalRequest<'a> {
    flag: Option<Arc<AtomicBool>>,
    registry: &'a LexicalCancellations,
    id: Option<u32>,
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Cancellation shared with parallel hydration workers.
#[derive(Clone, Default)]
pub(crate) struct LexicalFlag(Option<Arc<AtomicBool>>);

impl LexicalFlag {
    pub(crate) fn shared(flag: &Arc<AtomicBool>) -> Self {
        Self(Some(Arc::clone(flag)))
    }

    pub(crate) fn current() -> Self {
        Self(CURRENT.with(|current| current.borrow().clone()))
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if self
            .0
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err(LexicalSearchCancelled.into());
        }
        Ok(())
    }

    pub(crate) fn check_tantivy(&self) -> tantivy::Result<()> {
        match &self.0 {
            Some(flag) => cancelled(flag),
            None => Ok(()),
        }
    }

    pub(crate) fn wrap(&self, query: Box<dyn Query>) -> Box<dyn Query> {
        match &self.0 {
            Some(flag) => Box::new(CancellableQuery {
                inner: query,
                flag: Arc::clone(flag),
            }),
            None => query,
        }
    }
}

pub(crate) fn check_current() -> anyhow::Result<()> {
    LexicalFlag::current().check()
}

struct RestoreScope(Option<Arc<AtomicBool>>);

impl Drop for RestoreScope {
    fn drop(&mut self) {
        CURRENT.with(|current| *current.borrow_mut() = self.0.take());
    }
}

impl LexicalRequest<'_> {
    pub(crate) fn enter(&self) -> impl Drop {
        RestoreScope(CURRENT.with(|current| current.replace(self.flag.clone())))
    }

    /// Carries cancellation through query preparation and restores nested callers.
    pub(crate) fn scope<T>(&self, run: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        self.check()?;
        let _restore = RestoreScope(CURRENT.with(|current| current.replace(self.flag.clone())));
        self.finish(run())
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.flag
            .as_ref()
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
    }

    pub(crate) fn check(&self) -> anyhow::Result<()> {
        if self.is_cancelled() {
            return Err(LexicalSearchCancelled.into());
        }
        Ok(())
    }

    /// `result`, unless the request was cancelled while it ran: a cancelled scan
    /// may have stopped early, so its result is never handed out.
    pub(crate) fn finish<T>(&self, result: anyhow::Result<T>) -> anyhow::Result<T> {
        self.check()?;
        result
    }

    /// `query`, stopping its segment scans once the request is cancelled.
    pub(crate) fn wrap(&self, query: Box<dyn Query>) -> Box<dyn Query> {
        match &self.flag {
            Some(flag) => Box::new(CancellableQuery {
                inner: query,
                flag: Arc::clone(flag),
            }),
            None => query,
        }
    }
}

impl Drop for LexicalRequest<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.registry.end(id);
        }
    }
}

fn cancelled(flag: &AtomicBool) -> tantivy::Result<()> {
    if flag.load(Ordering::Relaxed) {
        return Err(TantivyError::InternalError(
            LexicalSearchCancelled::MESSAGE.to_string(),
        ));
    }
    Ok(())
}

/// `inner`, whose scans stop and fail once `flag` is raised. Its `Debug` is the
/// inner query's: the page cache keys on it, and a cancelled scan stores nothing.
struct CancellableQuery {
    inner: Box<dyn Query>,
    flag: Arc<AtomicBool>,
}

impl Clone for CancellableQuery {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.box_clone(),
            flag: Arc::clone(&self.flag),
        }
    }
}

impl fmt::Debug for CancellableQuery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, f)
    }
}

impl Query for CancellableQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        cancelled(&self.flag)?;
        Ok(Box::new(CancellableWeight {
            inner: self.inner.weight(enable_scoring)?,
            flag: Arc::clone(&self.flag),
        }))
    }

    fn query_terms<'a>(&'a self, visitor: &mut dyn FnMut(&'a tantivy::Term, bool)) {
        self.inner.query_terms(visitor);
    }
}

struct CancellableWeight {
    inner: Box<dyn Weight>,
    flag: Arc<AtomicBool>,
}

impl Weight for CancellableWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        cancelled(&self.flag)?;
        Ok(Box::new(CancellableScorer {
            inner: self.inner.scorer(reader, boost)?,
            flag: Arc::clone(&self.flag),
            until_check: CHECK_EVERY,
            stopped: false,
        }))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        self.inner.explain(reader, doc)
    }

    fn count(&self, reader: &SegmentReader) -> tantivy::Result<u32> {
        cancelled(&self.flag)?;
        let count = self.inner.count(reader)?;
        cancelled(&self.flag)?;
        Ok(count)
    }

    fn for_each(
        &self,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(DocId, Score),
    ) -> tantivy::Result<()> {
        let mut scorer = self.scorer(reader, 1.0)?;
        let mut doc = scorer.doc();
        while doc != TERMINATED {
            callback(doc, scorer.score());
            doc = scorer.advance();
        }
        cancelled(&self.flag)
    }

    fn for_each_no_score(
        &self,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(&[DocId]),
    ) -> tantivy::Result<()> {
        let mut scorer = self.scorer(reader, 1.0)?;
        let mut buffer = [0; tantivy::COLLECT_BLOCK_BUFFER_LEN];
        loop {
            let filled = scorer.fill_buffer(&mut buffer);
            callback(&buffer[..filled]);
            if filled != buffer.len() {
                break;
            }
        }
        cancelled(&self.flag)
    }

    // Delegated so the inner weight keeps its block-max pruning; once cancelled,
    // an unbeatable threshold makes it skip what is left.
    fn for_each_pruning(
        &self,
        threshold: Score,
        reader: &SegmentReader,
        callback: &mut dyn FnMut(DocId, Score) -> Score,
    ) -> tantivy::Result<()> {
        cancelled(&self.flag)?;
        self.inner
            .for_each_pruning(threshold, reader, &mut |doc, score| {
                if self.flag.load(Ordering::Relaxed) {
                    return Score::MAX;
                }
                callback(doc, score)
            })?;
        cancelled(&self.flag)
    }
}

struct CancellableScorer {
    inner: Box<dyn Scorer>,
    flag: Arc<AtomicBool>,
    until_check: u32,
    stopped: bool,
}

impl CancellableScorer {
    fn stop_if_cancelled(&mut self) -> bool {
        if !self.stopped && self.flag.load(Ordering::Relaxed) {
            self.stopped = true;
        }
        self.stopped
    }
}

impl DocSet for CancellableScorer {
    fn advance(&mut self) -> DocId {
        self.until_check -= 1;
        if self.until_check == 0 {
            self.until_check = CHECK_EVERY;
            if self.stop_if_cancelled() {
                return TERMINATED;
            }
        }
        if self.stopped {
            return TERMINATED;
        }
        self.inner.advance()
    }

    fn seek(&mut self, target: DocId) -> DocId {
        if self.stop_if_cancelled() {
            return TERMINATED;
        }
        self.inner.seek(target)
    }

    fn fill_buffer(&mut self, buffer: &mut [DocId; tantivy::COLLECT_BLOCK_BUFFER_LEN]) -> usize {
        if self.stop_if_cancelled() {
            return 0;
        }
        self.inner.fill_buffer(buffer)
    }

    fn doc(&self) -> DocId {
        if self.stopped {
            return TERMINATED;
        }
        self.inner.doc()
    }

    fn size_hint(&self) -> u32 {
        self.inner.size_hint()
    }

    fn cost(&self) -> u64 {
        self.inner.cost()
    }
}

impl Scorer for CancellableScorer {
    fn score(&mut self) -> Score {
        self.inner.score()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::collector::{Count, TopDocs};
    use tantivy::query::AllQuery;
    use tantivy::schema::{Schema, STRING};
    use tantivy::{doc, Index};

    fn index(docs: usize) -> Index {
        let mut schema = Schema::builder();
        let field = schema.add_text_field("text", STRING);
        let index = Index::create_in_ram(schema.build());
        let mut writer = index.writer_with_num_threads(1, 50_000_000).unwrap();
        for n in 0..docs {
            writer.add_document(doc!(field => format!("w{n}"))).unwrap();
        }
        writer.commit().unwrap();
        index
    }

    #[test]
    fn an_uncancelled_request_counts_as_the_bare_query() {
        let index = index(10_000);
        let searcher = index.reader().unwrap().searcher();
        let calls = LexicalCancellations::default();
        let request = calls.begin(Some(7));
        let query = request.wrap(Box::new(AllQuery));
        assert_eq!(searcher.search(query.as_ref(), &Count).unwrap(), 10_000);
        let top = TopDocs::with_limit(3).order_by_score();
        assert_eq!(searcher.search(query.as_ref(), &top).unwrap().len(), 3);
        assert_eq!(format!("{query:?}"), format!("{:?}", AllQuery));
        assert!(request.finish(Ok(())).is_ok());
        drop(request);
        assert_eq!(calls.len(), 0);
    }

    #[test]
    fn a_cancel_mid_scan_stops_it_and_fails_the_search() {
        let index = index(20_000);
        let searcher = index.reader().unwrap().searcher();
        let calls = LexicalCancellations::default();
        let request = calls.begin(Some(1));
        let query = request.wrap(Box::new(AllQuery));
        let weight = query
            .weight(EnableScoring::disabled_from_searcher(&searcher))
            .unwrap();
        let mut seen = 0;
        let result = weight.for_each_no_score(searcher.segment_reader(0), &mut |docs| {
            seen += docs.len();
            if seen >= 1_000 {
                calls.cancel(1);
            }
        });
        assert!(result.is_err());
        assert!(seen < 20_000);
        let err = request.finish(Ok(5)).unwrap_err();
        assert!(LexicalSearchCancelled::is(&err));
        assert_eq!(err.to_string(), LexicalSearchCancelled::MESSAGE);
        assert!(searcher.search(query.as_ref(), &Count).is_err());
    }

    #[test]
    fn a_cancel_before_the_call_is_remembered_and_other_ids_run() {
        let calls = LexicalCancellations::default();
        calls.cancel(3);
        assert!(calls.begin(Some(3)).check().is_err());
        assert!(calls.begin(Some(4)).check().is_ok());
        assert!(calls.begin(None).check().is_ok());
        for id in 100..100 + REMEMBERED as u32 {
            calls.cancel(id);
        }
        assert_eq!(calls.len(), REMEMBERED);
        assert!(calls.begin(Some(3)).check().is_ok());
    }
    #[test]
    fn nested_scopes_restore_the_parent_and_parallel_workers_keep_the_flag() {
        let calls = LexicalCancellations::default();
        let parent = calls.begin(Some(1));
        parent
            .scope(|| {
                let flag = LexicalFlag::current();
                calls.cancel(1);
                assert!(flag.check().is_err());
                let child = calls.begin(Some(2));
                child.scope(|| {
                    check_current()?;
                    Ok(())
                })?;
                assert!(check_current().is_err());
                std::thread::scope(|scope| {
                    assert!(scope.spawn(|| flag.check()).join().unwrap().is_err())
                });
                Ok(())
            })
            .unwrap_err();
        assert!(check_current().is_ok());
    }
}
