//! The plan of a vector build, from the release index: the sidecar's plan files, written
//! through its plan API.
//!
//! The sidecar defines the plan (`otzaria_semantic_search::distribution::plan`) and leaves
//! the planner over the release index to this crate, which has the index: `records.bin`,
//! which line of which book holds which text; `books.json`, the books by key;
//! `embed.jsonl` and its manifest, the texts that still need a vector; `tombstones.bin`,
//! the keys a previous release held and this one does not; and `plan-manifest.json`, which
//! hashes them all. Every file is the sidecar's writer's, so nothing here is a format.
//!
//! The text of a line is what the index stores, chunked under the recipe compiled into this
//! build — the one its `chunkKey` column is written under — so a plan describes the index
//! the application opens. A version 4 index has no column, and its keys are only ever
//! computed from that text. A version 5 index's column is held to the same computation, line
//! by line: the key's first eight bytes for a line the recipe embeds, `0` for one it does
//! not. That is the plan manifest's parity gate, and a plan that fails it is refused by
//! every reader of plans.
//!
//! A PDF's lines are not planned. No vector is built from them: the index keys them `0`,
//! and a device never resolves a hit to one.

use crate::api::search_engine::LINE_TEXT_VERSION;
use crate::semantic_keys::production_chunking;
use anyhow::{Context, Result};
use otzaria_semantic_search::distribution::ledger::{split, Ledger};
use otzaria_semantic_search::distribution::plan::{
    BookList, EmbedManifest, EmbedPlanWriter, HeldVectors, Parity, PlanManifest, PlanRecord,
    PlanRecords, RecordsWriter, BOOKS_FILE, RECORDS_FILE, TOMBSTONES_FILE,
};
use otzaria_semantic_search::semantic::chunk_key::LineRef;
use otzaria_semantic_search::semantic::chunker::Chunker;
use otzaria_semantic_search::semantic::official_index::readable_store_identity;
use otzaria_semantic_search::semantic::recipe::EmbeddingRecipe;
use otzaria_semantic_search::semantic::versioning::{
    IndexVersion, ModelIdentity, ModelPackage, TextIdentity,
};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use tantivy::columnar::Column;
use tantivy::schema::Value;
use tantivy::{DocAddress, Index, ReloadPolicy, Searcher, TantivyDocument};

/// What [`export_plan`] plans, and against what.
pub struct PlanExport<'a> {
    /// The release index, opened read-only.
    pub index_path: PathBuf,
    pub out_dir: PathBuf,
    /// The library edition the index holds: its `db_version`.
    pub library_version: u32,
    pub library_release_tag: String,
    /// The family the vectors are for. Its `chunking_identity` must be this build's.
    pub model: ModelIdentity,
    /// The package the passages are embedded with: one of `model.query_packages`.
    pub passage_package: ModelPackage,
    /// The release before this one, to split against.
    pub previous: Option<&'a Ledger>,
    /// The vectors embedded already, whose texts `embed.jsonl` leaves out.
    pub warehouse: Option<&'a dyn HeldVectors>,
    pub created_at: String,
}

/// What [`export_plan`] wrote.
pub struct PlanExportReport {
    pub manifest: PlanManifest,
    pub embed: EmbedManifest,
    /// Whether the keys were held to a `chunkKey` column: `false` for an index without one
    /// this build uses, whose parity checks nothing.
    pub column_checked: bool,
    /// Live documents in the index, and the PDF lines among them that were not planned.
    pub documents: u64,
    pub pdf_lines: u64,
}

/// Books read and keyed at once: enough to keep every core busy, few enough that their
/// texts stay small.
const BOOKS_PER_BATCH: usize = 64;

