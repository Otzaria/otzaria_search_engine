//! The chunk key the index stores for each line, and the recipe it is computed under.
//!
//! A semantic vector is stored under the key of the text it was embedded from — the
//! sidecar's [`ChunkKey`](otzaria_semantic_search::semantic::chunk_key::ChunkKey) — and
//! never under a position, so a line keeps its vector across a library update for as long
//! as the text the recipe builds for it is unchanged. The index records that key for every
//! line in its `chunkKey` column, computed as the line is indexed, which is what ties a
//! vector to the lines that hold its text today without recomputing anything.
//!
//! The key is a function of three things, and an index's metadata records all three beside
//! the column ([`ChunkKeyRecipe`]): the line recipe that turned the book into the text the
//! index stores ([`LINE_TEXT_VERSION`]), the key function
//! ([`KEY_VERSION`]), and the chunking the library's vectors are built with
//! ([`production_chunking`]). A column written under any other recipe counts as absent.
//!
//! Where an index has no column this build uses, the key of a line is computed again from
//! what the index stores, by [`recompute_chunk_key`]; and that is also how a result is checked
//! against the vector it came from before it is shown.
//!
//! At the crate root, not under `api`, so flutter_rust_bridge generates no bindings for it.
//! Compiled into every build, as the sidecar is: the release index is built by one build and
//! opened by all of them. Public on the Rust side alone, for the tools and tests that hold
//! the recipe to the one a model publishes.

use crate::api::search_engine::LINE_TEXT_VERSION;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use otzaria_semantic_search::semantic::chunk_key::{ChunkKey, LineRef, KEY_VERSION};
use otzaria_semantic_search::semantic::chunker::{Chunker, ChunkerConfig};
use rayon::prelude::*;
use tantivy::schema::Value;
use tantivy::{DocAddress, Searcher, TantivyDocument};

/// The chunking the library's vectors are built with: Meivin Round 2's, the sidecar's
/// `config/models/meivin-round2-onnx/chunking.json`.
///
/// Compiled in, because indexing runs with no model configured: every build computes a
/// line's key, whether or not it can embed. The model family the vectors are published
/// under names this configuration by its hash, [`ChunkerConfig::identity`], so a copy that
/// drifted from the file would be another `chunking_identity`, and the tests pin it to the
/// published one.
pub fn production_chunking() -> ChunkerConfig {
    ChunkerConfig {
        min_meaningful_chars: 20,
        context_window_lines: 2,
        max_chunk_chars: 512,
        min_embeddable_chars: 5,
        chunking_version: 1,
        embedding_text_version: 2,
        normalization_version: 1,
    }
}

/// [`production_chunking`], resolved once to the code that applies it.
static PRODUCTION_CHUNKER: Lazy<Chunker> = Lazy::new(|| {
    Chunker::new(production_chunking())
        .expect("the production chunking is one the pinned sidecar implements")
});

