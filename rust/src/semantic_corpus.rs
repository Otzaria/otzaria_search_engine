//! The lexical index as the semantic builder's corpus (S4b).
//!
//! `otzaria-semantic-search` never links Tantivy — the index, the schema and the id scheme
//! live here — so it takes the corpus as a port:
//! [`CorpusIndex`] answers "what is line N", and [`CorpusBooks`] answers "which lines share
//! a book, in what order". This module implements both over a live index, which is what
//! replaces the JSONL transcription that stood in for it.
//!
//! # One snapshot, for the whole build
//!
//! A build reads the corpus at least three times: once to derive the set of lines the
//! recipe embeds, once to derive the text to embed, and once when the packer joins each
//! finished vector back to its metadata. [`TantivyCorpus`] holds **one [`Searcher`]** for
//! its whole life and never reloads, because those three reads landing on three different
//! commits would mix a plan from one, context text from a second and metadata from a third.
//!
//! The packer's `source_line_sha256` would not catch that. It compares the *anchor* line's
//! text against the corpus, and the anchor is not what moved: a short line borrows text
//! from its neighbours, so a neighbour edited between two reads changes what was embedded
//! while every digest still agrees.
//!
//! `corpus_id` is derived from the same snapshot, so the identity an artifact carries names
//! the documents it was actually built from.
//!
//! # The set has to be checkable against something the scan did not produce
//!
//! [`CorpusBooks`] is the *only* source of the coverage contract: the plan is built from
//! `book_keys()` and `book_line_ids()`, and the packer then compares the vectors against
//! that same plan. A book this module failed to enumerate would therefore vanish from both
//! sides at once, and coverage would confirm itself.
//!
//! So [`TantivyCorpus::open`] cross-checks its enumeration against
//! [`Searcher::num_docs`] — a count Tantivy computes from each segment's metadata and its
//! deleted-document bitset, without any help from the scan. A book dropped, a segment
//! skipped or a document counted twice all show up as a disagreement between the two.
//!
//! # What this costs
//!
//! `open` walks every live document twice: once over the `id` and `filePath` columns to
//! build the book map, and once over the stored fields to derive `corpus_id`. The second
//! decompresses the whole store. Both are build-machine costs paid once per build, and
//! neither happens on a device — the application opens a finished artifact and never sees
//! this type.

use anyhow::{Context, Result};
use otzaria_semantic_search::distribution::builder::BuildPlan;
use otzaria_semantic_search::distribution::corpus::{CorpusBooks, CorpusIndex, CorpusLine};
use otzaria_semantic_search::errors::PackError;
use otzaria_semantic_search::semantic::chunker::ChunkerConfig;
use otzaria_semantic_search::semantic::recipe::EmbeddingRecipe;
use otzaria_semantic_search::semantic::versioning::{CorpusIdentity, ModelIdentity};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tantivy::schema::{Facet, OwnedValue, Value};
use tantivy::{DocAddress, Searcher, TantivyDocument};

/// The id scheme this crate builds, and the only one this adapter can order a book under.
///
/// `add_text_book` composes an id as `((catalogue_order + 1) << 32) + (ordinal + 1)`, so
/// within one book ascending ids are the lines in corpus order. That is not a convention
/// this module is trusting — it is arithmetic performed a few hundred lines away in
/// [`crate::api::search_engine`] — but it stops being true the moment the scheme changes,
/// which is why the version is pinned here rather than assumed.
pub const DOCUMENT_ID_SCHEME_VERSION: u32 = 1;

/// Version the digest below was computed under.
///
/// Folded into `corpus_id` itself, so a change to *what* is hashed cannot silently produce
/// a value that compares equal to one computed the old way.
const CORPUS_ID_VERSION: u32 = 1;

/// Where one line lives in the snapshot.
#[derive(Debug, Clone, Copy)]
struct Location {
    address: DocAddress,
}

/// A live, read-only Tantivy index, seen as the corpus a semantic artifact describes.
pub struct TantivyCorpus {
    /// Taken once and held. See the module documentation: reloading would let three reads
    /// of "the corpus" land on three different commits.
    searcher: Searcher,
    identity: CorpusIdentity,
    /// The recipe this corpus answers coverage for. Held because `chunking_identity` in a
    /// model identity is a one-way hash: nothing can recover a configuration from it, so an
    /// implementation that did not hold the real one could only pretend to check it.
    chunking: ChunkerConfig,
    /// Book key → its lines, ascending by `line_id`, which is corpus order under
    /// [`DOCUMENT_ID_SCHEME_VERSION`].
    books: BTreeMap<String, Vec<u64>>,
    locations: HashMap<u64, Location>,
    /// The plan, computed at most once. `expected_line_ids` is called by both `pack` and
    /// `validate_artifact`, and chunking the library twice to answer the same question
    /// twice is the kind of cost a build notices.
    plan: RefCell<Option<BTreeSet<u64>>>,
}

