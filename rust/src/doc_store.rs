//! A document as the engine reads it: what the doc store keeps, plus the fields it no
//! longer keeps (schema 6).
//!
//! The store holds `reference`, `id` and the stored texts. `title`, `isPdf` and `filePath`
//! are read from their columns, which hold one value per book and compress to almost
//! nothing; `segment` is stored only where `id` does not already say it.

use tantivy::schema::{Field, Schema, Value};
use tantivy::{DocAddress, DocId, Searcher, SegmentReader, TantivyDocument};

const LINE_MASK: u64 = 0xFFFF_FFFF;

/// The `segment` a document with `id` has unless it stores another: its line ordinal,
/// as [`crate::api::search_engine::SearchEngine::add_text_book`] composes ids.
fn derived_segment(id: u64) -> u64 {
    (id & LINE_MASK).saturating_sub(1)
}

/// The `segment` to store for a document with `id`, `None` when it is the derived one.
pub(crate) fn segment_to_store(id: u64, segment: u64) -> Option<u64> {
    (segment != derived_segment(id)).then_some(segment)
}

/// The document at `address`, with every field a stored document of schema 5 carried.
pub(crate) fn load_document(
    searcher: &Searcher,
    address: DocAddress,
) -> tantivy::Result<TantivyDocument> {
    let mut document: TantivyDocument = searcher.doc(address)?;
    complete_document(
        searcher.schema(),
        searcher.segment_reader(address.segment_ord),
        address.doc_id,
        &mut document,
    )?;
    Ok(document)
}

/// Adds to `document` the fields its columns and `id` answer. A field the document
/// already holds is left as it is.
pub(crate) fn complete_document(
    schema: &Schema,
    reader: &SegmentReader,
    doc: DocId,
    document: &mut TantivyDocument,
) -> tantivy::Result<()> {
    let missing = |field: Field, document: &TantivyDocument| document.get_first(field).is_none();
    for name in ["title", "filePath"] {
        let field = schema.get_field(name)?;
        if !missing(field, document) || !schema.get_field_entry(field).is_fast() {
            continue;
        }
        if let Some(column) = reader.fast_fields().str(name)? {
            let mut text = String::new();
            for ord in column.term_ords(doc) {
                text.clear();
                if column.ord_to_str(ord, &mut text)? {
                    document.add_text(field, &text);
                }
            }
        }
    }
    let is_pdf = schema.get_field("isPdf")?;
    if missing(is_pdf, document) && schema.get_field_entry(is_pdf).is_fast() {
        if let Some(column) = reader.fast_fields().column_opt::<bool>("isPdf")? {
            for value in column.values_for_doc(doc) {
                document.add_bool(is_pdf, value);
            }
        }
    }
    let segment = schema.get_field("segment")?;
    if missing(segment, document) {
        let id = document
            .get_first(schema.get_field("id")?)
            .and_then(|value| value.as_u64());
        if let Some(id) = id {
            document.add_u64(segment, derived_segment(id));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_segment_the_id_does_not_say_is_stored() {
        let id = (7u64 << 32) + 12;
        assert_eq!(segment_to_store(id, 11), None);
        assert_eq!(segment_to_store(id, 3), Some(3));
        assert_eq!(segment_to_store(7 << 32, 0), None);
        assert_eq!(segment_to_store(5, 0), Some(0));
    }
}