/// The `chunkKey` column value of every line of a book, in order: the line's
/// [`ChunkKey::column_value`] when the production recipe embeds it, and `0` when it does not.
///
/// `lines` is the whole book as the index stores it — each line's text and the section it
/// belongs to — since a short line borrows its neighbours' text. The answers are the
/// sidecar's `Chunker::chunk_keys`, computed in parallel: a line's text comes from its
/// window alone, and the SHA-256 over it is most of the cost.
pub fn column_values(lines: &[LineRef<'_>]) -> Vec<u64> {
    let chunker = &*PRODUCTION_CHUNKER;
    (0..lines.len())
        .into_par_iter()
        .map(|index| {
            chunker
                .embedded_text(lines, index)
                .map_or(0, |text| ChunkKey::of(&text).column_value())
        })
        .collect()
}

/// The key of the line at `ordinal` of a book, computed again from what the index stores:
/// the stored `text` of the line and of its neighbours within the recipe's context window,
/// and the `sectionId` each belongs to. `None` when the production recipe does not embed the
/// line, and for a PDF's line, which no vector is built from.
///
/// `book` is the book's documents in line order: the whole book, or any stretch of it that
/// holds the line and its neighbours on each side within the window — fewer only where the
/// book ends. Nothing further out is read, so a check costs at most five documents.
///
/// For a line `add_text_book` added, under this build's recipe, this is the key whose
/// [`ChunkKey::column_value`] the `chunkKey` column holds (`0` for `None`). An index with no
/// column this build uses is asked this instead; and since this is the whole 128-bit key, it
/// is also what a result is checked against before it is shown.
pub fn recompute_chunk_key(
    searcher: &Searcher,
    book: &[DocAddress],
    ordinal: usize,
) -> Result<Option<ChunkKey>> {
    let mut keys = recompute_chunk_keys(searcher, book, ordinal..ordinal + 1)?;
    Ok(keys.pop().flatten())
}

/// [`recompute_chunk_key`] for every line of `lines`, a range of positions in `book`,
/// reading the stretch they and their windows span once: what re-anchoring a hit around
/// its hint, or checking a page of results from one book, asks.
pub fn recompute_chunk_keys(
    searcher: &Searcher,
    book: &[DocAddress],
    lines: std::ops::Range<usize>,
) -> Result<Vec<Option<ChunkKey>>> {
    anyhow::ensure!(
        lines.start < lines.end && lines.end <= book.len(),
        "lines {lines:?} are not in a book of {} line(s)",
        book.len()
    );
    let schema = searcher.schema();
    let text_field = schema.get_field("text")?;
    let is_pdf_field = schema.get_field("isPdf")?;

    let reach = production_chunking().context_window_lines;
    let start = lines.start.saturating_sub(reach);
    let end = (lines.end - 1).saturating_add(reach).min(book.len() - 1);
    let mut texts = Vec::with_capacity(end - start + 1);
    let mut sections = Vec::with_capacity(end - start + 1);
    let mut pdf = Vec::with_capacity(end - start + 1);
    for &address in &book[start..=end] {
        let document: TantivyDocument = searcher
            .doc(address)
            .with_context(|| format!("reading the document at {address:?}"))?;
        pdf.push(
            document
                .get_first(is_pdf_field)
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        );
        texts.push(
            document
                .get_first(text_field)
                .and_then(|value| value.as_str())
                .with_context(|| format!("the document at {address:?} stores no text"))?
                .to_string(),
        );
        // FAST and not stored, so read from its column.
        sections.push(
            searcher
                .segment_reader(address.segment_ord)
                .fast_fields()
                .u64("sectionId")?
                .first(address.doc_id)
                .with_context(|| format!("the document at {address:?} has no sectionId"))?,
        );
    }
    let window: Vec<LineRef<'_>> = texts
        .iter()
        .zip(&sections)
        .map(|(text, &section)| LineRef { text, section })
        .collect();
    Ok(lines
        .map(|line| {
            let index = line - start;
            if pdf[index] {
                return None;
            }
            PRODUCTION_CHUNKER
                .embedded_text(&window, index)
                .map(|text| ChunkKey::of(&text))
        })
        .collect())
}

/// What a `chunkKey` column was computed under, as an index's metadata records it.
///
/// The column is used only when this equals [`ChunkKeyRecipe::current`]. Any other recipe —
/// a line recipe, key function or chunking this build does not compute — makes it count as
/// absent, and the keys are then computed from the stored text instead. Never a reason to
/// rebuild an index: what an index cannot answer from its column, its text still can.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ChunkKeyRecipe {
    /// [`LINE_TEXT_VERSION`] of the build that wrote the column.
    pub(crate) line_text_version: u32,
    /// The sidecar's [`KEY_VERSION`].
    pub(crate) key_version: u32,
    /// [`ChunkerConfig::identity`] of the chunking the keys were computed with.
    pub(crate) chunking_identity: u64,
}

