//! Optional positional metadata for enumerators at internal PDF layout breaks.
//!
//! The raw text and its postings stay intact. Phrase gaps omit markers strictly between
//! selected words, which can themselves be markers. Metadata lives
//! after the ordinary anchor in the already multivalued `lineFirst` fast field. The
//! anchor (or 0 when absent) remains first, so older readers keep their edge semantics.

use std::ops::Range;

use tantivy::tokenizer::{TokenStream, Tokenizer};

use crate::hebrew_tokenizer::HebrewTokenizer;

/// Raw `lineEdge` term: no user token or `<word` / `>word` edge can collide with it.
pub(crate) const MARKER_PAGE: &str = "~pdf-layout-markers";

#[derive(Default)]
pub(crate) struct LayoutMarkers {
    pub(crate) positions: Vec<Range<u32>>,
}

impl LayoutMarkers {
    pub(crate) fn of(text: &str) -> Self {
        // Most pages have no bracketed enumerators: three cheap ASCII searches
        // avoid walking/tokenizing their layout lines at indexing and display time.
        if !text.contains('\n') || !(text.contains('(') || text.contains('[') || text.contains('{'))
        {
            return Self::default();
        }
        let mut edges = Vec::new();
        let mut start = 0;
        for line in text.split('\n') {
            let end = start + line.len();
            let content = crate::cross_line::content_range(line);
            // A marker-only line has no content edge to continue through.
            if content.start < content.end {
                if start > 0 && content.start > 0 {
                    edges.push(start..start + content.start);
                }
                if end < text.len() && content.end < line.len() {
                    edges.push(start + content.end..end);
                }
            }
            start = end + 1;
        }
        if edges.is_empty() {
            return Self::default();
        }
        let mut out = Self::default();
        let mut tokenizer = HebrewTokenizer::default();
        let mut stream = tokenizer.token_stream(text);
        let mut edge = 0;
        let mut position = None;
        let mut removable = true;
        while let Some(token) = stream.next() {
            if position != Some(token.position) {
                out.push_group(position, removable);
                position = Some(token.position);
                removable = true;
            }
            while edge < edges.len() && edges[edge].end <= token.offset_from {
                edge += 1;
            }
            removable &= edges.get(edge).is_some_and(|range| {
                token.offset_from >= range.start && token.offset_to <= range.end
            });
        }
        out.push_group(position, removable);
        out
    }

    fn push_group(&mut self, position: Option<usize>, removable: bool) {
        if removable {
            if let Some(position) = position.and_then(|p| u32::try_from(p).ok()) {
                // Internal markers always follow a token, and have content after them.
                // Both bounds fit a position stored by Tantivy.
                if position > 0 && position < u32::MAX {
                    match self.positions.last_mut() {
                        Some(last) if last.end == position => last.end += 1,
                        _ => self.positions.push(position..position + 1),
                    }
                }
            }
        }
    }

    pub(crate) fn encoded(&self) -> impl Iterator<Item = u64> + '_ {
        self.positions
            .iter()
            .flat_map(|range| [u64::from(range.start), u64::from(range.end)])
    }
}

/// Prefix sums make each position lookup O(log marker ranges).
#[derive(Default)]
pub(crate) struct LayoutMap {
    ranges: Vec<Range<u32>>,
    removed: Vec<u32>,
}

impl LayoutMap {
    pub(crate) fn from_markers(markers: &LayoutMarkers) -> Self {
        let mut map = Self::default();
        map.reset(markers.positions.iter().cloned());
        map
    }

    fn reset(&mut self, ranges: impl Iterator<Item = Range<u32>>) {
        self.ranges.clear();
        self.removed.clear();
        self.removed.push(0);
        for range in ranges {
            let removed = self.removed.last().copied().unwrap_or(0) + range.end - range.start;
            self.ranges.push(range);
            self.removed.push(removed);
        }
    }

