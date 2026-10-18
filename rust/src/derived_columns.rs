//! `sectionId`, derived from `id` at query time.
//!
//! It used to be a full 64-bit column, which compresses badly: each value carries the
//! book's id base. The index keeps only what `id` does not already say — the section's
//! offset from its book's id base — and this column recombines them, exactly, for any
//! value the write API was given.

use tantivy::columnar::Column;
use tantivy::{DocId, SegmentReader};

/// `sectionId - (id & BOOK_MASK)`, wrapping: a small number within a book.
pub(crate) const SECTION_OFFSET_FIELD: &str = "sectionOffset";

const BOOK_MASK: u64 = !0xFFFF_FFFF;

/// What the `sectionOffset` column stores for a document with `id` in `section_id`.
pub(crate) fn section_offset(id: u64, section_id: u64) -> u64 {
    section_id.wrapping_sub(id & BOOK_MASK)
}

fn section_id(id: u64, offset: u64) -> u64 {
    (id & BOOK_MASK).wrapping_add(offset)
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
}
