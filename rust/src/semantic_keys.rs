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
//! A line whose text is in the library database (`TextStorage::LibraryDb`) is keyed from its
//! row, read back as a result reads it: a row changed since it was indexed is keyed as it
//! reads now, which is the text a result shows, and one that cannot be read now fails the
//! recomputation rather than keying an empty line.
//!
//! At the crate root, not under `api`, so flutter_rust_bridge generates no bindings for it.
//! Compiled into every build, as the sidecar is: the release index is built by one build and
//! opened by all of them. Public on the Rust side alone, for the tools and tests that hold
//! the recipe to the one a model publishes.

#[cfg(feature = "semantic-integration")]
use crate::api::search_engine::line_dedup_hashes;
use crate::api::search_engine::{indexed_texts, TextStatus, LINE_TEXT_VERSION};
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use otzaria_semantic_search::semantic::chunk_key::{ChunkKey, LineRef, KEY_VERSION};
use otzaria_semantic_search::semantic::chunker::{Chunker, ChunkerConfig};
use rayon::prelude::*;
#[cfg(feature = "semantic-integration")]
use std::collections::HashMap;
use tantivy::columnar::Column;
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
/// the text of the line and of its neighbours within the recipe's context window,
/// and the `sectionId` each belongs to. `None` when the production recipe does not embed the
/// line, and for a PDF's line, which no vector is built from.
///
/// `book` is the book's documents in line order: the whole book, or any stretch of it that
/// holds the line and its neighbours on each side within the window — fewer only where the
/// book ends. Nothing further out is read, so a check costs at most five documents, and one
/// for a line long enough to stand alone, which is embedded as its own text.
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
    let is_pdf_field = schema.get_field("isPdf")?;

    // One line long enough to stand alone is embedded as its own text, whatever its
    // neighbours say: what checking a resolved line asks, most of the time, at one document
    // instead of five.
    if lines.len() == 1 {
        let address = book[lines.start];
        let document: TantivyDocument = searcher
            .doc(address)
            .with_context(|| format!("reading the document at {address:?}"))?;
        if document
            .get_first(is_pdf_field)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            return Ok(vec![None]);
        }
        let text = key_texts(searcher, &[(address, document)])?
            .pop()
            .expect("one text for one document");
        if text.trim().chars().count() >= production_chunking().min_meaningful_chars {
            let alone = [LineRef {
                text: &text,
                section: 0,
            }];
            return Ok(vec![PRODUCTION_CHUNKER
                .embedded_text(&alone, 0)
                .map(|text| ChunkKey::of(&text))]);
        }
    }

    let reach = production_chunking().context_window_lines;
    let start = lines.start.saturating_sub(reach);
    let end = (lines.end - 1).saturating_add(reach).min(book.len() - 1);
    let mut documents = Vec::with_capacity(end - start + 1);
    let mut sections = Vec::with_capacity(end - start + 1);
    let mut pdf = Vec::with_capacity(end - start + 1);
    // Opened once per segment: opening a column reads its whole block index.
    let mut section_columns: Vec<Option<Column<u64>>> =
        vec![None; searcher.segment_readers().len()];
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
        documents.push((address, document));
        // FAST and not stored, so read from its column.
        let column = match &mut section_columns[address.segment_ord as usize] {
            Some(column) => column,
            unopened => unopened.insert(
                searcher
                    .segment_reader(address.segment_ord)
                    .fast_fields()
                    .u64("sectionId")?,
            ),
        };
        sections.push(
            column
                .first(address.doc_id)
                .with_context(|| format!("the document at {address:?} has no sectionId"))?,
        );
    }
    let texts = key_texts(searcher, &documents)?;
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

/// The text each of `documents` is keyed from: see the module documentation.
fn key_texts(
    searcher: &Searcher,
    documents: &[(DocAddress, TantivyDocument)],
) -> Result<Vec<String>> {
    indexed_texts(searcher, documents)?
        .into_iter()
        .zip(documents)
        .map(|(line, (address, _))| match line.status {
            TextStatus::Ok | TextStatus::Stale => Ok(line.text),
            TextStatus::Unavailable => anyhow::bail!(
                "the text of the document at {address:?} is in the library database, which \
                 cannot be read now"
            ),
        })
        .collect()
}

