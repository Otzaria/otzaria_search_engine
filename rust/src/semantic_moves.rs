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
//! every book, with nothing unreached.
//!
//! A short line that shares a neighbour's vector has no slot: its own key is recorded in its
//! book through its alias, and where it arrives it brings the vector it shares.
//!
//! Every key here is read from the set and compared with the set's own, so a set of 8-byte
//! keys (segment format 3) is compared at that width throughout, with no narrowing.
//!
//! Liveness is the set's own. A record counts only when a scan of its book reaches a live
//! slot through it — the sidecar's `book_records`, which reads the generation's deletions
//! and links — and a vector only when its slot is live in the generation.
//!
//! The arrivals are column values, so a plan is made only for an index with the column;
//! without one this build uses, a filtered search scans the admitted books alone, and
//! finds a text that moved into one once the vectors are updated.
//!
//! [`LiveResolver::plan`]: crate::semantic_resolver::LiveResolver::plan

use otzaria_semantic_search::cancellation::CancellationToken;
use otzaria_semantic_search::distribution::gates::book_records;
use otzaria_semantic_search::semantic::chunk_key::ChunkKey;
use otzaria_semantic_search::semantic::oxv::occurrence::ALIAS_WINDOW;
use otzaria_semantic_search::semantic::oxv::reader::Segment;
use otzaria_semantic_search::semantic::resolve::SlotRef;
use otzaria_semantic_search::semantic::segment_set::{self, SegmentSet};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tantivy::index::SegmentId;

/// How many slots a pass over the set reads between two looks at the token.
const SLOTS_PER_CHECK: u32 = 65_536;

/// One generation of an installed vector set as a plan reads it, kept by the session for
/// as long as it serves that generation.
pub(crate) struct SetView {
    /// Unique to this open: a plan made with another view is not this one's.
    id: u64,
    dir: PathBuf,
    set: SegmentSet,
    /// Each book's arrivals, and what they were computed for.
    arrivals: Mutex<HashMap<Arc<str>, Arrivals>>,
    /// Reverse alias lookup, built once for this immutable set generation.
    aliases: Mutex<Option<AliasMappings>>,
}

type AliasMappings = Arc<[(u64, ChunkKey)]>;

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

/// What a view knows of a book.
#[derive(Clone)]
pub(crate) enum Known {
    /// Its arrivals.
    Arrivals(Arc<[Arrival]>),
    /// The texts it holds that the set does not record in it, by value and first line, not
    /// yet looked up in the set.
    Unlooked(Arc<[(u64, u32)]>),
}

/// What a view knows of a book, and the state of the book it is of.
struct Arrivals {
    postings: Postings,
    /// The book's `textHash` then, when its lines agree on one: the same text keys the same,
    /// so they hold for as long as it does. `0`, which a book without one has, is never
    /// matched.
    text_hash: u64,
    known: Known,
}

impl SetView {
    /// The set at `dir` as its live generation is, when that is `generation` — the one the
    /// session serves. `Ok(None)` when it is another, because an install or a compaction
    /// moved it on, and when nothing is installed; what is installed is read from its small
    /// files first, so a set that moved on is not opened. Opened beside the session, without
    /// the set's lock and without recovery or garbage collection
    /// (`SegmentSet::open_without_recovery`): a search plans on its own thread, and must
    /// neither make an install wait nor clean up the set. A generation an install collects
    /// while it is opened fails the open, and the next search opens again.
    pub(crate) fn open(dir: &Path, generation: u64) -> Result<Option<Self>, String> {
        match segment_set::info(dir).map_err(|err| err.to_string())? {
            Some(info) if info.generation == generation => {}
            _ => return Ok(None),
        }
        let set = SegmentSet::open_without_recovery(dir).map_err(|err| err.to_string())?;
        if set.generation() != generation {
            return Ok(None);
        }
        static OPENED: AtomicU64 = AtomicU64::new(0);
        Ok(Some(Self {
            id: OPENED.fetch_add(1, Ordering::Relaxed),
            dir: dir.to_path_buf(),
            set,
            arrivals: Mutex::default(),
            aliases: Mutex::default(),
        }))
    }

    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn generation(&self) -> u64 {
        self.set.generation()
    }

    pub(crate) fn book_source_hash(&self, book: &str) -> Option<u64> {
        self.set.book_source_hash(book)
    }

