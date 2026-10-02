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
//! At the crate root, not under `api`, so flutter_rust_bridge generates no bindings for it.
//! Compiled into every build, as the sidecar is: the release index is built by one build and
//! opened by all of them.

use crate::api::search_engine::LINE_TEXT_VERSION;
use once_cell::sync::Lazy;
use otzaria_semantic_search::semantic::chunk_key::{ChunkKey, LineRef, KEY_VERSION};
use otzaria_semantic_search::semantic::chunker::{Chunker, ChunkerConfig};
use rayon::prelude::*;

/// The chunking the library's vectors are built with: Meivin Round 2's, the sidecar's
/// `config/models/meivin-round2-onnx/chunking.json`.
///
/// Compiled in, because indexing runs with no model configured: every build computes a
/// line's key, whether or not it can embed. The model family the vectors are published
/// under names this configuration by its hash, [`ChunkerConfig::identity`], so a copy that
/// drifted from the file would be another `chunking_identity`, and the tests pin it to the
/// published one.
pub(crate) fn production_chunking() -> ChunkerConfig {
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
pub(crate) fn column_values(lines: &[LineRef<'_>]) -> Vec<u64> {
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