impl TantivyCorpus {
    /// Take the engine's current snapshot and describe it.
    ///
    /// The entry point a build uses. One call is one snapshot, held for the whole build —
    /// see the module documentation for why that matters more than it looks.
    pub fn from_engine(
        engine: &crate::api::search_engine::SearchEngine,
        library_version: impl Into<String>,
        chunking: ChunkerConfig,
    ) -> Result<Self> {
        Self::open(engine.corpus_searcher(), library_version, chunking)
    }

    /// Take a snapshot of `searcher` and describe it.
    ///
    /// `library_version` is the catalogue release the index was built from — the one fact
    /// here that no index can report about itself. Everything else is read or derived:
    /// `corpus_id` from the documents, the schema version from this build, and the id
    /// scheme from the code that composes the ids.
    pub fn open(
        searcher: Searcher,
        library_version: impl Into<String>,
        chunking: ChunkerConfig,
    ) -> Result<Self> {
        let mut books: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        let mut locations: HashMap<u64, Location> = HashMap::new();
        let mut scanned: u64 = 0;

        for (segment_ord, reader) in searcher.segment_readers().iter().enumerate() {
            let segment_ord = segment_ord as u32;
            let ids = reader
                .fast_fields()
                .u64("id")
                .context("the `id` column is required to enumerate the corpus")?;
            let paths = reader
                .fast_fields()
                .str("filePath")
                .context("reading the `filePath` column")?
                .context("the `filePath` column is required to group lines into books")?;

            let mut key = String::new();
            for doc in reader.doc_ids_alive() {
                scanned += 1;
                let line_id = ids.first(doc).with_context(|| {
                    format!("document {doc} in segment {segment_ord} carries no id")
                })?;
                let term_ord = paths
                    .term_ords(doc)
                    .next()
                    .with_context(|| format!("line {line_id} carries no filePath"))?;
                key.clear();
                if !paths.ord_to_str(term_ord, &mut key)? {
                    anyhow::bail!("line {line_id} has a filePath ordinal with no string");
                }

                // Two live documents with one id would make "the line with id N" depend on
                // which segment answered first, and the artifact would describe whichever
                // one this scan happened to reach.
                if let Some(previous) = locations.insert(
                    line_id,
                    Location {
                        address: DocAddress::new(segment_ord, doc),
                    },
                ) {
                    anyhow::bail!(
                        "line {line_id} exists twice in this snapshot: {:?} and segment \
                         {segment_ord} doc {doc}",
                        previous.address
                    );
                }
                books.entry(key.clone()).or_default().push(line_id);
            }
        }

        // The cross-check. `num_docs` comes from the segments' own metadata and their
        // deleted-document bitsets, so it is not derived from the walk above — which is the
        // whole point: the walk is the only thing that decides what gets embedded, and a
        // book it silently dropped would be missing from the plan and from the vectors
        // together, with every coverage check agreeing.
        let expected = searcher.num_docs();
        if scanned != expected {
            anyhow::bail!(
                "the corpus scan found {scanned} live document(s) and the index reports \
                 {expected}: the enumeration is not the index, and a build from it would \
                 certify its own gaps"
            );
        }
        if locations.len() as u64 != expected {
            anyhow::bail!(
                "{expected} live document(s) carry {} distinct line ids",
                locations.len()
            );
        }

        for lines in books.values_mut() {
            lines.sort_unstable();
        }

        let corpus_id = compute_corpus_id(&searcher, &books, &locations)?;
        let identity = CorpusIdentity {
            corpus_id,
            library_version: library_version.into(),
            tantivy_schema_version: crate::api::search_engine::INDEX_SCHEMA_VERSION,
            document_id_scheme_version: DOCUMENT_ID_SCHEME_VERSION,
        };

        log::info!(
            "Semantic corpus opened over {} live line(s) in {} book(s); corpus_id {}",
            expected,
            books.len(),
            identity.corpus_id
        );

        Ok(Self {
            searcher,
            identity,
            chunking,
            books,
            locations,
            plan: RefCell::new(None),
        })
    }