/// Write the plan of the index at `request.index_path` into `request.out_dir`.
///
/// The plan is written even when the parity gate fails, so the counts can be read; the
/// report's `manifest.parity` says so, and the sidecar refuses to read such a plan.
pub fn export_plan(request: PlanExport<'_>) -> Result<PlanExportReport> {
    // The cheap refusals first: nothing below is worth reading six million lines for if
    // the manifest at the end could not be written.
    let chunking = production_chunking();
    EmbeddingRecipe::resolve(&chunking, &request.model)?;
    anyhow::ensure!(
        request.model.chunking_identity == chunking.identity(),
        "the model's chunking_identity is {}, and this build keys lines under chunking {}",
        request.model.chunking_identity,
        chunking.identity()
    );
    anyhow::ensure!(
        request
            .model
            .query_packages
            .contains(&request.passage_package),
        "the passage package {} is not one of the family's query packages",
        request.passage_package.checksum
    );
    anyhow::ensure!(
        request.library_release_tag.len() <= 64
            && !request.library_release_tag.chars().any(char::is_control),
        "the release tag {:?} is longer than 64 bytes or holds a control character",
        request.library_release_tag
    );
    let identity = IndexVersion {
        text: TextIdentity::with_line_text_version(LINE_TEXT_VERSION),
        model: request.model.clone(),
        store: readable_store_identity(),
    };
    identity.validate_complete()?;

    let (searcher, chunk_key) = open_index(&request.index_path)?;
    let columns = SegmentColumns::of(&searcher, chunk_key.is_some())?;
    let books = books_of(&searcher)?;
    let documents: u64 = books.values().map(|lines| lines.len() as u64).sum();

    let dir = &request.out_dir;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let book_list = BookList::new(books.keys().cloned().collect())?;
    book_list.write(&dir.join(BOOKS_FILE))?;

    let chunker = Chunker::new(chunking.clone())?;
    let mut records = RecordsWriter::create(&dir.join(RECORDS_FILE))?;
    let mut embed = EmbedPlanWriter::create(dir, request.warehouse)?;
    let mut parity = Parity::default();
    let mut pdf_lines = 0u64;
    let names: Vec<&String> = books.keys().collect();
    for (batch, chunk) in names.chunks(BOOKS_PER_BATCH).enumerate() {
        let planned = chunk
            .par_iter()
            .map(|name| plan_book(&searcher, &columns, &chunker, &books[*name]))
            .collect::<Result<Vec<_>>>()?;
        for (offset, lines) in planned.into_iter().enumerate() {
            let book = (batch * BOOKS_PER_BATCH + offset) as u32;
            for (ordinal, line) in lines.into_iter().enumerate() {
                if let Some(column) = line.column {
                    parity.checked += 1;
                    let expected = line
                        .text
                        .as_ref()
                        .map_or(0, |(sha256, _)| column_value(sha256));
                    if column != expected {
                        parity.mismatches += 1;
                    }
                }
                if line.pdf {
                    pdf_lines += 1;
                    continue;
                }
                let Some((sha256, text)) = line.text else {
                    continue;
                };
                records.push(PlanRecord {
                    book,
                    ordinal: ordinal as u32,
                    sha256,
                })?;
                embed.offer(&sha256, &text)?;
            }
        }
        if (batch + 1) % 50 == 0 {
            log::info!(
                "planned {} of {} book(s)",
                (batch + 1) * BOOKS_PER_BATCH,
                names.len()
            );
        }
    }
    let record_count = records.finish()?;
    let to_embed = embed.records();
    let embed = embed.finish(
        &request.model,
        chunking.identity(),
        &request.passage_package,
    )?;
    anyhow::ensure!(
        record_count > 0,
        "the recipe embeds no line of the {} book(s) in the index",
        book_list.len()
    );

    let plan_records = PlanRecords::open(&dir.join(RECORDS_FILE))?;
    let mut counts = split(
        &plan_records,
        &book_list,
        request.previous,
        request.warehouse,
        &dir.join(TOMBSTONES_FILE),
    )?
    .counts;
    counts.to_embed = to_embed;
    let manifest = PlanManifest::write(
        dir,
        identity,
        request.library_version,
        request.library_release_tag,
        request.previous.map(Ledger::as_previous),
        counts,
        parity,
        request.created_at,
    )?;
    Ok(PlanExportReport {
        manifest,
        embed,
        column_checked: chunk_key.is_some(),
        documents,
        pdf_lines,
    })
}

