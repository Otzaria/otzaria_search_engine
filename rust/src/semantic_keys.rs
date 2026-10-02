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
    anyhow::ensure!(
        ordinal < book.len(),
        "line {ordinal} is not in a book of {} line(s)",
        book.len()
    );
    let schema = searcher.schema();
    let text_field = schema.get_field("text")?;
    let is_pdf_field = schema.get_field("isPdf")?;

    let reach = production_chunking().context_window_lines;
    let start = ordinal.saturating_sub(reach);
    let end = ordinal.saturating_add(reach).min(book.len() - 1);
    let mut texts = Vec::with_capacity(end - start + 1);
    let mut sections = Vec::with_capacity(end - start + 1);
    for (index, &address) in book.iter().enumerate().take(end + 1).skip(start) {
        let document: TantivyDocument = searcher
            .doc(address)
            .with_context(|| format!("reading the document at {address:?}"))?;
        if index == ordinal
            && document
                .get_first(is_pdf_field)
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        {
            return Ok(None);
        }
        let text = document
            .get_first(text_field)
            .and_then(|value| value.as_str())
            .with_context(|| format!("the document at {address:?} stores no text"))?
            .to_string();
        // FAST and not stored, so read from its column.
        let section = searcher
            .segment_reader(address.segment_ord)
            .fast_fields()
            .u64("sectionId")?
            .first(address.doc_id)
            .with_context(|| format!("the document at {address:?} has no sectionId"))?;
        texts.push(text);
        sections.push(section);
    }
    let lines: Vec<LineRef<'_>> = texts
        .iter()
        .zip(&sections)
        .map(|(text, &section)| LineRef { text, section })
        .collect();
    Ok(PRODUCTION_CHUNKER
        .embedded_text(&lines, ordinal - start)
        .map(|text| ChunkKey::of(&text)))
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