    pub(crate) fn read(&mut self, values: impl Iterator<Item = u64>) {
        let mut values = values.skip(1);
        self.reset(
            std::iter::from_fn(|| Some((values.next()?, values.next()?))).filter_map(
                |(start, end)| {
                    (start > 0 && start < end && end <= u64::from(u32::MAX))
                        .then_some(start as u32..end as u32)
                },
            ),
        );
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub(crate) fn position(&self, raw: u32) -> LayoutPosition {
        let index = self.ranges.partition_point(|range| range.end <= raw);
        let removed = self.removed.get(index).copied().unwrap_or(0);
        let marker = self.ranges.get(index).filter(|range| range.contains(&raw));
        LayoutPosition {
            raw,
            content: marker.map_or(raw, |range| range.start) - removed,
            marker: marker.is_some(),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct LayoutPosition {
    pub(crate) raw: u32,
    pub(crate) content: u32,
    marker: bool,
}

impl LayoutPosition {
    /// For a < b, the gap is b.content - a.reach() - 1. Selected markers stay
    /// query words; only marker positions strictly between the endpoints are free.
    pub(crate) fn reach(self) -> u32 {
        self.content - u32::from(self.marker)
    }
}

/// Monotone ordered-phrase sweep. Raw positions retain strict order even when
/// content positions collapse: neither twins nor a marker can reuse one token.
#[derive(Default)]
pub(crate) struct LayoutSweep {
    feasible: Vec<LayoutPosition>,
    next: Vec<LayoutPosition>,
}

impl LayoutSweep {
    fn start(&mut self, first: &[u32], map: &LayoutMap) {
        self.feasible.clear();
        self.feasible
            .extend(first.iter().map(|&position| map.position(position)));
    }

    fn extend(&mut self, positions: &[u32], gap: u32, map: &LayoutMap) -> bool {
        self.next.clear();
        let mut cursor = 0;
        let step = u64::from(gap) + 1;
        for &raw in positions {
            let position = map.position(raw);
            while cursor < self.feasible.len()
                && u64::from(self.feasible[cursor].reach()) + step < u64::from(position.content)
            {
                cursor += 1;
            }
            if cursor < self.feasible.len() && self.feasible[cursor].raw < raw {
                self.next.push(position);
            }
        }
        std::mem::swap(&mut self.feasible, &mut self.next);
        !self.feasible.is_empty()
    }

    pub(crate) fn chain(&mut self, words: &[Vec<u32>], gaps: &[u32], map: &LayoutMap) -> bool {
        let Some((first, rest)) = words.split_first() else {
            return false;
        };
        self.start(first, map);
        !first.is_empty()
            && rest
                .iter()
                .zip(gaps)
                .all(|(word, &gap)| self.extend(word, gap, map))
    }

    pub(crate) fn end_slack(
        &mut self,
        words: &[Vec<u32>],
        gaps: &[u32],
        anchor: u32,
        max: u32,
        map: &LayoutMap,
    ) -> Option<u32> {
        if !self.chain(words, gaps, map) {
            return None;
        }
        let end = self.feasible.iter().rev().find(|end| end.raw <= anchor)?;
        let slack = map.position(anchor).content - end.reach();
        (slack <= max).then_some(slack)
    }

    pub(crate) fn start_slack(
        &mut self,
        words: &[Vec<u32>],
        gaps: &[u32],
        anchor: u32,
        max: u32,
        map: &LayoutMap,
    ) -> Option<u32> {
        let (first, rest) = words.split_first()?;
        let anchor = map.position(anchor);
        for &start in first.iter().filter(|&&start| start >= anchor.raw) {
            let slack = map.position(start).content - anchor.content;
            if slack > max {
                break;
            }
            self.start(&[start], map);
            if rest
                .iter()
                .zip(gaps)
                .all(|(word, &gap)| self.extend(word, gap, map))
            {
                return Some(slack);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn internal_markers_keep_outer_edges_and_round_trip() {
        let markers = LayoutMarkers::of("(א) שמע ישראל {פ}\n(ג) יהוה אלהינו (ד)");
        assert_eq!(markers.positions.len(), 1);
        assert_eq!(markers.positions[0], 3..5);
        let mut encoded = vec![2];
        encoded.extend(markers.encoded());
        let mut decoded = LayoutMap::default();
        decoded.read(encoded.into_iter());
        assert_eq!(decoded.ranges, markers.positions);
    }
    #[test]
    fn markers_do_not_include_readings_or_marker_only_lines() {
        for text in [
            "שמע ישראל\n(ג) [ד] יהוה אלהינו",
            "שמע ישראל\n(ג)\nיהוה אלהינו",
        ] {
            assert!(LayoutMarkers::of(text).positions.is_empty(), "{text}");
        }
    }
    #[test]
    fn marker_endpoints_keep_strict_order_and_real_words_in_the_gap() {
        let mut map = LayoutMap::default();
        map.reset([2..4, 7..8].into_iter());
        let mut sweep = LayoutSweep::default();
        assert!(sweep.chain(&[vec![1], vec![4]], &[0], &map));
        assert!(sweep.chain(&[vec![2], vec![3], vec![4]], &[0, 0], &map));
        assert!(!sweep.chain(&[vec![0], vec![2], vec![4]], &[0, 0], &map));
        assert!(!sweep.chain(&[vec![2], vec![2]], &[0], &map));
        assert_eq!(sweep.end_slack(&[vec![1]], &[], 4, 1, &map), Some(1));
        assert_eq!(sweep.end_slack(&[vec![2]], &[], 4, 1, &map), Some(1));
        assert_eq!(sweep.start_slack(&[vec![4]], &[], 1, 1, &map), Some(1));
    }

    #[test]
    fn layout_sweep_matches_direct_counting_between_raw_endpoints() {
        let mut seed = 19u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            (seed >> 32) as u32
        };
        for _ in 0..2000 {
            let marked: Vec<bool> = (0..16)
                .map(|p| p > 0 && p < 15 && next() % 3 == 0)
                .collect();
            let mut ranges: Vec<Range<u32>> = Vec::new();
            for (p, &marker) in marked.iter().enumerate() {
                if marker {
                    match ranges.last_mut() {
                        Some(range) if range.end == p as u32 => range.end += 1,
                        _ => ranges.push(p as u32..p as u32 + 1),
                    }
                }
            }
            let mut map = LayoutMap::default();
            map.reset(ranges.into_iter());
            let k = (next() % 4 + 2) as usize;
            let words: Vec<Vec<u32>> = (0..k)
                .map(|_| (0..16).filter(|_| next() % 3 == 0).collect())
                .collect();
            let gaps: Vec<u32> = (1..k)
                .map(|_| {
                    if next() % 20 == 0 {
                        u32::MAX
                    } else {
                        next() % 3
                    }
                })
                .collect();
            // Reference literally counts non-markers between raw endpoints, with
            // no projected coordinates or monotone-sweep shortcuts.
            let reference = |first: &[u32]| {
                let mut feasible = first.to_vec();
                for (positions, &gap) in words[1..].iter().zip(&gaps) {
                    feasible = positions
                        .iter()
                        .copied()
                        .filter(|&q| {
                            feasible.iter().any(|&p| {
                                p < q
                                    && (p + 1..q).filter(|&at| !marked[at as usize]).count() as u64
                                        <= u64::from(gap)
                            })
                        })
                        .collect();
                }
                feasible
            };
            let ends = reference(&words[0]);
            let mut sweep = LayoutSweep::default();
            assert_eq!(sweep.chain(&words, &gaps, &map), !ends.is_empty());
            let anchor = next() % 16;
            if marked[anchor as usize] {
                continue;
            }
            let max = next() % 5;
            let end = ends
                .iter()
                .filter(|&&p| p <= anchor)
                .map(|&p| (p + 1..=anchor).filter(|&at| !marked[at as usize]).count() as u32)
                .filter(|&slack| slack <= max)
                .min();
            assert_eq!(sweep.end_slack(&words, &gaps, anchor, max, &map), end);
            let start = words[0]
                .iter()
                .filter(|&&q| q >= anchor)
                .filter_map(|&q| {
                    let slack = (anchor..q).filter(|&at| !marked[at as usize]).count() as u32;
                    (slack <= max && !reference(&[q]).is_empty()).then_some(slack)
                })
                .min();
            assert_eq!(sweep.start_slack(&words, &gaps, anchor, max, &map), start);
        }
    }
}
