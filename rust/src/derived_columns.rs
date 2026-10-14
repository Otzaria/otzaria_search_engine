//! `sectionId` and `generationSort`, derived from `id` at query time.
//!
//! Both used to be full 64-bit columns, which compress badly: each value carries the
//! book's id base. The index keeps only what `id` does not already say — the section's
//! offset from its book's id base, and the book's generation — and these columns
//! recombine them, exactly, for any value the write API was given.

use tantivy::collector::sort_key::NaturalComparator;
use tantivy::collector::{SegmentSortKeyComputer, SortKeyComputer};
use tantivy::columnar::Column;
use tantivy::{DocId, Score, SegmentReader};

/// `sectionId - (id & BOOK_MASK)`, wrapping: a small number within a book.
pub(crate) const SECTION_OFFSET_FIELD: &str = "sectionOffset";
/// The book's generation order, capped at 255.
pub(crate) const GENERATION_FIELD: &str = "generation";

const BOOK_MASK: u64 = !0xFFFF_FFFF;
const GENERATION_SORT_SHIFT: u32 = 56;
const GENERATION_SORT_ID_MASK: u64 = (1u64 << GENERATION_SORT_SHIFT) - 1;

/// What the `sectionOffset` column stores for a document with `id` in `section_id`.
pub(crate) fn section_offset(id: u64, section_id: u64) -> u64 {
    section_id.wrapping_sub(id & BOOK_MASK)
}

/// What the `generation` column stores for a book of `generation_order`.
pub(crate) fn generation_value(generation_order: u32) -> u64 {
    u64::from(generation_order.min(255))
}

fn section_id(id: u64, offset: u64) -> u64 {
    (id & BOOK_MASK).wrapping_add(offset)
}

/// The catalogue key within one generation: generation first, then `id`.
pub(crate) fn generation_sort_key(generation: u64, id: u64) -> u64 {
    (generation.min(255) << GENERATION_SORT_SHIFT) | (id & GENERATION_SORT_ID_MASK)
}

/// The `sectionId` of each document of one segment.
#[derive(Clone)]
pub(crate) struct SectionIdColumn {
    id: Column<u64>,
    offset: Column<u64>,
}

impl SectionIdColumn {
    pub(crate) fn open(reader: &SegmentReader) -> tantivy::Result<Self> {
        let fast = reader.fast_fields();
        Ok(Self {
            id: fast.u64("id")?,
            offset: fast.u64(SECTION_OFFSET_FIELD)?,
        })
    }

    pub(crate) fn first(&self, doc: DocId) -> Option<u64> {
        Some(section_id(self.id.first(doc)?, self.offset.first(doc)?))
    }

    /// Every value the document holds: more than one only when it holds more than one
    /// `id` or offset, which a strict reader refuses.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    pub(crate) fn values_for_doc(&self, doc: DocId) -> impl Iterator<Item = u64> + '_ {
        let ids: Vec<u64> = self.id.values_for_doc(doc).collect();
        let offsets: Vec<u64> = self.offset.values_for_doc(doc).collect();
        let doubled = ids.len() > 1 || offsets.len() > 1;
        let first = ids
            .first()
            .zip(offsets.first())
            .map(|(&id, &offset)| section_id(id, offset));
        first.into_iter().chain(doubled.then_some(u64::MAX))
    }
}

/// The `generationSort` key of each document of one segment.
#[derive(Clone)]
pub(crate) struct GenerationSortColumn {
    id: Column<u64>,
    generation: Column<u64>,
}

impl GenerationSortColumn {
    pub(crate) fn open(reader: &SegmentReader) -> tantivy::Result<Self> {
        let fast = reader.fast_fields();
        Ok(Self {
            id: fast.u64("id")?,
            generation: fast.u64(GENERATION_FIELD)?,
        })
    }

    pub(crate) fn first(&self, doc: DocId) -> Option<u64> {
        Some(generation_sort_key(
            self.generation.first(doc)?,
            self.id.first(doc)?,
        ))
    }
}

/// Orders a `TopDocs` by the derived `generationSort`, as `order_by_fast_field` did by
/// the stored one.
pub(crate) struct GenerationSort;

impl SortKeyComputer for GenerationSort {
    type SortKey = Option<u64>;
    type Child = GenerationSortColumn;
    type Comparator = NaturalComparator;

    fn segment_sort_key_computer(&self, reader: &SegmentReader) -> tantivy::Result<Self::Child> {
        GenerationSortColumn::open(reader)
    }
}

impl SegmentSortKeyComputer for GenerationSortColumn {
    type SortKey = Option<u64>;
    type SegmentSortKey = Option<u64>;
    type SegmentComparator = NaturalComparator;

    fn segment_sort_key(&mut self, doc: DocId, _score: Score) -> Option<u64> {
        self.first(doc)
    }

    fn convert_segment_sort_key(&self, sort_key: Option<u64>) -> Option<u64> {
        sort_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_section_id_round_trips_through_its_offset() {
        for (id, section) in [
            ((5u64 << 32) + 7, (5u64 << 32) + 2),
            ((5 << 32) + 7, (5 << 32) + 7),
            (3, 99),
            ((9 << 32) + 1, 4),
            (u64::MAX, 0),
        ] {
            assert_eq!(section_id(id, section_offset(id, section)), section);
        }
        assert_eq!(section_offset((5 << 32) + 7, (5 << 32) + 2), 2);
    }

    #[test]
    fn generation_sort_key_orders_by_generation_then_id() {
        let early = generation_sort_key(generation_value(1), (9 << 32) + 1);
        let late = generation_sort_key(generation_value(2), (1 << 32) + 1);
        assert!(early < late);
        assert_eq!(
            generation_sort_key(generation_value(900), 1),
            (255u64 << 56) | 1
        );
    }
}