/// How a vector set's records land on a live index: see [`validate`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Validation {
    /// Live lines the recipe embeds.
    pub keyed_lines: u64,
    /// Those whose key the set records in their own book.
    pub covered_lines: u64,
    /// The set's records: in the index's books, and in books the index does not hold.
    pub records: u64,
    /// Records whose hint is a line that holds their key: resolved at the first look.
    pub at_hint: u64,
    /// Records whose key their book holds at another line: re-anchored by the resolver.
    pub moved: u64,
    /// Records whose key their book holds nowhere any more, or whose book the index does not
    /// hold at all.
    pub gone: u64,
    /// Books the set records texts in that the index does not hold.
    pub books_missing: u64,
    /// Whether the index has a `chunkKey` column this build uses, which is what a device
    /// resolves records by, and so whether it was held to the lines' text.
    pub column_checked: bool,
    /// Lines whose `chunkKey` column is not their text's key's first 64 bits, `0` for a line
    /// the recipe does not embed: a device would not resolve a record to them, or would find
    /// another text under their key.
    pub column_mismatches: u64,
}

/// How the set's records land on the index at `index_path`, read-only: every book's lines
/// keyed from their text, as [`export_plan`] keys them, against the records a scan of `set`
/// reaches for that book; the records of books the index does not hold; and, with the
/// `chunkKey` column, every line's column against its text's key. A set built from this
/// index has every record at its hint, covers every keyed line and names no missing book,
/// and an index built by this build has no column mismatch.
pub fn validate(
    index_path: &Path,
    set: &otzaria_semantic_search::semantic::segment_set::SegmentSet,
) -> Result<Validation> {
    use otzaria_semantic_search::distribution::gates::book_records;
    use otzaria_semantic_search::semantic::oxv::reader::Segment;
    use std::collections::{BTreeSet, HashSet};
    let (searcher, chunk_key) = open_index(index_path)?;
    let columns = SegmentColumns::of(&searcher, chunk_key.is_some())?;
    let books = books_of(&searcher)?;
    let chunker = Chunker::new(production_chunking())?;
    let names: Vec<&String> = books.keys().collect();
    let mut validation = Validation {
        column_checked: chunk_key.is_some(),
        ..Validation::default()
    };
    let mut records = Vec::new();
    for chunk in names.chunks(BOOKS_PER_BATCH) {
        let keyed = chunk
            .par_iter()
            .map(|name| {
                let mut mismatches = 0u64;
                let keys = plan_book(&searcher, &columns, &chunker, &books[*name])?
                    .into_iter()
                    .map(|line| {
                        let key = match &line.text {
                            Some((sha256, _)) if !line.pdf => {
                                Some(<[u8; 16]>::try_from(&sha256[..16]).expect("16 of 32"))
                            }
                            _ => None,
                        };
                        if let Some(column) = line.column {
                            let expected = line
                                .text
                                .as_ref()
                                .map_or(0, |(sha256, _)| column_value(sha256));
                            mismatches += u64::from(column != expected);
                        }
                        key
                    })
                    .collect::<Vec<_>>();
                Ok((keys, mismatches))
            })
            .collect::<Result<Vec<_>>>()?;
        for (name, (keys, mismatches)) in chunk.iter().zip(keyed) {
            validation.column_mismatches += mismatches;
            book_records(set, name, &mut records);
            let held: HashSet<[u8; 16]> = records.iter().map(|(key, _)| key.0).collect();
            let present: HashSet<[u8; 16]> = keys.iter().flatten().copied().collect();
            for key in keys.iter().flatten() {
                validation.keyed_lines += 1;
                validation.covered_lines += u64::from(held.contains(key));
            }
            for (key, hint) in &records {
                validation.records += 1;
                if keys.get(*hint as usize).copied().flatten() == Some(key.0) {
                    validation.at_hint += 1;
                } else if present.contains(&key.0) {
                    validation.moved += 1;
                } else {
                    validation.gone += 1;
                }
            }
        }
    }

    // The books the set records texts in, from its segments' book tables: those the index
    // does not hold resolve nowhere.
    let mut set_books = BTreeSet::new();
    for segment in &set.info().segments {
        let path = set
            .dir()
            .join("segments")
            .join(format!("{}.oxv", segment.id));
        let opened = Segment::open(&path).with_context(|| format!("opening {}", path.display()))?;
        set_books.extend(opened.books().iter().map(|book| book.name.to_string()));
    }
    for name in set_books.iter().filter(|name| !books.contains_key(*name)) {
        book_records(set, name, &mut records);
        if !records.is_empty() {
            validation.books_missing += 1;
            validation.records += records.len() as u64;
            validation.gone += records.len() as u64;
        }
    }
    Ok(validation)
}

