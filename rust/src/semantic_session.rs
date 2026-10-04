//! A semantic search's results in the order shown, so its next pages continue them: a wider
//! window fuses into another order, and recomputing each page would repeat or skip lines.

use lru::LruCache;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tantivy::Searcher;

/// How many searches' sessions are kept: the one on screen, and a few the user may go back to.
pub(crate) const SESSION_CAPACITY: usize = 4;

/// How long a session no page was asked of is kept.
pub(crate) const SESSION_TTL: Duration = Duration::from_secs(10 * 60);

/// Everything that decides what a session's pages are.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SessionKey {
    pub(crate) query: String,
    /// Sorted and deduplicated: the filter is a set.
    pub(crate) facets: Vec<String>,
    /// The lexical mode, retrieval mode, grouping, marks and every ranking parameter.
    pub(crate) options: String,
    /// The index's segments and their deletes, in order: a reload that changed neither keeps
    /// the session, whose addresses then hold in the new searcher too.
    pub(crate) index_content: u64,
    pub(crate) library_generation: (u64, Option<i64>, bool),
    pub(crate) vectors_generation: Option<u64>,
    pub(crate) epoch: u64,
}

/// The group a result stands for, under the grouping the search asked for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum GroupKey {
    Section(String, u64),
    Text(u64),
}

/// A line, by its book and its id.
pub(crate) type LineKey = (String, u64);

/// One search's results so far, in the order shown. `E` is a result, `S` what else the
/// engine keeps for the search.
pub(crate) struct SemanticSession<E, S> {
    /// The searcher every address in the session belongs to, and every expansion reads.
    pub(crate) searcher: Searcher,
    entries: Vec<E>,
    seen: HashSet<LineKey>,
    seen_groups: HashSet<GroupKey>,
    /// The candidate window the last expansion fused.
    pub(crate) window: u32,
    /// No wider window can add a result.
    pub(crate) exhausted: bool,
    pub(crate) state: S,
}

/// The results expansions found, appended to the session only once the search is served: a
/// search cancelled on the way leaves the session as it was.
pub(crate) struct Additions<E> {
    entries: Vec<E>,
    seen: HashSet<LineKey>,
    groups: HashSet<GroupKey>,
}

impl<E> Default for Additions<E> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            seen: HashSet::new(),
            groups: HashSet::new(),
        }
    }
}

impl<E> Additions<E> {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

impl<E, S> SemanticSession<E, S> {
    pub(crate) fn new(searcher: Searcher, state: S) -> Self {
        Self {
            searcher,
            entries: Vec::new(),
            seen: HashSet::new(),
            seen_groups: HashSet::new(),
            window: 0,
            exhausted: false,
            state,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    /// The results from `offset`, at most `limit` of them.
    pub(crate) fn page(&self, offset: u32, limit: u32) -> &[E] {
        let start = (offset as usize).min(self.entries.len());
        let end = start.saturating_add(limit as usize).min(self.entries.len());
        &self.entries[start..end]
    }

    /// Whether neither the session nor `additions` holds `line`, or the group it stands for.
    pub(crate) fn admits(
        &self,
        additions: &Additions<E>,
        line: &LineKey,
        group: Option<&GroupKey>,
    ) -> bool {
        let seen = |line: &LineKey| self.seen.contains(line) || additions.seen.contains(line);
        let grouped =
            |group: &GroupKey| self.seen_groups.contains(group) || additions.groups.contains(group);
        !seen(line) && !group.is_some_and(grouped)
    }

    /// Take `line`, the lines grouped under it, and its group as shown, with `entry` as its
    /// result; `None` for a line that will never be shown, such as a stale one.
    pub(crate) fn take<'a>(
        additions: &mut Additions<E>,
        line: LineKey,
        siblings: impl IntoIterator<Item = &'a LineKey>,
        group: Option<GroupKey>,
        entry: Option<E>,
    ) {
        additions.seen.insert(line);
        additions.seen.extend(siblings.into_iter().cloned());
        if let Some(entry) = entry {
            additions.groups.extend(group);
            additions.entries.push(entry);
        }
    }

    pub(crate) fn commit(&mut self, additions: Additions<E>) {
        self.entries.extend(additions.entries);
        self.seen.extend(additions.seen);
        self.seen_groups.extend(additions.groups);
    }
}

/// A kept session, with when a page was last asked of it.
type Kept<T> = (Instant, Arc<Mutex<T>>);

/// The sessions kept, by key, most recently used first.
pub(crate) struct SemanticSessions<T> {
    sessions: Mutex<LruCache<SessionKey, Kept<T>>>,
    epoch: AtomicU64,
    /// How many expansions searches ran.
    #[cfg(test)]
    pub(crate) expansions: AtomicU64,
}

impl<T> Default for SemanticSessions<T> {
    fn default() -> Self {
        Self {
            sessions: Mutex::new(LruCache::new(
                NonZeroUsize::new(SESSION_CAPACITY).expect("the cache holds sessions"),
            )),
            epoch: AtomicU64::new(0),
            #[cfg(test)]
            expansions: AtomicU64::new(0),
        }
    }
}

impl<T> SemanticSessions<T> {
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Forget every session: what they were computed from has changed. A search still
    /// running keeps its session under the old epoch, where nothing looks it up again.
    pub(crate) fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// The session kept under `key`, unless it has expired.
    pub(crate) fn get(&self, key: &SessionKey) -> Option<Arc<Mutex<T>>> {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let (used, session) = sessions.get_mut(key)?;
        if used.elapsed() > SESSION_TTL {
            sessions.pop(key);
            return None;
        }
        *used = Instant::now();
        Some(Arc::clone(session))
    }