/// The text of the lines at `addresses`, as a key is computed from it.
#[cfg(feature = "semantic-integration")]
pub(crate) fn line_texts(searcher: &Searcher, addresses: &[DocAddress]) -> Result<Vec<String>> {
    let documents = addresses
        .iter()
        .map(|&address| {
            let document: TantivyDocument = searcher
                .doc(address)
                .with_context(|| format!("reading the document at {address:?}"))?;
            Ok((address, document))
        })
        .collect::<Result<Vec<_>>>()?;
    key_texts(searcher, &documents)
}

/// The positions the key of a short line at `position` of a book of `len` lines is computed
/// over: the line, and up to the recipe's `context_window_lines` on each side for as long as
/// its section runs unbroken — the sidecar's chunker for a line under `min_meaningful_chars`.
/// `section` gives the section of a position; `None` when one the window needs cannot be
/// read.
#[cfg(feature = "semantic-integration")]
pub(crate) fn context_window(
    len: usize,
    position: usize,
    section: impl Fn(usize) -> Option<u64>,
) -> Option<std::ops::RangeInclusive<usize>> {
    let reach = production_chunking().context_window_lines;
    let own = section(position)?;
    let mut start = position;
    for _ in 0..reach {
        if start == 0 || section(start - 1)? != own {
            break;
        }
        start -= 1;
    }
    let mut end = position;
    for _ in 0..reach {
        if end + 1 >= len || section(end + 1)? != own {
            break;
        }
        end += 1;
    }
    Some(start..=end)
}

/// What a short line's key says of the lines that can hold it, from their windows'
/// `lineHash`es alone. Sound as long as stored lines are trimmed and single-spaced, as
/// `normalize_text_for_indexing` stores them.
#[cfg(feature = "semantic-integration")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeySpan {
    /// The key's text, when no window that does not join to it can hold the key: not for a
    /// line that stands alone, a text the cap may have cut, or one a line alone could be.
    words: Option<Words>,
    /// The first `lineHash` of the line's window that is not 0.
    lead: Option<u64>,
}

/// A text cut at each space, and the `lineHash` of every run of its words a line can be.
#[cfg(feature = "semantic-integration")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Words {
    /// Where two spaces meet: a blank line's place.
    empty: Vec<bool>,
    /// Each run of words that are not empty, `(first, last)`, by its `lineHash`.
    runs: HashMap<u64, Vec<(usize, usize)>>,
}

#[cfg(feature = "semantic-integration")]
impl KeySpan {
    /// Any line may hold the key.
    pub(crate) const ANY: Self = Self {
        words: None,
        lead: None,
    };

    /// The span of the key of a short line whose [`context_window`] holds `texts`, the line's
    /// own `lineHash` being `own`.
    pub(crate) fn joined(texts: &[&str], own: u64) -> Self {
        let lead = texts
            .iter()
            .filter_map(|&text| line_dedup_hashes(&[text]).next())
            .find(|&hash| hash != 0);
        Self {
            words: Words::of(texts, own),
            lead,
        }
    }

    /// Whether a line whose [`context_window`] — were the line short — has the `lineHash`es
    /// `window` can hold the key: whether those lines, in order, can join to its text.
    pub(crate) fn admits(&self, window: &[u64]) -> bool {
        self.words.as_ref().is_none_or(|words| words.admit(window))
    }

    /// Whether `window` begins as the line's own does: the likelier to hold the key.
    pub(crate) fn leads(&self, window: &[u64]) -> bool {
        self.lead
            .is_none_or(|lead| window.iter().find(|&&hash| hash != 0) == Some(&lead))
    }
}

