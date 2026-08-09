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
use std::path::Path;
use tantivy::schema::{Facet, Value};
use tantivy::{DocAddress, Index, ReloadPolicy, Searcher, TantivyDocument};

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
/// a value that compares equal to one computed the old way. Version 2 covers every field of
/// a line; version 1 covered only the id, the book key and the text, and therefore did not
/// move when a re-section changed what a short line embedded.
const CORPUS_ID_VERSION: u32 = 2;

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
    /// Open an index directory for reading, and describe it.
    ///
    /// **The read-only way in, and the one a build uses.** [`Self::from_engine`] goes
    /// through [`SearchEngine`](crate::api::search_engine::SearchEngine), which is the
    /// *writer's* door: it calls `Index::open_or_create`, opens an `IndexWriter`, and may
    /// stamp fresh metadata onto the directory. For a build that is four faults at once —
    /// a mistyped path creates an empty index instead of reporting that there is none, a
    /// legacy-compatible index is silently re-stamped, a writer lock is held for the hours
    /// a build takes, and an incompatible schema panics before anything can report it.
    ///
    /// This opens the directory, and nothing else. Compatibility is checked *first*, so an
    /// index this build cannot read is a `Result` rather than a panic from inside tantivy.
    pub fn from_index_path(
        index_path: &Path,
        library_version: impl Into<String>,
        chunking: ChunkerConfig,
    ) -> Result<Self> {
        let compatibility =
            crate::api::search_engine::check_index_compatibility(index_path.display().to_string());
        ensure_compatible(&compatibility)?;

        // `open_in_dir`, never `open_or_create`: a path that holds no index is an error to
        // report, not an index to invent.
        let index = Index::open_in_dir(index_path)
            .with_context(|| format!("opening the index at {}", index_path.display()))?;
        // No `IndexWriter`, and therefore no lock: nothing here writes, and a build that
        // held the writer for its whole run would block every other tool on the machine.
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .with_context(|| format!("reading the index at {}", index_path.display()))?;
        Self::open(reader.searcher(), library_version, chunking)
    }

    /// Take the engine's current snapshot and describe it.
    ///
    /// The entry point a build uses. One call is one snapshot, held for the whole build —
    /// see the module documentation for why that matters more than it looks.
    pub fn from_engine(
        engine: &crate::api::search_engine::SearchEngine,
        library_version: impl Into<String>,
        chunking: ChunkerConfig,
    ) -> Result<Self> {
        // The schema version an artifact declares has to be one something checked, not this
        // build's constant repeated back. `SearchEngine` validates its index's metadata
        // against `INDEX_SCHEMA_VERSION` when it opens it; asking again here is what turns
        // that into a precondition of building rather than a fact about the binary.
        ensure_compatible(&engine.index_compatibility())?;
        Self::open(engine.corpus_searcher(), library_version, chunking)
    }

    /// Take a snapshot of `searcher` and describe it.
    ///
    /// `library_version` is the catalogue release the index was built from — the one fact
    /// here that no index can report about itself. Everything else is read or derived:
    /// `corpus_id` from the documents, the schema version from this build, and the id
    /// scheme from the code that composes the ids.
    ///
    /// `pub(crate)`, so the only way in is [`Self::from_engine`]. A bare `Searcher` carries
    /// no evidence that the index it came from is one this build can read — the schema it
    /// would then be labelled with is this crate's constant, not a value anything checked —
    /// and an artifact labelled with an unverified schema version is an artifact that opens
    /// against the wrong index.
    pub(crate) fn open(
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
                let line_id = exactly_one(ids.values_for_doc(doc), "id", u64::from(doc))
                    .map_err(|reason| anyhow::anyhow!("in segment {segment_ord}: {reason}"))?;
                let term_ord = exactly_one(paths.term_ords(doc), "filePath", line_id)
                    .map_err(|reason| anyhow::anyhow!("{reason}"))?;
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
        ensure_ids_encode_positions(&books)?;

        // Built with a placeholder identity, because deriving `corpus_id` means reading
        // every line through the same strict reader the build will use — which is a method
        // on the corpus, not a second way to read a document.
        let mut corpus = Self {
            searcher,
            identity: CorpusIdentity {
                corpus_id: String::new(),
                library_version: library_version.into(),
                tantivy_schema_version: crate::api::search_engine::INDEX_SCHEMA_VERSION,
                document_id_scheme_version: DOCUMENT_ID_SCHEME_VERSION,
            },
            chunking,
            books,
            locations,
            plan: RefCell::new(None),
        };
        corpus.identity.corpus_id = compute_corpus_id(&corpus)?;

        log::info!(
            "Semantic corpus opened over {} live line(s) in {} book(s); corpus_id {}",
            expected,
            corpus.books.len(),
            corpus.identity.corpus_id
        );

        Ok(corpus)
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
        self.read_at(location.address, line_id)
            .map(Some)
            .map_err(|reason| PackError::Corpus { reason })
    }

    /// Every field of one line, or an explanation of which one the index could not answer.
    ///
    /// **Nothing here defaults.** A missing text is not `""`, a missing column is not `0`
    /// and a missing `isPdf` is not `false`: each of those is a value the artifact would
    /// then carry, that the packer would verify against the same fabricated value, and that
    /// the application would filter and group by. A document the index counts but cannot
    /// describe has to stop the build.
    ///
    /// `contentHash` is the one field where `0` is a real answer — a PDF has no fingerprint
    /// in the library database — which is exactly why "the column has no value for this
    /// document" and "the value is zero" must not collapse into each other.
    fn read_at(&self, address: DocAddress, line_id: u64) -> Result<CorpusLine, String> {
        let document: TantivyDocument = self
            .searcher
            .doc(address)
            .map_err(|error| format!("reading line {line_id}: {error}"))?;
        let reader = self.searcher.segment_reader(address.segment_ord);
        let doc = address.doc_id;
        let schema = self.searcher.schema();

        /// One stored value, or a named failure. Multi-valued is refused with the rest: a
        /// second `title` on a document would make "the title" depend on insertion order.
        fn stored<'a>(
            schema: &tantivy::schema::Schema,
            document: &'a TantivyDocument,
            name: &str,
            line_id: u64,
        ) -> Result<tantivy::schema::document::CompactDocValue<'a>, String> {
            let field = schema
                .get_field(name)
                .map_err(|error| format!("the schema has no {name} field: {error}"))?;
            let mut values = document.get_all(field);
            let first = values
                .next()
                .ok_or_else(|| format!("line {line_id} carries no {name}"))?;
            if values.next().is_some() {
                return Err(format!("line {line_id} carries more than one {name}"));
            }
            Ok(first)
        }

        let text_field = |name: &str| -> Result<String, String> {
            stored(schema, &document, name, line_id)?
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("line {line_id} carries a {name} that is not text"))
        };
        let stored_u64 = |name: &str| -> Result<u64, String> {
            stored(schema, &document, name, line_id)?
                .as_u64()
                .ok_or_else(|| format!("line {line_id} carries a {name} that is not a u64"))
        };
        let stored_bool = |name: &str| -> Result<bool, String> {
            stored(schema, &document, name, line_id)?
                .as_bool()
                .ok_or_else(|| format!("line {line_id} carries a {name} that is not a bool"))
        };
        // `sectionId`, `lineHash` and `contentHash` are FAST and **not stored**, so they
        // come off the columnar readers rather than out of the document. Reading them from
        // the stored document yields nothing at all — which, defaulted, is a zero in every
        // record of every artifact.
        //
        // `values_for_doc` rather than `first`: a columnar field is multi-valued in
        // tantivy whatever the schema suggests, and `first` answers "at least one" — so a
        // document carrying two `sectionId`s would silently contribute whichever the
        // column happened to hold first, and the artifact would group by it.
        let column_u64 = |name: &str| -> Result<u64, String> {
            let column = reader
                .fast_fields()
                .u64(name)
                .map_err(|error| format!("reading the {name} column: {error}"))?;
            exactly_one(column.values_for_doc(doc), name, line_id)
        };

        // The id the enumeration keyed this line by, read back from the document itself.
        // The scan takes it from the `id` column and this takes it from the stored field;
        // a document whose two copies disagree would be filed under one id and describe
        // another, and every record built from it would name the wrong line.
        let stored_id = stored(schema, &document, "id", line_id)?
            .as_u64()
            .ok_or_else(|| format!("line {line_id} carries an id that is not a u64"))?;
        if stored_id != line_id {
            return Err(format!(
                "the line enumerated as {line_id} stores the id {stored_id}"
            ));
        }

        Ok(CorpusLine {
            source_book_key: text_field("filePath")?,
            title: text_field("title")?,
            reference: text_field("reference")?,
            section_id: column_u64("sectionId")?,
            segment: stored_u64("segment")?,
            is_pdf: stored_bool("isPdf")?,
            line_hash: column_u64("lineHash")?,
            content_hash: column_u64("contentHash")?,
            facets: self
                .read_facets(address)
                .map_err(|error| format!("reading the facets of line {line_id}: {error}"))?,
            text: text_field("text")?,
        })
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

