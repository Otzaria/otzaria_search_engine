//! How a semantic search is abandoned: what a [`SemanticCancellationToken`] holds, and the
//! points at which [`SearchEngine::search_semantic`] looks at it.
//!
//! The application searches as the user types, so every query but the last is obsolete
//! before it finishes, and a semantic query embeds the text and then scans every stored
//! vector, which over the library is on the order of a second. A token is how the
//! application says a search is obsolete. The search looks at it before its lexical phase;
//! the sidecar looks at it throughout the semantic half (before and after it embeds the
//! query, every 1,024 records of the vector scan, before and after fusion); and the search
//! looks again before it hydrates the sidecar's results and before it paints the page, the
//! two stages after the sidecar whose cost grows with the page. A lexical fallback, which a
//! search is when no session can serve it, is looked at before it runs and once its page is
//! ready. The first look after the token is cancelled ends the search with a
//! [`SemanticError`] of kind `Cancelled`.
//!
//! At the crate root for the reason `semantic_errors` is: flutter_rust_bridge must not
//! generate bindings for it. Dart sees the token, in `crate::api`, and nothing here.
//!
//! [`SemanticCancellationToken`]: crate::api::search_engine::SemanticCancellationToken
//! [`SearchEngine::search_semantic`]: crate::api::search_engine::SearchEngine::search_semantic

use crate::api::search_engine::SemanticError;

/// What a token holds: with the sidecar, its own `CancellationToken`, so that a search hands
/// the sidecar the very flag the application cancels.
#[cfg(feature = "semantic-integration")]
pub(crate) use otzaria_semantic_search::cancellation::CancellationToken as SearchCancellation;

/// What a token holds without the sidecar: a flag of the same shape, since a search there is
/// never more than its lexical fallback.
#[cfg(not(feature = "semantic-integration"))]
#[derive(Debug, Default)]
pub(crate) struct SearchCancellation(std::sync::atomic::AtomicBool);

#[cfg(not(feature = "semantic-integration"))]
impl SearchCancellation {
    /// One-way, as the sidecar's: the flag publishes nothing but the instruction to stop, so
    /// a relaxed store is enough, and a search sees it at its next look.
    pub(crate) fn cancel(&self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Where a search looks at its token itself, in the order a search reaches them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchCheckpoint {
    /// Before anything: before the lexical phase, or before a lexical fallback runs.
    Start,
    /// After the lexical phase, where the token is handed to the sidecar, whose first act is
    /// to look at it. The search does not look here itself; a test is told the search got
    /// this far, so that it can cancel here and see the sidecar stop the search.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    Sidecar,
    /// The sidecar has answered: before its results are hydrated from the index.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    Hydration,
    /// The page is hydrated and cut: before its group members are hydrated and its snippets
    /// painted.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    Painting,
    /// A lexical fallback's page is ready: before it is returned.
    Fallback,
    /// A passage highlight is about to embed the clauses of one more line.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    Highlight,
}

/// End the search at `checkpoint` if its token has been cancelled.
pub(crate) fn look(
    cancel: &SearchCancellation,
    checkpoint: SearchCheckpoint,
) -> Result<(), SemanticError> {
    reached(checkpoint, cancel);
    if cancel.is_cancelled() {
        return Err(SemanticError::cancelled());
    }
    Ok(())
}

/// Tell a test that a search reached `checkpoint`: nothing, outside a test build.
#[cfg(not(test))]
#[inline(always)]
pub(crate) fn reached(_checkpoint: SearchCheckpoint, _cancel: &SearchCancellation) {}

#[cfg(test)]
pub(crate) use probe::{cancelling_at, reached};

/// What a search's checkpoints report to a test running it on the same thread, which can
/// cancel the search's token at one of them: a search stopped at a point of the test's
/// choosing, rather than a timer raced against it. Thread-local, so tests running side by
/// side see only their own searches; outside [`cancelling_at`] it records nothing.
#[cfg(test)]
mod probe {
    use super::{SearchCancellation, SearchCheckpoint};
    use std::cell::{Cell, RefCell};

    thread_local! {
        static ACTIVE: Cell<bool> = const { Cell::new(false) };
        static CANCEL_AT: Cell<Option<SearchCheckpoint>> = const { Cell::new(None) };
        static REACHED: RefCell<Vec<SearchCheckpoint>> = const { RefCell::new(Vec::new()) };
    }

    /// Run `body`, cancelling the token of a search it runs once that search reaches
    /// `cancel_at` (never, for `None`), and return its result with every checkpoint the
    /// searches it ran reached, in order.
    pub(crate) fn cancelling_at<R>(
        cancel_at: Option<SearchCheckpoint>,
        body: impl FnOnce() -> R,
    ) -> (R, Vec<SearchCheckpoint>) {
        struct Deactivate;
        impl Drop for Deactivate {
            fn drop(&mut self) {
                ACTIVE.set(false);
                CANCEL_AT.set(None);
            }
        }

        REACHED.take();
        CANCEL_AT.set(cancel_at);
        ACTIVE.set(true);
        let deactivate = Deactivate;
        let result = body();
        drop(deactivate);
        (result, REACHED.take())
    }

    pub(crate) fn reached(checkpoint: SearchCheckpoint, cancel: &SearchCancellation) {
        if !ACTIVE.get() {
            return;
        }
        REACHED.with_borrow_mut(|reached| reached.push(checkpoint));
        if CANCEL_AT.get() == Some(checkpoint) {
            cancel.cancel();
        }
    }
}