#[cfg(feature = "semantic-integration")]
impl Words {
    fn of(texts: &[&str], own: u64) -> Option<Self> {
        let chunking = production_chunking();
        // A window the cap cuts keeps the key only past a run of spaces, shorter than this.
        let room = 2 * chunking.context_window_lines + 1;
        let joined = texts.join(" ");
        let text = joined.trim();
        let length = text.chars().count();
        if length == 0 || length + room > chunking.max_chunk_chars {
            return None;
        }
        let words: Vec<&str> = text.split(' ').collect();
        let mut runs: HashMap<u64, Vec<(usize, usize)>> = HashMap::new();
        for first in 0..words.len() {
            let end = words[first..]
                .iter()
                .position(|word| word.is_empty())
                .map_or(words.len(), |run| first + run);
            let hashes = line_dedup_hashes(&words[first..end]);
            for (last, hash) in (first..end).zip(hashes) {
                runs.entry(hash).or_default().push((first, last));
            }
        }
        // A line that stands alone is keyed as its own text, whatever its window.
        let alone = runs
            .get(&own)
            .is_some_and(|found| found.contains(&(0, words.len() - 1)));
        if alone && length >= chunking.min_meaningful_chars {
            return None;
        }
        Some(Self {
            empty: words.iter().map(|word| word.is_empty()).collect(),
            runs,
        })
    }

    fn admit(&self, window: &[u64]) -> bool {
        let count = self.empty.len();
        // `at[w]`: the lines so far can be the words before `w`.
        let mut at = vec![false; count + 1];
        let mut next = vec![false; count + 1];
        at[0] = true;
        for &hash in window {
            next.fill(false);
            if hash == 0 {
                // Blank: trimmed before or after the text, or where two spaces meet.
                next[0] = at[0];
                next[count] = at[count];
                for word in 0..count {
                    next[word + 1] |= at[word] && self.empty[word];
                }
            }
            for &(first, last) in self.runs.get(&hash).into_iter().flatten() {
                next[last + 1] |= at[first];
            }
            if !next.contains(&true) {
                return false;
            }
            std::mem::swap(&mut at, &mut next);
        }
        at[count]
    }
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
    use crate::api::search_engine::{SearchEngine, TextStorage};
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
                TextStorage::InIndex,
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
        let text = searcher.schema().get_field("textStored").unwrap();
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

    /// [`KeySpan`] and [`context_window`] against the sidecar's chunker.
    #[cfg(feature = "semantic-integration")]
    mod spans {
        use super::*;