/// What a vector set must declare to be opened by this build with `model`: the line recipe
/// this build indexes with, the model, and the store this build reads — what
/// `open_semantic_artifact` holds a set to, with the one package it loaded among `model`'s
/// query packages.
pub fn installation_identity(model: ModelIdentity) -> IndexVersion {
    IndexVersion {
        text: TextIdentity::with_line_text_version(LINE_TEXT_VERSION),
        model,
        store: readable_store_identity(),
    }
}

/// `count` queries drawn from the index at `index_path`, the same ones whenever the index is
/// the same: spans of three to ten consecutive words of its lines, PDF pages aside, at lines
/// chosen with a fixed seed over every book's lines in order. What `validate_semantic_vectors`
/// measures retrieval with when it is handed no queries; fewer only for an index with fewer
/// lines of three words than the draw finds.
pub fn sample_queries(index_path: &Path, count: usize) -> Result<Vec<String>> {
    let (searcher, _) = open_index(index_path)?;
    let books = books_of(&searcher)?;
    let lines: Vec<DocAddress> = books.into_values().flatten().collect();
    anyhow::ensure!(
        !lines.is_empty(),
        "the index at {} holds no line",
        index_path.display()
    );
    let schema = searcher.schema();
    let text_field = schema.get_field("text")?;
    let is_pdf_field = schema.get_field("isPdf")?;
    let mut state: u64 = 0x005E_ED0F_0E1A_7CA5;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut queries = Vec::with_capacity(count);
    let mut draws = 0usize;
    while queries.len() < count && draws < count.saturating_mul(50).max(50) {
        draws += 1;
        let address = lines[(next() % lines.len() as u64) as usize];
        let document: TantivyDocument = searcher
            .doc(address)
            .with_context(|| format!("reading the document at {address:?}"))?;
        if document
            .get_first(is_pdf_field)
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let words: Vec<&str> = document
            .get_first(text_field)
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .split_whitespace()
            .collect();
        if words.len() < 3 {
            continue;
        }
        let len = (3 + (next() % 8) as usize).min(words.len());
        let start = (next() % (words.len() - len + 1) as u64) as usize;
        queries.push(words[start..start + len].join(" "));
    }
    Ok(queries)
}

/// The index at `index_path`, read-only, and its `chunkKey` column when this build uses it.
fn open_index(index_path: &Path) -> Result<(Searcher, Option<tantivy::schema::Field>)> {
    let compatibility =
        crate::api::search_engine::check_index_compatibility(index_path.display().to_string());
    crate::semantic_corpus::ensure_compatible(&compatibility)?;
    // `open_in_dir`, never `open_or_create`, and no writer: a path that holds no index is
    // an error, and nothing here writes.
    let index = Index::open_in_dir(index_path)
        .with_context(|| format!("opening the index at {}", index_path.display()))?;
    let searcher = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .with_context(|| format!("reading the index at {}", index_path.display()))?
        .searcher();
    let chunk_key = crate::api::search_engine::live_chunk_key_field(searcher.schema(), index_path);
    Ok((searcher, chunk_key))
}

/// What one line of a book plans to.
struct PlannedLine {
    /// The SHA-256 of the text the recipe embeds for it, and the text; `None` when it
    /// embeds none.
    text: Option<([u8; 32], String)>,
    pdf: bool,
    /// The `chunkKey` column's value, when the index has a column this build uses.
    column: Option<u64>,
}

/// The `chunkKey` column value of a text whose SHA-256 is `sha256`: the key's first eight
/// bytes, big-endian.
fn column_value(sha256: &[u8; 32]) -> u64 {
    u64::from_be_bytes(sha256[..8].try_into().expect("8 of 32"))
}

/// The columns a book's lines are read from, per segment.
struct SegmentColumns {
    section: Vec<Column<u64>>,
    chunk_key: Vec<Option<Column<u64>>>,
}