    /// How many lines the snapshot holds. Reported before a build starts, so an index that
    /// is obviously the wrong one is visible before a long run.
    pub fn line_count(&self) -> usize {
        self.locations.len()
    }

    pub fn book_count(&self) -> usize {
        self.books.len()
    }

    fn read_line(&self, line_id: u64) -> Result<Option<CorpusLine>, PackError> {
        let Some(location) = self.locations.get(&line_id) else {
            return Ok(None);
        };
        let address = location.address;
        let document: TantivyDocument =
            self.searcher
                .doc(address)
                .map_err(|error| PackError::Corpus {
                    reason: format!("reading line {line_id}: {error}"),
                })?;
        let reader = self.searcher.segment_reader(address.segment_ord);
        let doc = address.doc_id;
        let schema = self.searcher.schema();

        let text = |name: &str| -> Result<String, PackError> {
            let field = schema.get_field(name).map_err(|error| PackError::Corpus {
                reason: format!("schema has no {name}: {error}"),
            })?;
            Ok(document
                .get_first(field)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string())
        };
        let stored_u64 = |name: &str| -> Result<u64, PackError> {
            let field = schema.get_field(name).map_err(|error| PackError::Corpus {
                reason: format!("schema has no {name}: {error}"),
            })?;
            Ok(document
                .get_first(field)
                .and_then(|value| value.as_u64())
                .unwrap_or_default())
        };
        // `sectionId`, `lineHash` and `contentHash` are FAST and **not stored**, so they
        // come off the columnar readers rather than out of the document. Reading them from
        // the stored document would silently yield zero for all three.
        let column_u64 = |name: &str| -> Result<u64, PackError> {
            let column = reader
                .fast_fields()
                .u64(name)
                .map_err(|error| PackError::Corpus {
                    reason: format!("reading the {name} column: {error}"),
                })?;
            Ok(column.first(doc).unwrap_or_default())
        };

        let facets = self
            .read_facets(address)
            .map_err(|error| PackError::Corpus {
                reason: format!("reading the facets of line {line_id}: {error}"),
            })?;

        let is_pdf = schema
            .get_field("isPdf")
            .ok()
            .and_then(|field| document.get_first(field))
            .map(|value| matches!(OwnedValue::from(value), OwnedValue::Bool(true)))
            .unwrap_or(false);

        Ok(Some(CorpusLine {
            source_book_key: text("filePath")?,
            title: text("title")?,
            reference: text("reference")?,
            section_id: column_u64("sectionId")?,
            segment: stored_u64("segment")?,
            is_pdf,
            line_hash: column_u64("lineHash")?,
            content_hash: column_u64("contentHash")?,
            facets,
            text: text("text")?,
        }))
    }

    /// Every facet path on a document, sorted and deduplicated.
    ///
    /// Sorted because the sidecar stores this list on every vector and compares it
    /// structurally: the order a segment happens to hold ordinals in carries no meaning,
    /// and letting it through would make a record's facets differ from the same book's
    /// facets after a merge.
    fn read_facets(&self, address: DocAddress) -> Result<Vec<String>> {
        let reader = self.searcher.segment_reader(address.segment_ord);
        let facet_reader = reader.facet_reader("topics")?;
        let mut facet = Facet::default();
        let mut out = Vec::new();
        for ord in facet_reader.facet_ords(address.doc_id) {
            facet_reader.facet_from_ord(ord, &mut facet)?;
            out.push(facet.to_string());
        }
        out.sort();
        out.dedup();
        Ok(out)
    }
}

impl CorpusIndex for TantivyCorpus {
    fn identity(&self) -> Result<CorpusIdentity, PackError> {
        Ok(self.identity.clone())
    }

    /// The lines the declared recipe embeds — derived by applying it, never by looking at
    /// what came out.
    ///
    /// The recipe is checked against the identity first, and all three of its versions are
    /// resolved to behaviour this build implements. `chunking_identity` is a one-way hash,
    /// so being handed the real [`ChunkerConfig`] at construction is the only thing that
    /// makes the check possible at all.
    fn expected_line_ids(&self, model: &ModelIdentity) -> Result<BTreeSet<u64>, PackError> {
        EmbeddingRecipe::resolve(&self.chunking, model)?;
        let declared = self.chunking.identity();
        if declared != model.chunking_identity {
            return Err(PackError::RecipeMismatch {
                declared: model.chunking_identity,
                actual: declared,
            });
        }

        if let Some(cached) = self.plan.borrow().as_ref() {
            return Ok(cached.clone());
        }
        let ids = BuildPlan::compute(self, &self.chunking, model)?
            .line_ids()
            .clone();
        *self.plan.borrow_mut() = Some(ids.clone());
        Ok(ids)
    }