    pub(crate) fn has_windows(&self) -> bool {
        self.set.identity().model.chunking_identity
            == crate::semantic_keys::production_chunking().identity()
    }

    /// Every key a scan admitting `book` reaches through the book's records — its live slots,
    /// the live slots its extras name, and those its foreign records resolve to — into
    /// `out`, with the record's hint, sorted; and the own key of each short line of the book
    /// that shares a neighbour's vector, which is recorded there although no slot holds it.
    pub(crate) fn recorded(&self, book: &str, out: &mut Vec<(ChunkKey, u32)>) {
        let mut records = Vec::new();
        self.set.book_occurrences(book, &mut records);
        out.clear();
        for (key, record) in records {
            out.push((key, record.hint));
            if let Some(anchor) = record.anchor.filter(|_| record.window == ALIAS_WINDOW) {
                out.push((anchor, record.hint));
            }
        }
        out.sort_unstable();
        out.dedup();
    }

    /// Of `values`, short lines' own keys by column value, the vectors they share: the key
    /// of each alias occurrence of the set whose anchor is one of them, by that value.
    pub(crate) fn alias_targets(
        &self,
        values: &HashSet<u64>,
        cancel: &CancellationToken,
    ) -> Option<Vec<(u64, ChunkKey)>> {
        if cancel.is_cancelled() {
            return None;
        }
        let mut cached = self.aliases.lock().unwrap_or_else(PoisonError::into_inner);
        if cached.is_none() {
            *cached = Some(collect_aliases(self.set.segments(), cancel)?.into());
        }
        let aliases = Arc::clone(cached.as_ref().expect("initialized above"));
        drop(cached);
        let mut targets = Vec::new();
        for (at, &value) in values.iter().enumerate() {
            if at.is_multiple_of(4096) && cancel.is_cancelled() {
                return None;
            }
            let start = aliases.partition_point(|&(anchor, _)| anchor < value);
            targets.extend(
                aliases[start..]
                    .iter()
                    .take_while(|&&(anchor, _)| anchor == value)
                    .copied(),
            );
        }
        targets.sort_unstable();
        Some(targets)
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
            let snapshot = segments.iter().rev().find_map(|segment| {
                segment.key_occurrences(slot.key).map(|mut records| {
                    records.any(|record| admitted(&segment.books()[record.book as usize].name))
                })
            });
            if let Some(admitted) = snapshot {
                if admitted {
                    reached.insert(slot.key);
                }
                continue;
            }
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

    /// The live slots that hold a key of one of `values`, in slot order: one pass over every
    /// slot of the set, looking at `cancel` as it goes.
    pub(crate) fn live_slots(
        &self,
        values: &HashSet<u64>,
        cancel: &CancellationToken,
    ) -> Option<Vec<SlotRef>> {
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
        Some(held.into_iter().flatten().collect())
    }

    /// What is known of `book` when its postings are as they were then, with its text hash.
    pub(crate) fn known_arrivals(&self, book: &str, postings: &Postings) -> Option<(u64, Known)> {
        let arrivals = self.arrivals.lock().unwrap_or_else(PoisonError::into_inner);
        let known = arrivals.get(book)?;
        (known.postings == *postings).then(|| (known.text_hash, known.known.clone()))
    }

    /// `book`'s arrivals as computed before for the same text, when it has one: kept, under
    /// the postings it has now.
    pub(crate) fn known_arrivals_of_text(
        &self,
        book: &str,
        text_hash: u64,
        postings: &Postings,
    ) -> Option<Known> {
        if text_hash == 0 {
            return None;
        }
        let mut arrivals = self.arrivals.lock().unwrap_or_else(PoisonError::into_inner);
        let known = arrivals.get_mut(book)?;
        if known.text_hash != text_hash {
            return None;
        }
        known.postings.clone_from(postings);
        Some(known.known.clone())
    }

    pub(crate) fn remember_arrivals(
        &self,
        book: Arc<str>,
        postings: Postings,
        text_hash: u64,
        known: Known,
    ) {
        self.arrivals
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                book,
                Arrivals {
                    postings,
                    text_hash,
                    known,
                },
            );
    }
}