/// A deterministic digest of the corpus this snapshot holds.
///
/// **Every field of every line, not just its text.** `corpus_id` is the *only* thing
/// standing between an installed artifact and an index that has moved: on a device there is
/// no join against Tantivy — [`OfficialSemanticIndex::open`] compares identities and then
/// reads vectors. So anything that changes what a vector means, or what the application
/// does with the result, has to change this value:
///
/// * `text` and `section_id` decide what was embedded — a short line takes its context from
///   its neighbours *in the same section*, so a re-sectioned book embeds different text.
/// * `facets` and `is_pdf` are what the sidecar filters on, before any hydration.
/// * `line_hash` is what `IdenticalText` grouping collapses on, and `section_id` is what
///   `SameSection` groups by.
/// * `title` and `reference` are displayed from Tantivy at query time, but they are also
///   stored in the artifact and compared record by record when it is validated — and "the
///   build machine would have caught it" is not a guarantee an installation has.
///
/// The cost of the wider digest is that re-titling a book invalidates its vectors. That is
/// the right default: repacking metadata without re-running inference is a future
/// optimization, and silently accepting stale metadata is not.
///
/// Every field is length-prefixed through the canonical JSON of [`CorpusLine`], so no two
/// different corpora serialize to the same bytes by moving a boundary.
fn compute_corpus_id(corpus: &TantivyCorpus) -> Result<String> {
    // Ascending id globally, so the value does not depend on how segments are laid out or
    // on the order books happen to be enumerated in.
    let mut ordered: Vec<u64> = corpus.locations.keys().copied().collect();
    ordered.sort_unstable();

    let mut hasher = Sha256::new();
    hasher.update(CORPUS_ID_VERSION.to_le_bytes());
    hasher.update((ordered.len() as u64).to_le_bytes());

    for line_id in ordered {
        let address = corpus.locations[&line_id].address;
        let line = corpus
            .read_at(address, line_id)
            .map_err(|reason| anyhow::anyhow!("{reason}"))?;
        feed_line(&mut hasher, line_id, &line)?;
    }

    Ok(format!("{:x}", hasher.finalize()))
}