    fn line(&self, line_id: u64) -> Result<Option<CorpusLine>, PackError> {
        self.read_line(line_id)
    }
}

impl CorpusBooks for TantivyCorpus {
    fn book_keys(&self) -> Result<Vec<String>, PackError> {
        Ok(self.books.keys().cloned().collect())
    }

    /// A book's lines in corpus order.
    ///
    /// Ascending `line_id`, which under [`DOCUMENT_ID_SCHEME_VERSION`] is the line's
    /// position in the book — the low half of an id is `ordinal + 1`, assigned by
    /// `add_text_book` as it walks the book's lines in order. `segment` cannot be used
    /// instead: for a PDF it is the page index, so every line on a page shares it.
    fn book_line_ids(&self, book_key: &str) -> Result<Vec<u64>, PackError> {
        self.books
            .get(book_key)
            .cloned()
            .ok_or_else(|| PackError::Corpus {
                reason: format!("no book keyed {book_key:?} in this snapshot"),
            })
    }
}

/// A deterministic digest of the documents this snapshot holds.
///
/// **What it covers, and why that is the line drawn.** For every live document, in
/// ascending `line_id`: the id, the book it belongs to, and its text. Those three are what
/// a semantic result *is* — an id the application resolves back to a book and a passage —
/// so an index whose ids or text moved must produce a different value, and a semantic
/// artifact built from the old one must be refused.
///
/// Metadata (title, reference, facets) is deliberately outside. It is compared record by
/// record when an artifact is validated, and at query time the application hydrates it from
/// Tantivy rather than from the artifact, so a re-titled book is not a reason to rebuild
/// six million vectors.
///
/// Every field is length-prefixed, so no two different corpora can serialize to the same
/// bytes by moving a boundary — `("a", "bc")` and `("ab", "c")` are different inputs here.
fn compute_corpus_id(
    searcher: &Searcher,
    books: &BTreeMap<String, Vec<u64>>,
    locations: &HashMap<u64, Location>,
) -> Result<String> {
    let schema = searcher.schema();
    let text_field = schema
        .get_field("text")
        .context("the `text` field is required to derive a corpus_id")?;

    // Ascending id globally, so the value does not depend on how segments are laid out or
    // on the order books happen to be enumerated in.
    let mut ordered: Vec<(u64, &str)> = Vec::with_capacity(locations.len());
    for (book_key, lines) in books {
        ordered.extend(lines.iter().map(|line_id| (*line_id, book_key.as_str())));
    }
    ordered.sort_unstable();

    let mut hasher = Sha256::new();
    hasher.update(CORPUS_ID_VERSION.to_le_bytes());
    hasher.update((ordered.len() as u64).to_le_bytes());

    let feed = |hasher: &mut Sha256, bytes: &[u8]| {
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };

    for (line_id, book_key) in ordered {
        let location = locations
            .get(&line_id)
            .expect("every ordered id came from `locations`");
        let document: TantivyDocument = searcher.doc(location.address)?;
        let text = document
            .get_first(text_field)
            .and_then(|value| value.as_str())
            .unwrap_or_default();

        hasher.update(line_id.to_le_bytes());
        feed(&mut hasher, book_key.as_bytes());
        feed(&mut hasher, text.as_bytes());
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::search_engine::SearchEngine;
    use otzaria_semantic_search::semantic::versioning::ModelIdentity;
    use tempfile::TempDir;

    const GENESIS: &str = "/books/genesis.txt";
    const BERACHOT: &str = "/books/berachot.txt";

    /// A book whose third line is under the recipe's `min_embeddable_chars`, so the recipe
    /// skips it and a complete artifact does not carry a vector for it.
    const GENESIS_TEXT: &str = "בראשית ברא אלהים את השמים ואת הארץ\n\
                                והארץ היתה תהו ובהו וחשך על פני תהום רבה\n\
                                או\n\
                                ויאמר אלהים יהי אור ויהי אור";
    const BERACHOT_TEXT: &str = "מאימתי קורין את שמע בערבית משעה שהכהנים נכנסין לאכול בתרומתן";

    fn engine_with_books(dir: &TempDir) -> SearchEngine {
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        engine
            .add_text_book(
                "בראשית".to_string(),
                "/מקרא/תורה".to_string(),
                GENESIS.to_string(),
                0,
                0,
                GENESIS_TEXT.to_string(),
                Some(vec!["/era/תנך".to_string()]),
            )
            .unwrap();
        engine
            .add_text_book(
                "משנה ברכות".to_string(),
                "/משנה/זרעים".to_string(),
                BERACHOT.to_string(),
                1,
                0,
                BERACHOT_TEXT.to_string(),
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        engine
    }

    fn corpus(engine: &SearchEngine) -> TantivyCorpus {
        TantivyCorpus::from_engine(engine, "otzaria-library-2026-08", ChunkerConfig::default())
            .unwrap()
    }

    fn model_for(chunking: &ChunkerConfig) -> ModelIdentity {
        ModelIdentity {
            model_id: "EMD123/Otzaria-Embedding-V1-Flash-0.6B".to_string(),
            model_checksum: "ab".repeat(32),
            model_quantization: "Q4_K_M".to_string(),
            embedding_backend: "mock-hash-v1".to_string(),
            embedding_dim: 64,
            pooling: "last-token".to_string(),
            max_tokens: 512,
            embedding_text_version: chunking.embedding_text_version,
            normalization_version: 1,
            chunking_identity: chunking.identity(),
        }
    }

    /// The corpus is what the artifact's every stored field is read from, so a field this
    /// adapter cannot answer becomes a field the artifact invents.
    ///
    /// **`sectionId`, `lineHash` and `contentHash` are the reason this test exists.** All
    /// three are FAST and *not stored*, so reading them out of the retrieved document — the
    /// obvious way to write this adapter — yields zero for every line, silently, and every
    /// record in every artifact carries a zero the packer then dutifully verifies against
    /// the same zero.
    #[test]
    fn every_field_a_record_carries_is_answered_from_the_index() {
        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        let corpus = corpus(&engine);

        let first = corpus.book_line_ids(GENESIS).unwrap()[0];
        let line = corpus.line(first).unwrap().expect("the first line exists");

        assert_eq!(line.source_book_key, GENESIS);
        assert_eq!(line.title, "בראשית");
        assert_eq!(line.text, "בראשית ברא אלהים את השמים ואת הארץ");
        assert!(!line.is_pdf);
        assert_ne!(
            line.section_id, 0,
            "sectionId is FAST-only and must be read columnar"
        );
        assert_ne!(
            line.line_hash, 0,
            "lineHash is FAST-only and must be read columnar"
        );
        assert_ne!(
            line.content_hash, 0,
            "contentHash is FAST-only and must be read columnar"
        );
        assert_eq!(
            line.facets,
            vec!["/era/תנך".to_string(), "/מקרא/תורה".to_string()],
            "every facet of the book reaches the line, sorted"
        );

        assert!(corpus.line(u64::MAX).unwrap().is_none());
    }

    /// The order is the answer, not presentation: a short line takes its context from the
    /// entries next to it here, so the wrong order embeds text the book does not contain.
    #[test]
    fn books_are_grouped_and_their_lines_are_in_corpus_order() {
        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        let corpus = corpus(&engine);

        assert_eq!(corpus.book_keys().unwrap(), vec![BERACHOT, GENESIS]);
        assert_eq!(corpus.book_count(), 2);
        assert_eq!(corpus.line_count(), 5);

        let genesis = corpus.book_line_ids(GENESIS).unwrap();
        assert_eq!(genesis.len(), 4);
        assert!(
            genesis.windows(2).all(|pair| pair[0] < pair[1]),
            "ascending ids are the book's line order under scheme 1"
        );
        // The order is the text's, not the ids' by luck: the fourth line is the fourth id.
        let texts: Vec<String> = genesis
            .iter()
            .map(|id| corpus.line(*id).unwrap().unwrap().text)
            .collect();
        assert_eq!(texts, GENESIS_TEXT.lines().collect::<Vec<_>>());

        assert!(matches!(
            corpus.book_line_ids("/books/absent.txt"),
            Err(PackError::Corpus { .. })
        ));
    }

    /// The identity a build records has to name the documents it actually read. Held for
    /// the whole build, so an index that moves under it changes nothing this build sees.
    #[test]
    fn the_snapshot_does_not_move_when_the_index_does() {
        let dir = TempDir::new().unwrap();
        let mut engine = engine_with_books(&dir);
        let snapshot = corpus(&engine);
        let before = snapshot.identity().unwrap();
        let count_before = snapshot.line_count();

        engine
            .add_text_book(
                "ספר נוסף".to_string(),
                "/מקרא/נביאים".to_string(),
                "/books/joshua.txt".to_string(),
                2,
                0,
                "ויהי אחרי מות משה עבד יהוה".to_string(),
                None,
            )
            .unwrap();
        engine.commit().unwrap();

        assert_eq!(snapshot.line_count(), count_before);
        assert_eq!(snapshot.book_count(), 2);
        assert_eq!(snapshot.identity().unwrap(), before);

        // And the same engine, asked again, sees the commit — so the snapshot above is a
        // property of the corpus rather than of a reader that never reloads.
        assert_eq!(corpus(&engine).book_count(), 3);
    }

    /// A deleted line is not part of the corpus, and the cross-check has to agree with the
    /// index about that rather than with the scan about itself.
    #[test]
    fn a_deleted_line_leaves_the_corpus() {
        let dir = TempDir::new().unwrap();
        let mut engine = engine_with_books(&dir);
        let doomed = corpus(&engine).book_line_ids(BERACHOT).unwrap()[0];

        engine.delete_document_by_id(doomed).unwrap();
        engine.commit().unwrap();

        let corpus = corpus(&engine);
        assert_eq!(corpus.line_count(), 4);
        assert!(corpus.line(doomed).unwrap().is_none());
        assert!(
            !corpus.book_keys().unwrap().contains(&BERACHOT.to_string()),
            "a book with no live line is not a book"
        );
    }

    /// `corpus_id` exists to answer one question: are these the same documents? So it must
    /// move when a line's text moves, and stay put when something the application hydrates
    /// from Tantivy anyway is relabelled.
    #[test]
    fn the_corpus_id_tracks_the_documents_and_not_their_labels() {
        let first = TempDir::new().unwrap();
        let baseline = corpus(&engine_with_books(&first))
            .identity()
            .unwrap()
            .corpus_id;

        // Same documents, built again from scratch: the digest is a property of the corpus
        // and not of a run.
        let repeat = TempDir::new().unwrap();
        assert_eq!(
            corpus(&engine_with_books(&repeat))
                .identity()
                .unwrap()
                .corpus_id,
            baseline
        );

        // A different title, the same texts. The artifact's records are compared against
        // the corpus field by field when it is validated, and at query time the title comes
        // out of Tantivy — so this must not invalidate six million vectors.
        let retitled = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(retitled.path().to_str().unwrap());
        engine
            .add_text_book(
                "בראשית — מהדורה מחודשת".to_string(),
                "/מקרא/תורה".to_string(),
                GENESIS.to_string(),
                0,
                0,
                GENESIS_TEXT.to_string(),
                Some(vec!["/era/תנך".to_string()]),
            )
            .unwrap();
        engine
            .add_text_book(
                "משנה ברכות".to_string(),
                "/משנה/זרעים".to_string(),
                BERACHOT.to_string(),
                1,
                0,
                BERACHOT_TEXT.to_string(),
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        assert_eq!(corpus(&engine).identity().unwrap().corpus_id, baseline);

        // One word changed in one line. Every vector built from that line is now wrong.
        let edited = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(edited.path().to_str().unwrap());
        engine
            .add_text_book(
                "בראשית".to_string(),
                "/מקרא/תורה".to_string(),
                GENESIS.to_string(),
                0,
                0,
                GENESIS_TEXT.replace("השמים", "השמיים"),
                Some(vec!["/era/תנך".to_string()]),
            )
            .unwrap();
        engine
            .add_text_book(
                "משנה ברכות".to_string(),
                "/משנה/זרעים".to_string(),
                BERACHOT.to_string(),
                1,
                0,
                BERACHOT_TEXT.to_string(),
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        assert_ne!(corpus(&engine).identity().unwrap().corpus_id, baseline);
    }

    /// Coverage is the recipe applied to the corpus — not every document in the index, and
    /// not anything derived from vectors that were produced.
    #[test]
    fn the_expected_set_is_the_recipe_applied_to_this_snapshot() {
        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        let corpus = corpus(&engine);
        let chunking = ChunkerConfig::default();

        let expected = corpus.expected_line_ids(&model_for(&chunking)).unwrap();
        assert_eq!(
            expected.len(),
            4,
            "five live lines, and the recipe skips the one below min_embeddable_chars"
        );
        let skipped = corpus.book_line_ids(GENESIS).unwrap()[2];
        assert_eq!(corpus.line(skipped).unwrap().unwrap().text, "או");
        assert!(!expected.contains(&skipped));

        // Asked twice, answered the same — and from the cache, which must not become a way
        // to answer for a recipe the corpus was not built with.
        assert_eq!(
            corpus.expected_line_ids(&model_for(&chunking)).unwrap(),
            expected
        );
    }

    /// `chunking_identity` is a one-way hash, so an implementation that did not hold the
    /// real configuration could only pretend to check it. This is that check.
    #[test]
    fn a_model_declaring_another_recipe_is_refused() {
        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        let corpus = corpus(&engine);

        let other = ChunkerConfig {
            min_embeddable_chars: 40,
            ..ChunkerConfig::default()
        };
        assert_ne!(other.identity(), ChunkerConfig::default().identity());

        match corpus.expected_line_ids(&model_for(&other)) {
            Err(PackError::RecipeMismatch { declared, actual }) => {
                assert_eq!(declared, other.identity());
                assert_eq!(actual, ChunkerConfig::default().identity());
            }
            other => panic!("a corpus must not certify coverage for another recipe, got {other:?}"),
        }
    }

    /// **S4b's acceptance gate, without Dart:** a Tantivy index and a model in, a full
    /// semantic artifact out, verified against that same index.
    ///
    /// Everything before this ran the builder against a transcription of an index. This is
    /// the index — the recipe applied to real documents, the identity taken off the
    /// snapshot, and every record joined back to the corpus that produced it. The backend
    /// is the deterministic stand-in, so the vectors mean nothing; what is under test is
    /// the join, the coverage and the identity, none of which depend on that.
    #[test]
    fn a_tantivy_index_and_a_model_produce_an_artifact_that_verifies() {
        use otzaria_semantic_search::distribution::builder::{build, BuildRequest};
        use otzaria_semantic_search::distribution::packer::validate_artifact;
        use otzaria_semantic_search::semantic::embedding::{mock, validate_and_checksum_gguf};

        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        let corpus = corpus(&engine);
        let chunking = ChunkerConfig::default();

        let model_file = dir.path().join("model.gguf");
        mock::write_stub_gguf(&model_file, 3).unwrap();
        let model = ModelIdentity {
            model_checksum: validate_and_checksum_gguf(&model_file).unwrap(),
            ..model_for(&chunking)
        };

        let out = dir.path().join("artifact");
        let report = build(
            BuildRequest {
                output_path: out.clone(),
                model_path: model_file,
                model: model.clone(),
                chunking,
                created_at: "2026-08-09T00:00:00Z".to_string(),
                collection_name: "chunks".to_string(),
                batch_size: 2,
                // The stand-in's vectors carry no meaning; saying so is what keeps the
                // refusal the default for everything that ships.
                allow_non_semantic_backend: true,
            },
            &corpus,
        )
        .expect("a Tantivy corpus builds an artifact");

        assert_eq!(
            report.vector_count, 4,
            "five live lines, and the recipe skips the one below min_embeddable_chars"
        );
        assert_eq!(report.book_count, 2);
        assert_eq!(report.identity.corpus, corpus.identity().unwrap());

        // Verified again from the outside, against the same snapshot: the ids cover the
        // recipe exactly, and every stored record still agrees with the index field by
        // field.
        assert_eq!(
            validate_artifact(&out, &model, &corpus).unwrap().digest,
            report.digest
        );
    }

    /// The identity fields this side owns are read, not typed in beside the vectors.
    #[test]
    fn the_identity_comes_from_the_index_and_this_build() {
        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        let identity = corpus(&engine).identity().unwrap();

        assert_eq!(identity.library_version, "otzaria-library-2026-08");
        assert_eq!(
            identity.tantivy_schema_version,
            crate::api::search_engine::INDEX_SCHEMA_VERSION
        );
        assert_eq!(
            identity.document_id_scheme_version,
            DOCUMENT_ID_SCHEME_VERSION
        );
        assert_eq!(identity.corpus_id.len(), 64);
        assert!(identity.corpus_id.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