    /// Keep `session` under `key`. Sessions of other index contents go: each holds a
    /// searcher, and with it segments the index has let go of.
    pub(crate) fn put(&self, key: SessionKey, session: Arc<Mutex<T>>) {
        let mut sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
        let old: Vec<SessionKey> = sessions
            .iter()
            .filter(|(kept, (used, _))| {
                kept.index_content != key.index_content || used.elapsed() > SESSION_TTL
            })
            .map(|(kept, _)| kept.clone())
            .collect();
        for kept in old {
            sessions.pop(&kept);
        }
        sessions.put(key, (Instant::now(), session));
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Invalidates `sessions` when dropped: at the end of a call that changes the semantic
/// session, whether it succeeded or not, so no search keeps a page computed before it.
pub(crate) struct InvalidateOnDrop<'a, T>(pub(crate) &'a SemanticSessions<T>);

impl<T> Drop for InvalidateOnDrop<'_, T> {
    fn drop(&mut self) {
        self.0.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::schema::Schema;
    use tantivy::Index;

    fn searcher() -> Searcher {
        let index = Index::create_in_ram(Schema::builder().build());
        index.reader().unwrap().searcher()
    }

    fn key(index_content: u64) -> SessionKey {
        SessionKey {
            query: "שבת".to_string(),
            facets: Vec::new(),
            options: String::new(),
            index_content,
            library_generation: (0, None, false),
            vectors_generation: None,
            epoch: 0,
        }
    }

    fn line(id: u64) -> LineKey {
        ("/book".to_string(), id)
    }

    type Session = SemanticSession<u64, ()>;

    /// A line, a line grouped under another and a group are each shown once, whichever
    /// expansion found them; nothing is appended before the commit.
    #[test]
    fn a_line_or_a_group_already_taken_is_not_taken_again() {
        let mut session = Session::new(searcher(), ());
        let mut first = Additions::default();
        let group = GroupKey::Section("/book".to_string(), 7);
        assert!(session.admits(&first, &line(1), Some(&group)));
        Session::take(
            &mut first,
            line(1),
            &[line(2)],
            Some(group.clone()),
            Some(1),
        );
        assert!(!session.admits(&first, &line(2), None), "a sibling shown");
        assert!(!session.admits(&first, &line(3), Some(&group)), "its group");
        Session::take(&mut first, line(4), &[], None, None);
        assert!(!session.admits(&first, &line(4), None), "a stale line");
        assert_eq!(session.len(), 0);
        session.commit(first);
        assert_eq!(session.page(0, 10), [1]);

        let second = Additions::default();
        assert!(!session.admits(&second, &line(1), None));
        assert!(!session.admits(&second, &line(5), Some(&group)));
        assert!(session.admits(&second, &line(5), None));
        assert_eq!(session.page(1, 10), [] as [u64; 0]);
        assert_eq!(session.page(5, 10), [] as [u64; 0]);
    }

    /// A session of other index contents is let go once one of the new contents is kept,
    /// and invalidation forgets them all and moves the epoch.
    #[test]
    fn another_generation_or_an_invalidation_lets_sessions_go() {
        let sessions = SemanticSessions::<u32>::default();
        sessions.put(key(1), Arc::new(Mutex::new(1)));
        let mut other_query = key(1);
        other_query.query = "תפילין".to_string();
        sessions.put(other_query.clone(), Arc::new(Mutex::new(2)));
        assert_eq!(sessions.len(), 2);
        assert!(sessions.get(&key(1)).is_some());

        sessions.put(key(2), Arc::new(Mutex::new(3)));
        assert_eq!(sessions.len(), 1);
        assert!(sessions.get(&key(1)).is_none());
        assert!(sessions.get(&other_query).is_none());

        let epoch = sessions.epoch();
        drop(InvalidateOnDrop(&sessions));
        assert_eq!(sessions.epoch(), epoch + 1);
        assert_eq!(sessions.len(), 0);
    }

    #[test]
    fn at_most_the_capacity_is_kept() {
        let sessions = SemanticSessions::<u32>::default();
        for n in 0..(SESSION_CAPACITY as u32 + 2) {
            let mut kept = key(1);
            kept.query = n.to_string();
            sessions.put(kept, Arc::new(Mutex::new(n)));
        }
        assert_eq!(sessions.len(), SESSION_CAPACITY);
    }
}