/// One line's contribution to the digest: its id, and the canonical JSON of everything the
/// corpus says about it.
///
/// Serde emits a struct's fields in declaration order, so this is stable for a given
/// [`CorpusLine`] — and a field *added* to that struct changes every `corpus_id`, which is
/// correct: a field worth storing in an artifact is a field worth invalidating one over.
fn feed_line(hasher: &mut Sha256, line_id: u64, line: &CorpusLine) -> Result<()> {
    let canonical = serde_json::to_vec(line)?;
    hasher.update(line_id.to_le_bytes());
    hasher.update((canonical.len() as u64).to_le_bytes());
    hasher.update(&canonical);
    Ok(())
}

/// Refuse an index this build does not read, before anything opens it.
fn ensure_compatible(compatibility: &crate::api::search_engine::IndexCompatibility) -> Result<()> {
    if compatibility.compatible {
        return Ok(());
    }
    anyhow::bail!(
        "this index is not one this build reads (status {}, schema {:?}, required {}): {}",
        compatibility.status,
        compatibility.found_schema_version,
        compatibility.required_schema_version,
        compatibility.reason.as_deref().unwrap_or("no reason given")
    )
}

/// One value, or a named refusal.
///
/// "At least one" is not the check that matters. Every columnar field in tantivy is
/// multi-valued underneath, whatever the schema implies, so a document carrying two
/// `sectionId`s or two `id`s has one of them chosen by storage layout — and an artifact
/// built from it describes a line nobody can point at.
fn exactly_one<T>(
    mut values: impl Iterator<Item = T>,
    field: &str,
    line_id: u64,
) -> Result<T, String> {
    let first = values
        .next()
        .ok_or_else(|| format!("line {line_id} has no {field}"))?;
    if values.next().is_some() {
        return Err(format!("line {line_id} carries more than one {field}"));
    }
    Ok(first)
}