/// Sorted reverse lookup of the generation's effective alias snapshots. Dead targets
/// are excluded by `live_slots` when the caller looks up the returned vector keys.
fn collect_aliases(
    segments: &[Segment],
    cancel: &CancellationToken,
) -> Option<Vec<(u64, ChunkKey)>> {
    // Track only keys that have ever had an alias, rather than every vector key.
    // This bounds the temporary index by the alias population and avoids probing
    // every later segment for each alias when there are many deltas.
    let mut pending = HashSet::new();
    for segment in segments {
        for (at, row) in segment.occurrences().enumerate() {
            if at.is_multiple_of(4096) && cancel.is_cancelled() {
                return None;
            }
            if row.is_alias() {
                pending.insert(row.key);
            }
        }
    }
    let mut aliases = Vec::new();
    for segment in segments.iter().rev() {
        if pending.is_empty() {
            break;
        }
        let mut previous = None;
        let mut keep = false;
        for (at, row) in segment.occurrences().enumerate() {
            if at.is_multiple_of(4096) && cancel.is_cancelled() {
                return None;
            }
            // Occurrences are sorted by key. A key's newest group is its complete
            // snapshot, even when it contains no aliases any more.
            if previous != Some(row.key) {
                if pending.is_empty() {
                    break;
                }
                previous = Some(row.key);
                keep = pending.remove(&row.key);
            }
            if keep && row.is_alias() {
                aliases.push((row.anchor.column_value(), row.key));
            }
        }
    }
    aliases.sort_unstable();
    aliases.dedup();
    (!cancel.is_cancelled()).then_some(aliases)
}

#[cfg(test)]
mod tests {
    use super::*;
    use otzaria_semantic_search::distribution::package::PackageKind;
    use otzaria_semantic_search::semantic::oxv::codec::Codec;
    use otzaria_semantic_search::semantic::oxv::occurrence::Occurrence;
    use otzaria_semantic_search::semantic::oxv::writer::{SegmentBuilder, SegmentSpec};

    #[test]
    fn reverse_alias_lookup_uses_only_the_latest_snapshot() {
        let dir = tempfile::TempDir::new().unwrap();
        let key = |value| ChunkKey::of(value).narrowed();
        let target = key("shared vector");
        let old = key("old short line");
        let fresh = key("new short line");
        let fresh_too = key("another new short line");
        let other = key("another vector");
        let write = |version, rows: &[(ChunkKey, ChunkKey, u8)]| {
            let path = dir.path().join(format!("{version}.oxv"));
            let mut builder = SegmentBuilder::new(
                SegmentSpec {
                    kind: PackageKind::Base,
                    identity_digest: [1; 32],
                    from_library_version: 0,
                    to_library_version: version,
                    library_release_tag: format!("v{version}"),
                },
                Codec::i8_sym_vec(2).unwrap(),
            );
            builder.set_format_version(3).unwrap();
            builder
                .add_book("book", &[(target, 0), (other, 1)], &[], &[])
                .unwrap();
            let rows: Vec<_> = rows
                .iter()
                .map(|&(key, anchor, window)| Occurrence {
                    key,
                    book: 0,
                    hint: 0,
                    anchor,
                    window,
                })
                .collect();
            builder.add_occurrences("book", &rows).unwrap();
            let mut sink = builder.write(&path).unwrap();
            sink.push_f32(&[1.0, 0.0]).unwrap();
            sink.push_f32(&[0.0, 1.0]).unwrap();
            sink.finish().unwrap();
            Segment::open(&path).unwrap()
        };
        let segments = [
            write(
                1,
                &[(target, old, ALIAS_WINDOW), (other, old, ALIAS_WINDOW)],
            ),
            write(
                2,
                &[
                    (target, fresh, ALIAS_WINDOW),
                    (target, fresh_too, ALIAS_WINDOW),
                ],
            ),
        ];
        let cancel = CancellationToken::new();
        let mut expected = vec![
            (old.column_value(), other),
            (fresh.column_value(), target),
            (fresh_too.column_value(), target),
        ];
        expected.sort_unstable();
        assert_eq!(collect_aliases(&segments, &cancel).unwrap(), expected);
        // Replacing the alias with a normal occurrence withdraws the old mapping.
        let normal = write(3, &[(target, target, 0)]);
        assert_eq!(
            collect_aliases(
                &[Segment::open(&dir.path().join("2.oxv")).unwrap(), normal],
                &cancel
            )
            .unwrap(),
            []
        );
        cancel.cancel();
        assert!(collect_aliases(&segments, &cancel).is_none());
    }
}