impl ChunkKeyRecipe {
    /// The recipe this build computes keys under.
    pub(crate) fn current() -> Self {
        Self {
            line_text_version: LINE_TEXT_VERSION,
            key_version: KEY_VERSION,
            chunking_identity: production_chunking().identity(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::search_engine::SearchEngine;
    use tantivy::Index;
    use tempfile::TempDir;

    /// `chunking_identity` in the model family Meivin Round 2's vectors are published under,
    /// the sidecar's `config/models/meivin-round2-onnx/model.json` at the pinned revision.
    const PUBLISHED_CHUNKING_IDENTITY: u64 = 2_685_558_872_390_372_738;

    #[test]
    fn the_compiled_in_chunking_is_the_one_meivin_round_2_publishes() {
        assert_eq!(
            production_chunking().identity(),
            PUBLISHED_CHUNKING_IDENTITY
        );
        assert_eq!(
            ChunkKeyRecipe::current(),
            ChunkKeyRecipe {
                line_text_version: 1,
                key_version: 1,
                chunking_identity: PUBLISHED_CHUNKING_IDENTITY,
            }
        );
    }

    /// A synthetic book, raw as `add_text_book` receives it, with a line on every branch of
    /// the recipe: a heading that opens a section and is short, so it borrows the lines
    /// after it; a line that stands alone; one under five characters, skipped but still
    /// context; one whose tags and niqqud normalization removes; a blank one, which context
    /// joins as an empty string; a section edge context stops at; and a line past the
    /// 512-character cap, whose 512th character is a space recipe 2 drops.
    fn book() -> String {
        [
            "<h2>פרק ראשון</h2>".to_string(),
            "שורה ארוכה דיה כדי לעמוד בפני עצמה".to_string(),
            "קצרה".to_string(),
            "<b>שורה</b> קצרה".to_string(),
            String::new(),
            "שָׁלוֹם עֲלֵיכֶם".to_string(),
            "<h2>פרק שני</h2>".to_string(),
            "abcdefg ".repeat(80),
            "סוף".to_string(),
        ]
        .join("\n")
    }

    /// What `book()` stores, line by line: the text every key is computed from.
    fn stored() -> Vec<String> {
        vec![
            "פרק ראשון".to_string(),
            "שורה ארוכה דיה כדי לעמוד בפני עצמה".to_string(),
            "קצרה".to_string(),
            "שורה קצרה".to_string(),
            String::new(),
            "שלום עליכם".to_string(),
            "פרק שני".to_string(),
            // The raw line's last space is trimmed: 639 characters.
            ["abcdefg"; 80].join(" "),
            "סוף".to_string(),
        ]
    }

    /// The column `book()` must get, computed outside this crate with Python's hashlib alone,
    /// so neither the recipe nor SHA-256 can drift with the code. The embedded strings were
    /// written out by hand from the recipe — `P = "[PASSAGE] "`, `L1` the second line,
    /// `L7 = " ".join(["abcdefg"] * 80)` and `cap(t) = t.strip()[:512].rstrip()` — and each
    /// value is `int.from_bytes(hashlib.sha256(s.encode()).digest()[:8], "big")`:
    ///
    /// | line | embedded as |
    /// | --- | --- |
    /// | 0 | `P + "פרק ראשון " + L1 + " קצרה"` |
    /// | 1 | `P + L1` |
    /// | 2 | not embedded: four characters |
    /// | 3 | `P + L1 + " קצרה שורה קצרה  שלום עליכם"`, the blank line a second space |
    /// | 4 | not embedded: blank |
    /// | 5 | `P + "שורה קצרה  שלום עליכם"`, stopped by the heading after it |
    /// | 6 | `P + cap("פרק שני " + L7 + " סוף")`, 511 characters after the prefix |
    /// | 7 | `P + cap(L7)`, 511 characters after the prefix |
    /// | 8 | not embedded: three characters |
    const GOLDEN: [u64; 9] = [
        14_486_376_462_059_397_028,
        3_513_560_321_072_629_322,
        0,
        14_656_262_133_556_633_992,
        0,
        6_853_974_869_505_662_655,
        17_229_778_065_505_227_803,
        6_624_076_032_601_917_199,
        0,
    ];

    /// An index of the one book `text`, committed, and a fresh searcher over it.
    fn indexed(dir: &TempDir, text: String) -> Searcher {
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        engine
            .add_text_book(
                "ספר בדיקה".to_string(),
                "/בדיקה".to_string(),
                "/books/test.txt".to_string(),
                3,
                0,
                text,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        drop(engine);
        Index::open_in_dir(dir.path())
            .unwrap()
            .reader()
            .unwrap()
            .searcher()
    }

    /// Every live document by id: its address, its stored text and what its `chunkKey`
    /// column holds, `None` where it holds nothing.
    fn rows(searcher: &Searcher) -> Vec<(u64, DocAddress, String, Option<u64>)> {
        let text = searcher.schema().get_field("text").unwrap();
        let mut rows = Vec::new();
        for (segment_ord, reader) in searcher.segment_readers().iter().enumerate() {
            let ids = reader.fast_fields().u64("id").unwrap();
            let keys = reader.fast_fields().column_opt::<u64>("chunkKey").unwrap();
            for doc in reader.doc_ids_alive() {
                let address = DocAddress::new(segment_ord as u32, doc);
                let stored: TantivyDocument = searcher.doc(address).unwrap();
                rows.push((
                    ids.first(doc).unwrap(),
                    address,
                    stored
                        .get_first(text)
                        .and_then(|value| value.as_str())
                        .unwrap()
                        .to_string(),
                    keys.as_ref().and_then(|keys| keys.first(doc)),
                ));
            }
        }
        rows.sort_by_key(|row| row.0);
        rows
    }

    #[test]
    fn a_fixed_book_is_keyed_as_python_computes_it() {
        let dir = TempDir::new().unwrap();
        let rows = rows(&indexed(&dir, book()));

        let texts: Vec<String> = rows.iter().map(|row| row.2.clone()).collect();
        assert_eq!(texts, stored(), "what the keys are computed from");

        let column: Vec<Option<u64>> = rows.iter().map(|row| row.3).collect();
        assert_eq!(column, GOLDEN.map(Some).to_vec());
    }

    /// What P2's resolver asks an index without a column, and what a displayed result is
    /// checked against: for every line, the key recomputed from the stored window is the
    /// key the column holds — from the whole book, and from the window alone.
    #[test]
    fn every_line_recomputes_to_the_key_its_column_holds() {
        let dir = TempDir::new().unwrap();
        let searcher = indexed(&dir, book());
        let rows = rows(&searcher);
        let book: Vec<DocAddress> = rows.iter().map(|row| row.1).collect();
        let reach = production_chunking().context_window_lines;

        for (ordinal, row) in rows.iter().enumerate() {
            let key = recompute_chunk_key(&searcher, &book, ordinal).unwrap();
            assert_eq!(
                Some(key.map_or(0, ChunkKey::column_value)),
                row.3,
                "line {ordinal}"
            );
            let start = ordinal.saturating_sub(reach);
            let end = (ordinal + reach).min(book.len() - 1);
            assert_eq!(
                recompute_chunk_key(&searcher, &book[start..=end], ordinal - start).unwrap(),
                key,
                "line {ordinal}, from its window"
            );
        }
        assert!(recompute_chunk_key(&searcher, &book, book.len()).is_err());
    }

    /// A PDF's lines are not the library's text: their column holds 0, and nothing
    /// recomputes a key for them.
    #[test]
    fn a_pdf_line_has_no_key() {
        use crate::api::search_engine::PdfPageInput;

        let dir = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        engine
            .add_pdf_book(
                "ספר סרוק".to_string(),
                "/בדיקה".to_string(),
                "/books/scan.pdf".to_string(),
                4,
                0,
                vec![PdfPageInput {
                    page_index: 0,
                    reference: "עמוד 1".to_string(),
                    text: "שורה ארוכה דיה כדי לעמוד בפני עצמה\nשורה שנייה ארוכה דיה גם היא"
                        .to_string(),
                }],
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        drop(engine);
        let searcher = Index::open_in_dir(dir.path())
            .unwrap()
            .reader()
            .unwrap()
            .searcher();

        let rows = rows(&searcher);
        assert_eq!(rows.len(), 2);
        let book: Vec<DocAddress> = rows.iter().map(|row| row.1).collect();
        for (ordinal, row) in rows.iter().enumerate() {
            assert_eq!(row.3, Some(0));
            assert_eq!(
                recompute_chunk_key(&searcher, &book, ordinal).unwrap(),
                None
            );
        }
    }

    /// The parallel computation answers what the sidecar's own `chunk_keys` answers, on
    /// books with sections, blank lines, short lines and lines past the cap.
    #[test]
    fn the_column_values_are_the_sidecars_chunk_keys() {
        let mut state: u64 = 0x5eed;
        let mut next = move |bound: u64| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) % bound
        };
        for _ in 0..200 {
            let mut section = 0u64;
            let texts: Vec<String> = (0..next(40) + 1)
                .map(|_| match next(5) {
                    0 => String::new(),
                    1 => "קצר".to_string(),
                    2 => "שורה קצרה".to_string(),
                    3 => "מילה "
                        .repeat(next(150) as usize + 1)
                        .trim_end()
                        .to_string(),
                    _ => "שורה ארוכה דיה כדי לעמוד בפני עצמה".to_string(),
                })
                .collect();
            let lines: Vec<LineRef<'_>> = texts
                .iter()
                .map(|text| {
                    if next(6) == 0 {
                        section += 1;
                    }
                    LineRef { text, section }
                })
                .collect();
            let sequential: Vec<u64> = PRODUCTION_CHUNKER
                .chunk_keys(&lines)
                .into_iter()
                .map(|key| key.map_or(0, ChunkKey::column_value))
                .collect();
            assert_eq!(column_values(&lines), sequential);
        }
    }
}
