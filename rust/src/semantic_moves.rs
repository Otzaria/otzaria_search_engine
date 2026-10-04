//! What a filtered semantic search weighs besides the books it admits: the vectors of the
//! texts a live book holds that the vector set does not record in it.
//!
//! A filtered search scans only the vectors with a record in a book the filter admits —
//! the sidecar's book filter, which is what keeps it fast — and a set's records say where
//! its texts were when it was built. A text that has since moved into an admitted book, or
//! been copied into one, from a book the filter does not admit, has a vector whose records
//! name only the books it was in: the filtered scan never reaches it, although a live
//! admitted book holds it. A text a book holds twice is not one of these: the set records a
//! text once per book, and the resolver finds the book's other lines of it.
//!
//! So a filtered search is planned first ([`LiveResolver::plan`]). For every admitted book
//! its **arrivals** are read: the texts, by `chunkKey` column value, that its live lines
//! hold and that no live record of the set places in it, with the live slots the set holds
//! their vectors in — a text the set holds no live vector of is nothing to look for. They
//! are computed once per book and kept with the view of the set, under the book's postings
//! (the segments of the index that hold its lines, and how many of their documents are
//! deleted), which a commit that leaves the book alone leaves as they were, and under its
//! text hash, which a merge that moves its lines into another segment keeps. When no
//! admitted book has any, nothing moved, and the plan is the filtered scan exactly as
//! before. An arrival a live record of some admitted book reaches is scanned already. The
//! vectors of the others are **unreached**: the sidecar weighs them besides the scan, each
//! at its own score and none in place of one of the admitted books' hits
//! (`CandidateResolver::unreached`), and the resolver looks for each in the admitted books
//! it arrived in, held to its whole key, and in no other book. An unfiltered search plans
//! every book this way, with nothing unreached: the scan reads every vector.
//!
//! Liveness is the set's own. A record counts only when a scan of its book reaches a live
//! slot through it — the sidecar's `book_records`, which reads the generation's deletions
//! and links — and a vector only when its slot is live in the generation.
//!
//! The arrivals are column values, so a plan is made only for an index with the column;
//! without it — a version 4 index — a filtered search scans the admitted books alone, and
//! finds a text that moved into one once the vectors are updated.
//!
//! [`LiveResolver::plan`]: crate::semantic_resolver::LiveResolver::plan

use otzaria_semantic_search::cancellation::CancellationToken;
use otzaria_semantic_search::distribution::gates::book_records;
use otzaria_semantic_search::semantic::chunk_key::ChunkKey;
use otzaria_semantic_search::semantic::resolve::SlotRef;
use otzaria_semantic_search::semantic::segment_set::{self, SegmentSet};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use tantivy::index::SegmentId;

/// How many slots a pass over the set reads between two looks at the token.
const SLOTS_PER_CHECK: u32 = 65_536;

/// One generation of an installed vector set as a plan reads it, kept by the session for
/// as long as it serves that generation.
pub(crate) struct SetView {
    dir: PathBuf,
    set: SegmentSet,
    /// Each book's arrivals, and what they were computed for.
    arrivals: Mutex<HashMap<Arc<str>, Arrivals>>,
}

/// Where a book's lines are in the index: every segment that holds one, with how many
/// documents of that segment are deleted. The same postings are the same lines.
pub(crate) type Postings = Vec<(SegmentId, u32)>;

/// A text a book holds that no live record of the set places in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Arrival {
    /// The live slot of its vector.
    pub(crate) slot: SlotRef,
    /// The first line of the book that holds it: where the resolver looks for it first.
    pub(crate) ordinal: u32,
}

/// A book's arrivals, and the state of the book they are of.
struct Arrivals {
    postings: Postings,
    /// The book's `textHash` then, when its lines agree on one: the same text keys the same,
    /// so they hold for as long as it does. `0`, which a book without one has, is never
    /// matched.
    text_hash: u64,
    /// Its arrivals the set holds a live vector of, sorted.
    slots: Arc<[Arrival]>,
}