impl SegmentColumns {
    fn of(searcher: &Searcher, chunk_key: bool) -> Result<Self> {
        let mut section = Vec::new();
        let mut keys = Vec::new();
        for reader in searcher.segment_readers() {
            section.push(reader.fast_fields().u64("sectionId")?);
            keys.push(if chunk_key {
                Some(reader.fast_fields().u64("chunkKey")?)
            } else {
                None
            });
        }
        Ok(Self {
            section,
            chunk_key: keys,
        })
    }
}

/// Every live document, by book, in line order: ascending `id`, which `add_text_book`
/// composes from the line's position in its book.
fn books_of(searcher: &Searcher) -> Result<BTreeMap<String, Vec<DocAddress>>> {
    let mut books: BTreeMap<String, Vec<(u64, DocAddress)>> = BTreeMap::new();
    let mut scanned = 0u64;
    let mut name = String::new();
    for (segment_ord, reader) in searcher.segment_readers().iter().enumerate() {
        let ids = reader.fast_fields().u64("id")?;
        let paths = reader
            .fast_fields()
            .str("filePath")?
            .context("the index has no filePath column")?;
        for doc in reader.doc_ids_alive() {
            scanned += 1;
            let id = ids
                .first(doc)
                .with_context(|| format!("document {doc} of segment {segment_ord} has no id"))?;
            let ord = paths.term_ords(doc).next().with_context(|| {
                format!("document {doc} of segment {segment_ord} has no filePath")
            })?;
            name.clear();
            paths.ord_to_str(ord, &mut name)?;
            books
                .entry(name.clone())
                .or_default()
                .push((id, DocAddress::new(segment_ord as u32, doc)));
        }
    }
    anyhow::ensure!(
        scanned == searcher.num_docs(),
        "the scan found {scanned} live document(s), and the index holds {}",
        searcher.num_docs()
    );
    Ok(books
        .into_iter()
        .map(|(name, mut lines)| {
            lines.sort_unstable_by_key(|(id, _)| *id);
            (
                name,
                lines.into_iter().map(|(_, address)| address).collect(),
            )
        })
        .collect())
}

/// One book's lines, read from the index and planned under `chunker`.
fn plan_book(
    searcher: &Searcher,
    columns: &SegmentColumns,
    chunker: &Chunker,
    book: &[DocAddress],
) -> Result<Vec<PlannedLine>> {
    let schema = searcher.schema();
    let text_field = schema.get_field("text")?;
    let is_pdf_field = schema.get_field("isPdf")?;
    let mut texts = Vec::with_capacity(book.len());
    let mut sections = Vec::with_capacity(book.len());
    let mut pdf = Vec::with_capacity(book.len());
    let mut stored_keys = Vec::with_capacity(book.len());
    for &address in book {
        let document: TantivyDocument = searcher
            .doc(address)
            .with_context(|| format!("reading the document at {address:?}"))?;
        texts.push(
            document
                .get_first(text_field)
                .and_then(|value| value.as_str())
                .with_context(|| format!("the document at {address:?} stores no text"))?
                .to_string(),
        );
        pdf.push(
            document
                .get_first(is_pdf_field)
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        );
        let segment = address.segment_ord as usize;
        sections.push(
            columns.section[segment]
                .first(address.doc_id)
                .with_context(|| format!("the document at {address:?} has no sectionId"))?,
        );
        stored_keys.push(
            columns.chunk_key[segment]
                .as_ref()
                .map(|column| column.first(address.doc_id).unwrap_or(0)),
        );
    }
    let lines: Vec<LineRef<'_>> = texts
        .iter()
        .zip(&sections)
        .map(|(text, &section)| LineRef { text, section })
        .collect();
    Ok((0..book.len())
        .map(|index| PlannedLine {
            text: (!pdf[index])
                .then(|| chunker.embedded_text(&lines, index))
                .flatten()
                .map(|text| (Sha256::digest(text.as_bytes()).into(), text)),
            pdf: pdf[index],
            column: stored_keys[index],
        })
        .collect())
}

/// Now, as a manifest records it: `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_timestamp() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);

    // Howard Hinnant's civil_from_days: exact over the proleptic Gregorian calendar.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_position + 2) / 5 + 1) as u32;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        (second_of_day % 3600) / 60,
        second_of_day % 60
    )
}