/// Refuse a snapshot whose ids were not composed the way this adapter reads them.
///
/// A book's lines come back in ascending `line_id` because scheme 1 puts the line's
/// position in the low half. `add_text_book` composes ids that way, but the public
/// `add_document` API accepts any `u64` — so an index can hold ids that do not encode a
/// position at all, and this adapter would still declare `document_id_scheme_version = 1`
/// and hand the chunker neighbours in an order nobody chose.
///
/// What is checked is the structure the ordering depends on: every line of a book shares one
/// high half, no two books share one, and no line sits at position zero.
///
/// **Contiguity is deliberately not required.** Deleting a line leaves a gap, and a corpus
/// with a deleted line is a normal corpus — demanding `1..=n` would refuse it.
fn ensure_ids_encode_positions(books: &BTreeMap<String, Vec<u64>>) -> Result<()> {
    let mut owner: HashMap<u32, &str> = HashMap::new();
    for (book_key, lines) in books {
        let high = (lines[0] >> 32) as u32;
        // Scheme 1 composes the high half as `catalogue_order + 1`, so zero is not a
        // catalogue position at all — it is what a raw ordinal looks like when nothing
        // composed it, and `line_id = 1` would otherwise be accepted and stamped as
        // scheme 1.
        if high == 0 {
            anyhow::bail!(
                "book {book_key:?} holds line {} with an id prefix of 0, and prefixes are \
                 catalogue_order + 1 under document_id_scheme_version \
                 {DOCUMENT_ID_SCHEME_VERSION}",
                lines[0]
            );
        }
        if let Some(other) = owner.insert(high, book_key.as_str()) {
            anyhow::bail!(
                "books {other:?} and {book_key:?} share the id prefix {high}: their lines                  interleave, and neither book's order survives"
            );
        }
        for line_id in lines {
            if (line_id >> 32) as u32 != high {
                anyhow::bail!(
                    "book {book_key:?} holds line {line_id}, whose id prefix is not {high}:                      these ids were not composed by document_id_scheme_version                      {DOCUMENT_ID_SCHEME_VERSION}"
                );
            }
            if *line_id as u32 == 0 {
                anyhow::bail!(
                    "line {line_id} in {book_key:?} sits at position 0, and positions are                      1-based under document_id_scheme_version {DOCUMENT_ID_SCHEME_VERSION}"
                );
            }
        }
    }
    Ok(())
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

    /// **Every field of a line reaches the digest**, and a field added to `CorpusLine`
    /// reaches it without anyone remembering to add it here.
    ///
    /// Driven off the *serialized* line rather than a hand-written list, for the same reason
    /// the identity fields are: a list can be forgotten. On a device there is no join
    /// against Tantivy — the installation compares `CorpusIdentity` and then reads vectors —
    /// so a field left out of this digest is a field an index can change under a shipped
    /// artifact with nothing anywhere noticing. `section_id` decides what a short line
    /// embeds; `facets` and `is_pdf` are filtered on before hydration; `line_hash` and
    /// `section_id` are what grouping collapses on.
    #[test]
    fn every_field_of_a_line_reaches_the_digest() {
        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        let corpus = corpus(&engine);
        let line_id = corpus.book_line_ids(GENESIS).unwrap()[0];
        let line = corpus.line(line_id).unwrap().unwrap();

        let digest_of = |line: &CorpusLine| {
            let mut hasher = Sha256::new();
            feed_line(&mut hasher, line_id, line).unwrap();
            format!("{:x}", hasher.finalize())
        };
        let baseline = digest_of(&line);
        assert_eq!(
            baseline,
            digest_of(&line),
            "the digest is a function of the line"
        );

        let serialized: serde_json::Map<String, serde_json::Value> =
            match serde_json::to_value(&line).unwrap() {
                serde_json::Value::Object(map) => map,
                other => panic!("a corpus line serializes to an object, got {other:?}"),
            };
        assert!(!serialized.is_empty());

        for (field, value) in &serialized {
            let mut changed = serialized.clone();
            changed.insert(field.clone(), disturb(value));
            let changed: CorpusLine =
                serde_json::from_value(serde_json::Value::Object(changed)).unwrap();
            assert_ne!(
                digest_of(&changed),
                baseline,
                "changing {field} must change corpus_id: nothing on a device would catch it"
            );
        }

        // The id itself, which is not part of the serialized line.
        let mut hasher = Sha256::new();
        feed_line(&mut hasher, line_id + 1, &line).unwrap();
        assert_ne!(format!("{:x}", hasher.finalize()), baseline);
    }

    /// Produce a different value of the same JSON type.
    fn disturb(value: &serde_json::Value) -> serde_json::Value {
        use serde_json::Value;
        match value {
            Value::String(text) => Value::String(format!("{text}!")),
            Value::Bool(flag) => Value::Bool(!flag),
            Value::Number(number) => {
                Value::Number(serde_json::Number::from(number.as_u64().unwrap_or(0) + 1))
            }
            Value::Array(items) => {
                let mut items = items.clone();
                items.push(Value::String("/disturbed".to_string()));
                Value::Array(items)
            }
            other => panic!("no disturbance defined for {other:?}"),
        }
    }

    /// `corpus_id` is a property of the corpus, not of a run: the same library built twice
    /// produces the same value, and a word changed in one line does not.
    #[test]
    fn the_corpus_id_is_reproducible_and_moves_with_the_documents() {
        let first = TempDir::new().unwrap();
        let baseline = corpus(&engine_with_books(&first))
            .identity()
            .unwrap()
            .corpus_id;

        let repeat = TempDir::new().unwrap();
        assert_eq!(
            corpus(&engine_with_books(&repeat))
                .identity()
                .unwrap()
                .corpus_id,
            baseline
        );

        // Through the index this time rather than through a synthesized line: a facet added
        // to a book changes what the sidecar filters on, and must invalidate its vectors.
        let refaceted = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(refaceted.path().to_str().unwrap());
        engine
            .add_text_book(
                "בראשית".to_string(),
                "/מקרא/תורה".to_string(),
                GENESIS.to_string(),
                0,
                0,
                GENESIS_TEXT.to_string(),
                Some(vec!["/era/תנך".to_string(), "/author/משה".to_string()]),
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

    /// A document the index counts but cannot describe must stop the build.
    ///
    /// Written straight to Tantivy, past the engine's own document builder, because that is
    /// the only way this shape occurs: a schema change, a partially-written segment, or an
    /// index some other tool wrote. It is counted by `num_docs`, so the enumeration
    /// cross-check agrees with it — and every defaulted field would then be a real value in
    /// a real artifact, verified by the packer against the same fabrication and filtered on
    /// by the application.
    #[test]
    fn a_document_missing_a_required_field_is_refused_rather_than_defaulted() {
        use tantivy::{Index, TantivyDocument};

        let dir = TempDir::new().unwrap();
        let engine = engine_with_books(&dir);
        // The next position in Berachot, so the id scheme check still passes.
        let maimed_id = corpus(&engine).book_line_ids(BERACHOT).unwrap()[0] + 1;
        drop(engine);

        // Reopened directly. The tokenizers this schema names are registered on the
        // engine's own `Index`, so a bare reopen has to re-register something under those
        // names before it can write; what it tokenizes with is irrelevant here.
        let index = Index::open_in_dir(dir.path()).unwrap();
        for name in ["hebrew", "hebrew_vocalized"] {
            index.tokenizers().register(
                name,
                tantivy::tokenizer::TextAnalyzer::from(
                    tantivy::tokenizer::SimpleTokenizer::default(),
                ),
            );
        }
        let schema = index.schema();
        let field = |name: &str| schema.get_field(name).unwrap();

        let mut writer = index.writer(50_000_000).unwrap();
        let mut maimed = TantivyDocument::new();
        maimed.add_text(field("text"), "שורה ארוכה דיה כדי לעמוד בפני עצמה");
        maimed.add_text(field("reference"), "ref");
        maimed.add_text(field("title"), "משנה ברכות");
        maimed.add_text(field("filePath"), BERACHOT);
        maimed.add_u64(field("id"), maimed_id);
        maimed.add_u64(field("segment"), 1);
        // Everything else present, so the refusal below can only be about the one field
        // that is not — otherwise this would pass for the wrong reason.
        for name in ["sectionId", "lineHash", "contentHash", "generationSort"] {
            maimed.add_u64(field(name), 1);
        }
        // `isPdf` deliberately absent. Defaulted it would read as `false` — a filterable
        // value the application acts on, invented here and then verified against itself.
        maimed.add_facet(
            field("topics"),
            tantivy::schema::Facet::from_text("/משנה/זרעים").unwrap(),
        );
        writer.add_document(maimed).unwrap();
        writer.commit().unwrap();
        drop(writer);

        let engine = SearchEngine::new(dir.path().to_str().unwrap());
        // Refused at `open`, not at the line: deriving `corpus_id` reads every document
        // through the same strict reader, so a corpus that cannot describe itself never
        // becomes one a build can start from. The document is counted by `num_docs`, so
        // the enumeration cross-check agrees with it and would never have seen it.
        let error = expect_refusal(&engine, "a document with no isPdf must be refused");
        assert!(
            error.contains("isPdf") && error.contains(&maimed_id.to_string()),
            "the refusal names the field and the line: {error}"
        );
    }

    /// A columnar field is multi-valued underneath whatever the schema suggests, so
    /// "the first value" is a choice storage makes, not one anybody wrote down.
    ///
    /// Two `id`s file a line under one number while it describes another; two `sectionId`s
    /// pick which section a short line borrows context from, and which bucket the
    /// application groups it into. Neither is visible afterwards.
    #[test]
    fn a_document_with_two_values_in_a_single_valued_field_is_refused() {
        use tantivy::{Index, TantivyDocument};

        for (field_name, expected) in [("id", "id"), ("sectionId", "sectionId")] {
            let dir = TempDir::new().unwrap();
            let engine = engine_with_books(&dir);
            let next_id = corpus(&engine).book_line_ids(BERACHOT).unwrap()[0] + 1;
            drop(engine);

            let index = Index::open_in_dir(dir.path()).unwrap();
            for name in ["hebrew", "hebrew_vocalized"] {
                index.tokenizers().register(
                    name,
                    tantivy::tokenizer::TextAnalyzer::from(
                        tantivy::tokenizer::SimpleTokenizer::default(),
                    ),
                );
            }
            let schema = index.schema();
            let field = |name: &str| schema.get_field(name).unwrap();

            let mut writer = index.writer(50_000_000).unwrap();
            let mut doubled = TantivyDocument::new();
            doubled.add_text(field("text"), "שורה ארוכה דיה כדי לעמוד בפני עצמה");
            doubled.add_text(field("reference"), "ref");
            doubled.add_text(field("title"), "משנה ברכות");
            doubled.add_text(field("filePath"), BERACHOT);
            doubled.add_bool(field("isPdf"), false);
            doubled.add_u64(field("id"), next_id);
            doubled.add_u64(field("segment"), 1);
            for name in ["sectionId", "lineHash", "contentHash", "generationSort"] {
                doubled.add_u64(field(name), 1);
            }
            // The second value, on the field under test.
            doubled.add_u64(field(field_name), 99);
            doubled.add_facet(
                field("topics"),
                tantivy::schema::Facet::from_text("/משנה/זרעים").unwrap(),
            );
            writer.add_document(doubled).unwrap();
            writer.commit().unwrap();
            drop(writer);

            let engine = SearchEngine::new(dir.path().to_str().unwrap());
            let error = expect_refusal(&engine, "a doubled field must be refused");
            assert!(
                error.contains("more than one") && error.contains(expected),
                "the refusal names the field: {error}"
            );
        }
    }

    /// The one catalogue position that cannot form an id is refused where the id is formed.
    ///
    /// `(u32::MAX + 1) << 32` overflows a `u64` and wraps to a base of zero in release —
    /// which the corpus would then refuse as "no catalogue prefix", for a whole index,
    /// blaming the reader for something the writer did.
    #[test]
    fn the_last_catalogue_position_is_refused_where_the_id_is_composed() {
        let dir = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        let refused = engine.add_text_book(
            "ספר אחרון".to_string(),
            "/מקרא".to_string(),
            GENESIS.to_string(),
            u32::MAX,
            0,
            "שורה ארוכה דיה לעמוד בפני עצמה".to_string(),
            None,
        );
        let error = refused.expect_err("the last catalogue position cannot form an id");
        assert!(format!("{error}").contains("overflows u64"), "{error}");
    }

    /// Scheme 1 composes the high half as `catalogue_order + 1`, so a prefix of zero is not
    /// a catalogue position — it is what a bare ordinal looks like when nothing composed
    /// it. `line_id = 1` would otherwise be accepted and stamped as scheme 1.
    #[test]
    fn an_id_with_no_catalogue_prefix_is_refused() {
        let dir = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(dir.path().to_str().unwrap());
        engine
            .add_document(
                1,
                "t",
                "r",
                "/root",
                "שורה ארוכה דיה לעמוד בפני עצמה",
                0,
                false,
                GENESIS,
                None,
                None,
                None,
            )
            .unwrap();
        engine.commit().unwrap();

        let error = expect_refusal(&engine, "an id with no catalogue prefix must be refused");
        assert!(error.contains("id prefix of 0"), "{error}");
    }

    /// Open a corpus expecting a refusal. `TantivyCorpus` has no `Debug`, and giving it
    /// one just so `expect_err` compiles would put a `Searcher` and a book map in a panic
    /// message.
    fn expect_refusal(engine: &SearchEngine, what: &str) -> String {
        match TantivyCorpus::from_engine(engine, "v", ChunkerConfig::default()) {
            Err(error) => format!("{error}"),
            Ok(_) => panic!("{what}"),
        }
    }

    /// The ordering this adapter performs is only correct because the ids encode a
    /// position — and the public `add_document` accepts any `u64` at all.
    ///
    /// Without this check a caller could hand the engine ids from another scheme (or none),
    /// and a build would sort a book's lines into an order nobody chose, hand the chunker
    /// neighbours that never surrounded a line, and still stamp
    /// `document_id_scheme_version = 1` on the artifact.
    #[test]
    fn ids_that_do_not_encode_a_position_are_refused() {
        // Two books sharing a prefix: their lines interleave, and neither book's order
        // survives the sort.
        let shared = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(shared.path().to_str().unwrap());
        for (id, path) in [(1u64 << 32 | 1, GENESIS), (1u64 << 32 | 2, BERACHOT)] {
            engine
                .add_document(
                    id,
                    "t",
                    "r",
                    "/root",
                    "שורה ארוכה דיה לעמוד בפני עצמה",
                    0,
                    false,
                    path,
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        engine.commit().unwrap();
        let error = expect_refusal(&engine, "two books cannot share an id prefix");
        assert!(error.contains("share the id prefix"), "{error}");

        // One book whose lines come from two prefixes: the low halves are no longer
        // positions within one book.
        let mixed = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(mixed.path().to_str().unwrap());
        for id in [1u64 << 32 | 1, 2u64 << 32 | 1] {
            engine
                .add_document(
                    id,
                    "t",
                    "r",
                    "/root",
                    "שורה ארוכה דיה לעמוד בפני עצמה",
                    0,
                    false,
                    GENESIS,
                    None,
                    None,
                    None,
                )
                .unwrap();
        }
        engine.commit().unwrap();
        let error = expect_refusal(&engine, "a book's ids must share one prefix");
        assert!(error.contains("id prefix is not"), "{error}");

        // Position zero: ids are 1-based, so a zero low half is not a position at all.
        let zero = TempDir::new().unwrap();
        let mut engine = SearchEngine::new(zero.path().to_str().unwrap());
        engine
            .add_document(
                1u64 << 32,
                "t",
                "r",
                "/root",
                "שורה ארוכה דיה לעמוד בפני עצמה",
                0,
                false,
                GENESIS,
                None,
                None,
                None,
            )
            .unwrap();
        engine.commit().unwrap();
        let error = expect_refusal(&engine, "position 0 does not exist under scheme 1");
        assert!(error.contains("position 0"), "{error}");
    }

    /// A gap left by a deleted line is not a broken id scheme.
    ///
    /// Contiguity is deliberately not required: a corpus a book was deleted from is an
    /// ordinary corpus, and demanding `1..=n` would refuse it.
    #[test]
    fn a_gap_left_by_a_deletion_is_still_a_valid_id_scheme() {
        let dir = TempDir::new().unwrap();
        let mut engine = engine_with_books(&dir);
        let middle = corpus(&engine).book_line_ids(GENESIS).unwrap()[1];
        engine.delete_document_by_id(middle).unwrap();
        engine.commit().unwrap();

        let corpus = corpus(&engine);
        assert_eq!(corpus.line_count(), 4);
        assert!(!corpus.book_line_ids(GENESIS).unwrap().contains(&middle));
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