impl SetView {
    /// The set at `dir` as its live generation is, when that is `generation` — the one the
    /// session serves. `Ok(None)` when it is another, because an install or a compaction
    /// moved it on, and when nothing is installed; what is installed is read from its small
    /// files first, so a set that moved on is not opened. Opened beside the session, without
    /// the set's lock and without recovery or garbage collection
    /// (`SegmentSet::open_without_recovery`): a search plans on its own thread, and must
    /// neither make an install wait nor clean up the set. A generation an install collects
    /// while it is opened fails the open, and the next filtered search opens again.
    pub(crate) fn open(dir: &Path, generation: u64) -> Result<Option<Self>, String> {
        match segment_set::info(dir).map_err(|err| err.to_string())? {
            Some(info) if info.generation == generation => {}
            _ => return Ok(None),
        }
        let set = SegmentSet::open_without_recovery(dir).map_err(|err| err.to_string())?;
        if set.generation() != generation {
            return Ok(None);
        }
        Ok(Some(Self {
            dir: dir.to_path_buf(),
            set,
            arrivals: Mutex::default(),
        }))
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn generation(&self) -> u64 {
        self.set.generation()
    }

    /// Every key a scan admitting `book` reaches through the book's records — its live slots,
    /// the live slots its extras name, and those its foreign records resolve to — into
    /// `out`, with the record's hint, sorted.
    pub(crate) fn recorded(&self, book: &str, out: &mut Vec<(ChunkKey, u32)>) {
        book_records(&self.set, book, out);
    }

    /// Of `slots` — live slots that hold arrivals' keys — the keys a filtered scan of the
    /// books `admitted` says yes to reaches already: as the sidecar's book filter visits a
    /// slot, through the primary record or an extra of an admitted book, or a foreign record
    /// of one that resolves to it. Read from the slots' own records, so the cost is the
    /// arrivals', not the admitted books': the primary record and the extras from the slot,
    /// and a foreign record — whether its link resolves is the generation's — by the
    /// sidecar's count of the few admitted books that hold one of the key.
    pub(crate) fn reached(
        &self,
        slots: &[SlotRef],
        admitted: &dyn Fn(&str) -> bool,
    ) -> HashSet<ChunkKey> {
        let segments = self.set.segments();
        let mut reached = HashSet::new();
        let mut unsure = HashSet::new();
        for slot in slots {
            let Some(segment) = segments.get(slot.seg as usize) else {
                continue;
            };
            let books = segment.books();
            let through_extra = || {
                segment.has_extras(slot.slot)
                    && segment
                        .extras_of_slot(slot.slot)
                        .any(|extra| admitted(&books[segment.book_of_extra(extra)].name))
            };
            if admitted(&books[segment.book_of_slot(slot.slot)].name) || through_extra() {
                reached.insert(slot.key);
            } else {
                unsure.insert(slot.key);
            }
        }
        unsure.retain(|key| !reached.contains(key));
        if unsure.is_empty() {
            return reached;
        }
        let mut holders: Vec<Arc<str>> = Vec::new();
        for segment in segments {
            for entry in segment.books() {
                if entry.foreign.is_empty() || !admitted(&entry.name) {
                    continue;
                }
                if entry
                    .foreign
                    .clone()
                    .any(|index| unsure.contains(&segment.foreign_record(index).0))
                {
                    holders.push(Arc::clone(&entry.name));
                }
            }
        }
        holders.sort();
        holders.dedup();
        let mut records = Vec::new();
        for book in holders {
            book_records(&self.set, &book, &mut records);
            reached.extend(
                records
                    .iter()
                    .map(|(key, _)| *key)
                    .filter(|key| unsure.contains(key)),
            );
        }
        reached
    }

    /// The live slots that hold a key of each of `values`, sorted: one pass over every slot
    /// of the set, looking at `cancel` as it goes. A value no live slot holds is absent.
    pub(crate) fn live_slots(
        &self,
        values: &HashSet<u64>,
        cancel: &CancellationToken,
    ) -> Option<HashMap<u64, Vec<SlotRef>>> {
        let segments = self.set.segments();
        // In stretches of slots read in parallel, and gathered in order.
        let stretches: Vec<(u16, u32)> = segments
            .iter()
            .enumerate()
            .flat_map(|(seg, segment)| {
                (0..segment.slot_count())
                    .step_by(SLOTS_PER_CHECK as usize)
                    .map(move |start| (seg as u16, start))
            })
            .collect();
        let held: Vec<Vec<SlotRef>> = stretches
            .into_par_iter()
            .map(|(seg, start)| {
                if cancel.is_cancelled() {
                    return None;
                }
                let segment = &segments[seg as usize];
                let end = start
                    .saturating_add(SLOTS_PER_CHECK)
                    .min(segment.slot_count());
                Some(
                    (start..end)
                        .filter_map(|slot| {
                            let key = segment.key(slot);
                            (values.contains(&key.column_value()) && self.set.is_live(seg, slot))
                                .then_some(SlotRef { seg, slot, key })
                        })
                        .collect(),
                )
            })
            .collect::<Option<_>>()?;
        let mut found: HashMap<u64, Vec<SlotRef>> = HashMap::new();
        for slot in held.into_iter().flatten() {
            found.entry(slot.key.column_value()).or_default().push(slot);
        }
        Some(found)
    }

    /// `book`'s arrivals as computed before, when its postings are as they were then.
    pub(crate) fn known_arrivals(&self, book: &str, postings: &Postings) -> Option<Arc<[Arrival]>> {
        let arrivals = self.arrivals.lock().unwrap_or_else(PoisonError::into_inner);
        let known = arrivals.get(book)?;
        (known.postings == *postings).then(|| Arc::clone(&known.slots))
    }

    /// `book`'s arrivals as computed before for the same text, when it has one: kept, under
    /// the postings it has now.
    pub(crate) fn known_arrivals_of_text(
        &self,
        book: &str,
        text_hash: u64,
        postings: &Postings,
    ) -> Option<Arc<[Arrival]>> {
        if text_hash == 0 {
            return None;
        }
        let mut arrivals = self.arrivals.lock().unwrap_or_else(PoisonError::into_inner);
        let known = arrivals.get_mut(book)?;
        if known.text_hash != text_hash {
            return None;
        }
        known.postings.clone_from(postings);
        Some(Arc::clone(&known.slots))
    }

    pub(crate) fn remember_arrivals(
        &self,
        book: Arc<str>,
        postings: Postings,
        text_hash: u64,
        slots: Arc<[Arrival]>,
    ) {
        self.arrivals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                book,
                Arrivals {
                    postings,
                    text_hash,
                    slots,
                },
            );
    }
}
