//! A document as the engine reads it: what the doc store keeps, plus the fields it no
//! longer keeps (schema 6).
//!
//! The store holds `reference`, `id` and the stored texts. `title`, `isPdf` and `filePath`
//! are read from their columns, which hold one value per book and compress to almost
//! nothing; `segment` is stored only where `id` does not already say it.

use std::collections::HashMap;

use tantivy::columnar::{Column, StrColumn};
use tantivy::schema::{Field, Value};
use tantivy::{DocAddress, DocId, Searcher, TantivyDocument};

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
/// Several documents: [`DocumentLoader`], which opens each segment's columns once.
pub(crate) fn load_document(
    searcher: &Searcher,
    address: DocAddress,
) -> tantivy::Result<TantivyDocument> {
    DocumentLoader::new(searcher).load(address)
}

/// Loads documents like [`load_document`], with each segment's columns opened once:
/// opening a column reads its block index, which costs more than the document itself.
pub(crate) struct DocumentLoader<'a> {
    searcher: &'a Searcher,
    fields: Option<CompletedFields>,
    segments: Vec<Option<SegmentColumns>>,
}

#[derive(Clone, Copy)]
struct CompletedFields {
    title: Field,
    file_path: Field,
    is_pdf: Field,
    segment: Field,
    id: Field,
}

/// The columns a segment answers the unstored fields from; `None` when a field is not
/// a column there (an index whose store still holds it).
struct SegmentColumns {
    title: Option<MemoColumn>,
    file_path: Option<MemoColumn>,
    is_pdf: Option<Column<bool>>,
}

/// A string column whose values are memoized by term ordinal: a page of hits repeats a
/// few books, and decoding an ordinal reads a dictionary block.
struct MemoColumn {
    column: StrColumn,
    values: HashMap<u64, String>,
}

impl MemoColumn {
    fn values(&mut self, doc: DocId, mut each: impl FnMut(&str)) -> tantivy::Result<()> {
        for ord in self.column.term_ords(doc) {
            if let Some(value) = self.values.get(&ord) {
                each(value);
                continue;
            }
            let mut text = String::new();
            if self.column.ord_to_str(ord, &mut text)? {
                each(&text);
                self.values.insert(ord, text);
            }
        }
        Ok(())
    }
}

impl<'a> DocumentLoader<'a> {
    pub(crate) fn new(searcher: &'a Searcher) -> Self {
        Self {
            searcher,
            fields: None,
            segments: Vec::new(),
        }
    }

    pub(crate) fn load(&mut self, address: DocAddress) -> tantivy::Result<TantivyDocument> {
        let mut document: TantivyDocument = self.searcher.doc(address)?;
        let fields = match self.fields {
            Some(fields) => fields,
            None => {
                let schema = self.searcher.schema();
                let fields = CompletedFields {
                    title: schema.get_field("title")?,
                    file_path: schema.get_field("filePath")?,
                    is_pdf: schema.get_field("isPdf")?,
                    segment: schema.get_field("segment")?,
                    id: schema.get_field("id")?,
                };
                self.fields = Some(fields);
                fields
            }
        };
        let ordinal = address.segment_ord as usize;
        if self.segments.len() <= ordinal {
            self.segments.resize_with(ordinal + 1, || None);
        }
        if self.segments[ordinal].is_none() {
            let schema = self.searcher.schema();
            let reader = self.searcher.segment_reader(address.segment_ord);
            let str_column = |field: Field, name: &str| -> tantivy::Result<Option<MemoColumn>> {
                if !schema.get_field_entry(field).is_fast() {
                    return Ok(None);
                }
                Ok(reader.fast_fields().str(name)?.map(|column| MemoColumn {
                    column,
                    values: HashMap::new(),
                }))
            };
            self.segments[ordinal] = Some(SegmentColumns {
                title: str_column(fields.title, "title")?,
                file_path: str_column(fields.file_path, "filePath")?,
                is_pdf: if schema.get_field_entry(fields.is_pdf).is_fast() {
                    reader.fast_fields().column_opt::<bool>("isPdf")?
                } else {
                    None
                },
            });
        }
        let columns = self.segments[ordinal].as_mut().expect("opened above");
        complete_document(fields, columns, address.doc_id, &mut document)?;
        Ok(document)
    }
}

/// Adds to `document` the fields its columns and `id` answer. A field the document
/// already holds is left as it is.
fn complete_document(
    fields: CompletedFields,
    columns: &mut SegmentColumns,
    doc: DocId,
    document: &mut TantivyDocument,
) -> tantivy::Result<()> {
    let missing = |field: Field, document: &TantivyDocument| document.get_first(field).is_none();
    for (field, column) in [
        (fields.title, &mut columns.title),
        (fields.file_path, &mut columns.file_path),
    ] {
        if let Some(column) = column.as_mut().filter(|_| missing(field, document)) {
            column.values(doc, |text| document.add_text(field, text))?;
        }
    }
    if let Some(column) = columns
        .is_pdf
        .as_ref()
        .filter(|_| missing(fields.is_pdf, document))
    {
        for value in column.values_for_doc(doc) {
            document.add_bool(fields.is_pdf, value);
        }
    }
    if missing(fields.segment, document) {
        if let Some(id) = document
            .get_first(fields.id)
            .and_then(|value| value.as_u64())
        {
            document.add_u64(fields.segment, derived_segment(id));
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
