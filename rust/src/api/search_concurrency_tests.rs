use super::*;
use std::cell::RefCell;
use std::sync::mpsc;
use std::time::Duration;
use tempfile::TempDir;

type AfterGate = Box<dyn FnOnce(&TermCacheKey)>;

thread_local! {
    static AFTER_GATE: RefCell<Option<AfterGate>> = const { RefCell::new(None) };
}

// Only this test's primer thread installs a hook. Taking it before calling it
// keeps the hook one-shot even if the callback reenters.
pub(super) fn after_materialize_gate(key: &TermCacheKey) {
    let hook = AFTER_GATE.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = hook {
        hook(key);
    }
}

const LINES: [&str; 4] = [
    "בראשית ברא אלהים את השמים",
    "ויאמר אלהים יהי אור ברא",
    "אלהים ברא",
    "ברא דבר אחר",
];

fn fixture() -> (SearchEngine, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
    // Both calls must contend for the same snapshot, even on a slow runner:
    // a delayed watcher reload would otherwise give the search a different key.
    engine.index_reader = engine
        .index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .unwrap();
    for (ordinal, text) in LINES.iter().enumerate() {
        engine
            .add_document(
                ordinal as u32,
                "ספר",
                "שורה",
                "/order/book",
                text,
                ordinal as u64,
                false,
                "id:1",
                None,
                None,
            )
            .unwrap();
        // Keep several real segment scans under each term materialization.
        engine.commit().unwrap();
    }
    assert_eq!(engine.index_reader.searcher().segment_readers().len(), 4);
    (engine, dir)
}

fn any_order_search(engine: &SearchEngine) -> Result<Vec<SearchResult>> {
    engine.search_advanced(
        "אלהים ברא".into(),
        String::new(),
        vec!["/order/book".into()],
        10,
        0,
        0,
        0,
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        HashMap::new(),
        ResultsOrder::Catalogue,
        false,
        false,
        SearchScope::WordDistance,
        SearchScope::WordDistance,
        None,
        None,
        None,
        Some(true),
        None,
    )
}

#[test]
fn cold_any_order_search_on_rayon_finishes_while_highlight_primer_owns_term_work() {
    const DEADLINE: Duration = Duration::from_secs(10);
    let (engine, _dir) = fixture();
    let generation = engine.index_reader.searcher().generation().generation_id();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(2)
        .build()
        .unwrap();

    let (ready, initially_cold, search_before_release, primer, search_thread) =
        std::thread::scope(|scope| {
            // The release sender belongs to this scope so an unexpected panic also
            // disconnects the receiver before scoped threads are joined.
            let (release_tx, release_rx) = mpsc::channel();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            let engine = &engine;
            let primer = scope.spawn(move || {
                assert!(rayon::current_thread_index().is_none());
                AFTER_GATE.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move |key| {
                        assert!(ready_tx.send(key.clone()).is_ok());
                        // A disconnected sender also releases the owner on failure.
                        let _ = release_rx.recv();
                    }));
                });
                engine.generate_index_highlight_pattern(
                    "אלהים ברא".into(),
                    0,
                    HashMap::new(),
                    HashMap::new(),
                    HashMap::new(),
                    Some(true),
                    None,
                )
            });
            let ready = ready_rx.recv_timeout(DEADLINE);
            let owned_key = ready.as_ref().ok().cloned();
            let initially_cold = owned_key
                .as_ref()
                .is_some_and(|key| engine.term_cache.lock().unwrap().peek(key).is_none());
            let pool = &pool;
            let search = scope.spawn(move || {
                let results = pool.install(|| {
                    assert!(rayon::current_thread_index().is_some());
                    any_order_search(engine)
                });
                // The primer is still paused before computing this key. Its new
                // cache entry can therefore only have come from the real search.
                let computed_owned_key = owned_key
                    .as_ref()
                    .is_some_and(|key| engine.term_cache.lock().unwrap().peek(key).is_some());
                let _ = finished_tx.send((results, computed_owned_key));
            });
            let search_before_release = ready
                .as_ref()
                .ok()
                .map(|_| finished_rx.recv_timeout(DEADLINE));

            // Never panic on the timeout until the real primer is released and both
            // APIs have returned. The old blocking gate therefore fails cleanly.
            let _ = release_tx.send(());
            (
                ready,
                initially_cold,
                search_before_release,
                primer.join(),
                search.join(),
            )
        });

    assert!(search_thread.is_ok(), "the Rayon search thread panicked");
    let owned_key = ready.expect("the real primer must acquire a cold term gate");
    assert_eq!(owned_key.generation, generation);
    assert!(
        initially_cold,
        "the owned key must be absent before the search"
    );
    let (results, computed_owned_key) = search_before_release
        .expect("the primer acquired its gate")
        .expect("a Rayon search must finish while the primer still owns its term gate");
    let results = results.unwrap();
    assert!(
        computed_owned_key,
        "the search must compute the exact generation/field/term/budgets owned by the primer"
    );
    assert_eq!(
        results
            .iter()
            .map(|result| result.segment)
            .collect::<Vec<_>>(),
        [0, 2]
    );
    let pattern = primer.unwrap().unwrap().expect("the primer must finish");
    let matcher = pattern.matcher.expect("the indexed pattern has a matcher");
    for (segment, line) in LINES.iter().enumerate() {
        assert_eq!(
            !matcher.find_matches((*line).into(), Vec::new()).is_empty(),
            [0, 2].contains(&segment),
            "search and the completed primer must agree for line {segment}"
        );
    }
}