        /// A repeatable sequence: `next(bound)` in `0..bound`.
        fn sequence(seed: u64) -> impl FnMut(u64) -> u64 {
            let mut state = seed;
            move |bound: u64| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 33) % bound
            }
        }

        /// The index's `lineHash` of a line.
        fn line_hash(text: &str) -> u64 {
            line_dedup_hashes(&[text]).next().unwrap()
        }

        /// A book of lines with no space inside them — so a text is cut into lines one way
        /// only — on every branch the span takes: blank lines, lines under twelve letters,
        /// short lines with a `lineHash`, lines that stand alone, and lines long enough for
        /// the cap to cut a window; few distinct ones, so that keys recur, and in sections.
        fn book(next: &mut impl FnMut(u64) -> u64) -> (Vec<String>, Vec<u64>) {
            const TEXTS: [&str; 6] = [
                "",
                "קצר",
                "אבגדהוזחטיכלמ",
                "נסעפצקרשתאבגד",
                "שורה_ארוכה_דיה_כדי_לעמוד_בפני_עצמה",
                "אחרת_ארוכה_דיה_כדי_לעמוד_בפני_עצמה",
            ];
            let mut section = 0;
            (0..next(30) + 1)
                .map(|_| {
                    if next(5) == 0 {
                        section += 1;
                    }
                    let text = match next(10) {
                        0 => "א".repeat(next(700) as usize + 100),
                        n => TEXTS[(n as usize - 1) % TEXTS.len()].to_string(),
                    };
                    (text, section)
                })
                .unzip()
        }

        fn lines<'a>(texts: &'a [String], sections: &[u64]) -> Vec<LineRef<'a>> {
            texts
                .iter()
                .zip(sections)
                .map(|(text, &section)| LineRef { text, section })
                .collect()
        }

        /// Blocks of up to three lines, each a section, so a short line's key is its block's
        /// words; half of them an earlier block's words cut at other spaces.
        fn recut(next: &mut impl FnMut(u64) -> u64) -> (Vec<String>, Vec<u64>) {
            const WORDS: [&str; 5] = ["אבגדה", "וזחטי", "כלמנס", "עפ,", "צקרשת"];
            let mut blocks: Vec<Vec<&str>> = Vec::new();
            let (mut texts, mut sections) = (Vec::new(), Vec::new());
            for section in 0..next(12) + 1 {
                let words = match blocks.len() {
                    0 => None,
                    seen => Some(blocks[next(seen as u64) as usize].clone()),
                }
                .filter(|_| next(2) == 0)
                .unwrap_or_else(|| {
                    (0..next(8) + 1)
                        .map(|_| WORDS[next(WORDS.len() as u64) as usize])
                        .collect()
                });
                let mut lines: Vec<String> = Vec::new();
                let mut line: Vec<&str> = Vec::new();
                for (at, word) in words.iter().enumerate() {
                    line.push(word);
                    if at + 1 < words.len() && lines.len() < 2 && next(3) == 0 {
                        lines.push(line.join(" "));
                        line.clear();
                    }
                }
                lines.push(line.join(" "));
                if lines.len() < 3 && next(4) == 0 {
                    lines.insert(next(lines.len() as u64 + 1) as usize, String::new());
                }
                sections.extend(std::iter::repeat_n(section, lines.len()));
                texts.extend(lines);
                blocks.push(words);
            }
            (texts, sections)
        }

        fn is_short(text: &str) -> bool {
            text.trim().chars().count() < production_chunking().min_meaningful_chars
        }

        /// The window is the lines the chunker joins: a short line's text from the whole book
        /// is its text from its window alone, as one section.
        #[test]
        fn the_window_is_the_lines_the_chunker_joins() {
            let mut next = sequence(0x5eed);
            for _ in 0..300 {
                let (texts, sections) = book(&mut next);
                let lines = lines(&texts, &sections);
                for position in 0..lines.len() {
                    if !is_short(&texts[position]) {
                        continue;
                    }
                    let window =
                        context_window(lines.len(), position, |at| Some(sections[at])).unwrap();
                    let alone: Vec<LineRef<'_>> = lines[window.clone()]
                        .iter()
                        .map(|line| LineRef {
                            text: line.text,
                            section: 0,
                        })
                        .collect();
                    assert_eq!(
                        PRODUCTION_CHUNKER.embedded_text(&alone, position - window.start()),
                        PRODUCTION_CHUNKER.embedded_text(&lines, position),
                        "line {position} of {texts:?} in {sections:?}"
                    );
                }
            }
        }

        /// No line that can be a candidate and holds the key is passed over, however its window
        /// is cut into lines; most that do not hold it are.
        #[test]
        fn a_line_that_holds_the_key_is_never_passed_over() {
            let mut next = sequence(0xa11ce);
            let (mut shared, mut recut_shared, mut passed_over) = (0, 0, 0);
            for round in 0..800 {
                let (texts, sections) = if round % 2 == 0 {
                    book(&mut next)
                } else {
                    recut(&mut next)
                };
                let lines = lines(&texts, &sections);
                let keys = PRODUCTION_CHUNKER.chunk_keys(&lines);
                let hashes: Vec<u64> = texts.iter().map(|text| line_hash(text)).collect();
                let window = |position: usize| {
                    context_window(lines.len(), position, |at| Some(sections[at])).unwrap()
                };
                for (position, key) in keys.iter().enumerate() {
                    let Some(key) = key else { continue };
                    let span = if is_short(&texts[position]) {
                        let texts: Vec<&str> =
                            texts[window(position)].iter().map(String::as_str).collect();
                        KeySpan::joined(&texts, hashes[position])
                    } else {
                        KeySpan::ANY
                    };
                    for other in (0..lines.len()).filter(|&other| other != position) {
                        let admitted = span.admits(&hashes[window(other)]);
                        // The resolver's candidates share the line's `lineHash`; a short line
                        // is held to its window whatever its own.
                        let candidate =
                            hashes[other] == hashes[position] || is_short(&texts[other]);
                        if keys[other] == Some(*key) && candidate {
                            shared += 1;
                            if hashes[window(other)] != hashes[window(position)] {
                                recut_shared += 1;
                            }
                            assert!(
                                admitted,
                                "line {other} holds line {position}'s key: {texts:?} in \
                                 {sections:?}, {span:?}"
                            );
                        } else if !admitted {
                            passed_over += 1;
                        }
                    }
                }
            }
            assert!(shared > 1_000, "{shared} pairs shared a key");
            assert!(recut_shared > 1_000, "{recut_shared} pairs were cut apart");
            assert!(passed_over > 10_000, "{passed_over} pairs were passed over");
        }

        /// A short line's span by hand.
        #[test]
        fn a_span_admits_every_cut_of_its_text_and_nothing_else() {
            let short = "ויאמר משה אל העם";
            let window = [
                "ויקרא משה אל כל ישראל ויאמר אליהם",
                "שמע ישראל את החוקים ואת המשפטים",
                short,
                "אשר אנכי דובר באזניכם היום",
                "ולמדתם אותם ושמרתם לעשותם",
            ];
            let hashes =
                |texts: &[&str]| -> Vec<u64> { texts.iter().map(|t| line_hash(t)).collect() };
            let own = hashes(&window);
            let span = KeySpan::joined(&window, line_hash(short));
            assert!(span.admits(&own) && span.leads(&own));
            // Its first two lines cut at another space: admitted, and tried after those that
            // begin as it does.
            let resplit = hashes(&[
                "ויקרא משה אל כל ישראל",
                "ויאמר אליהם שמע ישראל את החוקים ואת המשפטים",
                short,
                window[3],
                window[4],
            ]);
            assert!(span.admits(&resplit) && !span.leads(&resplit));
            // Blank lines at either end are trimmed.
            assert!(span.admits(&[&[0][..], &own[..]].concat()));
            assert!(span.admits(&[&own[..], &[0]].concat()));
            // Out of order, short of the text, past it, or with a line it does not hold.
            assert!(!span.admits(&[own[1], own[0], own[2], own[3], own[4]]));
            assert!(!span.admits(&own[..4]));
            assert!(!span.admits(&[&own[1..], &[line_hash("שורה אחרת שאינה בטקסט כלל")]].concat()));
            assert!(!span.admits(&[own[0], own[1], own[2], 0, own[4]]));

            // A blank line inside the text is a place two spaces meet.
            let span = KeySpan::joined(&[window[0], "", short], line_hash(short));
            assert!(span.admits(&[own[0], 0, own[2]]));
            assert!(!span.admits(&[own[0], own[2]]));

            // A text the cap may cut, and one a line standing alone could be, rule nothing out.
            let long = "א".repeat(600);
            let anything = [line_hash("שורה אחרת שאינה בטקסט כלל"), 0, 7];
            for texts in [[short, &long, ""], ["—", short, "—"]] {
                assert!(KeySpan::joined(&texts, line_hash(short)).admits(&anything));
            }
            assert!(KeySpan::ANY.admits(&anything) && KeySpan::ANY.leads(&anything));
        }

        /// Within a few characters of the cap a window can go on without changing its key:
        /// the cap ends the text on the separator before the next line — after a blank line's
        /// too — and the recipe drops it. So a text that close to the cap is not held to be
        /// whole, and a longer window that holds its key is not passed over.
        #[test]
        fn a_window_that_goes_on_past_the_cap_can_hold_the_key() {
            let short = "אבגדהוזחטיכלמ";
            let after = "שורה_ארוכה_דיה_כדי_לעמוד_בפני_עצמה";
            for (long, blanks) in [(497, 0), (497, 1), (496, 1), (495, 2)] {
                let long = "ב".repeat(long);
                // The text alone in its section, and again with more after it in another.
                let mut texts = vec![long.clone(), short.to_string(), long, short.to_string()];
                texts.extend(std::iter::repeat_n(String::new(), blanks));
                texts.push(after.to_string());
                let sections: Vec<u64> = (0..texts.len()).map(|at| u64::from(at >= 2)).collect();
                let lines = lines(&texts, &sections);
                let keys = PRODUCTION_CHUNKER.chunk_keys(&lines);
                assert_eq!(keys[1], keys[3], "{blanks} blank line(s)");
                assert!(keys[1].is_some());

                let hashes: Vec<u64> = texts.iter().map(|text| line_hash(text)).collect();
                let window = |position: usize| {
                    context_window(lines.len(), position, |at| Some(sections[at])).unwrap()
                };
                let own = window(1);
                let own_texts: Vec<&str> = texts[own.clone()].iter().map(String::as_str).collect();
                let span = KeySpan::joined(&own_texts, hashes[1]);
                assert!(
                    span.admits(&hashes[window(3)]),
                    "{blanks} blank line(s): {span:?}"
                );
            }
        }
    }
}
