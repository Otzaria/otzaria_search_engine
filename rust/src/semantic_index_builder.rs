//! Offline rebuild preserving the released catalogue, canonical ids and text metadata.
use crate::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookSource, LibraryBookStatus,
};
use crate::api::search_engine::{
    configure_line_source, BookOrderEntry, SearchEngine, TextStorage, FACET_DIMENSION_ROOTS,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use tantivy::schema::{Facet, TantivyDocument, Value};
use tantivy::{DocAddress, Index, ReloadPolicy, Searcher};

pub struct BuildIndexRequest {
    pub source_index: PathBuf,
    pub seforim_db: PathBuf,
    pub out: PathBuf,
    pub library_version: u32,
}
#[derive(Debug, Serialize)]
pub struct BuildIndexReport {
    pub format: &'static str,
    pub version: u32,
    pub library_version: u32,
    pub db_schema_version: u32,
    pub books: usize,
    pub source_documents: u64,
    pub rebuilt_documents: u64,
    pub source_index_metadata: serde_json::Value,
    pub rebuilt_index_metadata: serde_json::Value,
    pub source_index_meta_sha256: String,
    pub source_catalogue_sha256: String,
    pub source_book_order_sha256: String,
    pub database_sha256: String,
    pub ids_text_and_metadata_verified: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Book {
    file_path: String,
    number: u32,
    title: String,
    topics: String,
    extra_facets: Vec<String>,
    documents: u64,
    lines_sha256: String,
    stamps: [u64; 4],
}
#[derive(Default)]
struct Accum {
    book: Option<Book>,
    lines: Vec<(u64, [u8; 32])>,
}
#[derive(Deserialize)]
struct OrderFile {
    revision: u64,
    books: Vec<OrderEntry>,
}
#[derive(Deserialize)]
struct OrderEntry {
    #[serde(rename = "filePath")]
    file_path: String,
    #[serde(rename = "catalogueOrder")]
    catalogue_order: u32,
    #[serde(rename = "generationOrder")]
    generation_order: u32,
}
fn digest_file(path: &Path) -> Result<String> {
    let mut input =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1 << 20];
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn json(path: &Path) -> Result<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(path)?)
        .with_context(|| format!("reading {}", path.display()))
}
fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

/// Sorted per-line digests verify ids, sections, ordinals, references and lexical
/// hashes. Book fingerprints verify raw text, including short unhashable lines.
fn catalogue(searcher: &Searcher) -> Result<BTreeMap<String, Book>> {
    let schema = searcher.schema();
    let path_f = schema.get_field("filePath")?;
    let title_f = schema.get_field("title")?;
    let segment_f = schema.get_field("segment")?;
    let reference_f = schema.get_field("reference")?;
    let pdf_f = schema.get_field("isPdf")?;
    let mut books: BTreeMap<String, Accum> = BTreeMap::new();
    let mut seen_numbers = BTreeMap::<u32, String>::new();
    for (segment_ord, reader) in searcher.segment_readers().iter().enumerate() {
        let ids = reader.fast_fields().u64("id")?;
        let hashes = reader.fast_fields().u64("lineHash")?;
        let sections = crate::derived_columns::SectionIdColumn::open(reader)?;
        let stamps = ["contentHash", "textHash", "metadataHash", "sourceHash"]
            .map(|name| reader.fast_fields().u64(name));
        let stamps = stamps
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let facets = reader.facet_reader("topics")?;
        for doc in 0..reader.max_doc() {
            if reader.is_deleted(doc) {
                continue;
            }
            let id = ids.first(doc).context("document has no id")?;
            let number = u32::try_from(id >> 32)?;
            anyhow::ensure!(
                number > 0 && number < u32::MAX && id as u32 > 0,
                "invalid canonical document id {id}"
            );
            let stored: TantivyDocument = crate::doc_store::load_document(
                searcher,
                DocAddress::new(segment_ord as u32, doc),
            )?;
            let text = |f| {
                stored
                    .get_first(f)
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
                    .context("missing stored text metadata")
            };
            let path = text(path_f)?;
            anyhow::ensure!(
                path.strip_prefix("id:")
                    .and_then(|s| s.parse::<i64>().ok())
                    .is_some_and(|id| id > 0),
                "source contains a non-library book {path:?}"
            );
            anyhow::ensure!(
                stored.get_first(pdf_f).and_then(|v| v.as_bool()) == Some(false),
                "cannot rebuild PDF book {path:?} from library rows"
            );
            if let Some(held) = seen_numbers.insert(number, path.clone()) {
                anyhow::ensure!(
                    held == path,
                    "book number {number} belongs to both {held:?} and {path:?}"
                );
            }
            let mut all_facets = Vec::new();
            let mut facet = Facet::default();
            for ord in facets.facet_ords(doc) {
                facets.facet_from_ord(ord, &mut facet)?;
                all_facets.push(facet.to_string());
            }
            all_facets.sort();
            all_facets.dedup();
            let (extra, topics): (Vec<_>, Vec<_>) = all_facets.into_iter().partition(|path| {
                path.trim_start_matches('/')
                    .split('/')
                    .next()
                    .is_some_and(|root| FACET_DIMENSION_ROOTS.contains(&root))
            });
            anyhow::ensure!(
                topics.len() == 1,
                "book {path:?} has ambiguous catalogue facets {topics:?}"
            );
            let title = text(title_f)?;
            let acc = books.entry(path.clone()).or_default();
            let book = acc.book.get_or_insert_with(|| Book {
                file_path: path.clone(),
                number,
                title: title.clone(),
                topics: topics[0].clone(),
                extra_facets: extra.clone(),
                documents: 0,
                lines_sha256: String::new(),
                stamps: [0; 4],
            });
            anyhow::ensure!(
                book.number == number
                    && book.title == title
                    && book.topics == topics[0]
                    && book.extra_facets == extra,
                "inconsistent canonical book metadata for {path:?}"
            );
            for (at, column) in stamps.iter().enumerate() {
                if let Some(value) = column.first(doc).filter(|v| *v != 0) {
                    anyhow::ensure!(
                        book.stamps[at] == 0 || book.stamps[at] == value,
                        "inconsistent book stamp {at} for {path:?}"
                    );
                    book.stamps[at] = value;
                }
            }
            let ordinal = stored
                .get_first(segment_f)
                .and_then(|v| v.as_u64())
                .context("missing line ordinal")?;
            anyhow::ensure!(
                id as u32 as u64 == ordinal + 1,
                "document {id} disagrees with canonical ordinal {ordinal}"
            );
            let section = sections.first(doc).context("missing sectionId")?;
            let mut line = Sha256::new();
            for value in [
                id,
                ordinal,
                section,
                hashes.first(doc).context("missing lineHash")?,
            ] {
                line.update(value.to_le_bytes());
            }
            frame(&mut line, text(reference_f)?.as_bytes());
            acc.lines.push((id, line.finalize().into()));
        }
    }
    anyhow::ensure!(!books.is_empty(), "source index has no live library books");
    books
        .into_iter()
        .map(|(key, mut acc)| {
            acc.lines.sort_unstable_by_key(|(id, _)| *id);
            let mut hash = Sha256::new();
            for (ordinal, (id, digest)) in acc.lines.iter().enumerate() {
                anyhow::ensure!(
                    *id as u32 as usize == ordinal + 1,
                    "book {key:?} has duplicate or missing canonical line ids"
                );
                hash.update(id.to_le_bytes());
                hash.update(digest);
            }
            let mut book = acc.book.unwrap();
            book.documents = acc.lines.len() as u64;
            book.lines_sha256 = format!("{:x}", hash.finalize());
            anyhow::ensure!(
                book.stamps[0] != 0 && book.stamps[1] != 0 && book.stamps[2] != 0,
                "book {key:?} lacks raw-text/metadata fingerprints needed for a verified rebuild"
            );
            Ok((key, book))
        })
        .collect()
}

pub fn build(request: BuildIndexRequest) -> Result<BuildIndexReport> {
    anyhow::ensure!(
        std::fs::symlink_metadata(&request.out)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "output already exists or cannot be inspected: {}",
        request.out.display()
    );
    let source_path = request.source_index.canonicalize()?;
    let parent = request
        .out
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .canonicalize()?;
    anyhow::ensure!(
        !parent.starts_with(&source_path),
        "output must be outside the source index"
    );
    let db = Connection::open_with_flags(
        &request.seforim_db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.execute_batch("PRAGMA query_only=ON")?;
    let db_version: u32 = db
        .query_row(
            "SELECT value FROM schema_meta WHERE key='db_version'",
            [],
            |r| r.get::<_, String>(0),
        )?
        .parse()?;
    let db_schema: u32 = db
        .query_row(
            "SELECT value FROM schema_meta WHERE key='db_schema_version'",
            [],
            |r| r.get::<_, String>(0),
        )?
        .parse()?;
    anyhow::ensure!(
        db_version == request.library_version,
        "DB version {db_version} differs from requested {}",
        request.library_version
    );
    anyhow::ensure!(
        db_schema == 7,
        "unsupported library DB schema {db_schema}; this rebuild requires schema 7"
    );
    let source_metadata = json(&source_path.join("otzaria_index_meta.json"))?;
    let source_meta_sha = digest_file(&source_path.join("meta.json"))?;
    let order_path = source_path.join(crate::book_order::BOOK_ORDER_FILE);
    let order_bytes = std::fs::read(&order_path)
        .context("released index must carry authoritative book catalogue/generation order")?;
    let order: OrderFile = serde_json::from_slice(&order_bytes)?;
    let mut order_paths = HashSet::new();
    for entry in &order.books {
        anyhow::ensure!(
            order_paths.insert(&entry.file_path),
            "duplicate path in source catalogue: {:?}",
            entry.file_path
        );
    }
    let index = Index::open_in_dir(&source_path)?;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let source = reader.searcher();
    eprintln!(
        "Reading canonical ids, text stamps and metadata of {} source documents",
        source.num_docs()
    );
    let expected = catalogue(&source)?;
    let source_documents = expected.values().map(|book| book.documents).sum();
    let mut inputs = Vec::with_capacity(expected.len());
    for book in expected.values() {
        anyhow::ensure!(
            order_paths.contains(&book.file_path),
            "source catalogue has no placement for {:?}",
            book.file_path
        );
        let book_id: i64 = book.file_path.strip_prefix("id:").unwrap().parse()?;
        let (db_title, source_hash): (String, i64) = db.query_row(
            "SELECT title,contentHash FROM book WHERE id=?1",
            [book_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        anyhow::ensure!(
            book.stamps[3] == 0 || book.stamps[3] == source_hash as u64,
            "source DB hash changed for {:?}",
            book.file_path
        );
        inputs.push((
            book.number,
            LibraryBookInput {
                source: LibraryBookSource::Library,
                attached_slug: None,
                book_id,
                db_title: Some(db_title),
                title: book.title.clone(),
                topics: book.topics.clone(),
                file_path: book.file_path.clone(),
                extra_facets: Some(book.extra_facets.clone()),
                source_hash: Some(book.stamps[3]),
                text_storage: TextStorage::LibraryDb,
            },
        ));
    }
    inputs.sort_unstable_by_key(|(number, _)| *number);
    let database_sha = digest_file(&request.seforim_db)?;
    let catalogue_sha = format!("{:x}", Sha256::digest(serde_json::to_vec(&expected)?));
    std::fs::create_dir(&request.out)?;
    std::fs::write(
        request.out.join(crate::book_order::BOOK_ORDER_FILE),
        &order_bytes,
    )?;
    let numbers_path = source_path.join(crate::book_order::BOOK_NUMBERS_FILE);
    if numbers_path.exists() {
        let bytes = std::fs::read(numbers_path)?;
        let numbers: serde_json::Value = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            numbers
                .get("reserved")
                .and_then(|v| v.as_u64())
                .is_some_and(|n| n > 0 && n <= u32::MAX as u64),
            "invalid source book number reservation"
        );
        std::fs::write(
            request.out.join(crate::book_order::BOOK_NUMBERS_FILE),
            bytes,
        )?;
    }
    configure_line_source(
        request
            .seforim_db
            .to_str()
            .context("DB path is not UTF-8")?
            .to_string(),
    )?;
    let mut engine = SearchEngine::new(request.out.to_str().context("output path is not UTF-8")?);
    engine.open_index_writer()?;
    engine.seed_book_numbers_for_rebuild(&source)?;
    let revision = engine.set_book_order(
        order
            .books
            .into_iter()
            .map(|entry| BookOrderEntry {
                file_path: entry.file_path,
                catalogue_order: entry.catalogue_order,
                generation_order: entry.generation_order,
            })
            .collect(),
    )?;
    anyhow::ensure!(
        revision == order.revision,
        "catalogue revision changed during rebuild"
    );
    engine.set_bulk_indexing(true)?;
    let cancel = IndexingCancellationToken::new();
    let mut rebuilt_documents = 0;
    for batch in inputs.chunks(64) {
        let results = engine.add_library_books(
            batch.iter().map(|(_, input)| input.clone()).collect(),
            &cancel,
        )?;
        for ((number, input), result) in batch.iter().zip(results) {
            anyhow::ensure!(
                result.status == LibraryBookStatus::Indexed,
                "rebuilding {:?} failed: {:?}: {:?}",
                input.file_path,
                result.status,
                result.error
            );
            let book = &expected[&input.file_path];
            anyhow::ensure!(
                result.book_number == *number && u64::from(result.docs) == book.documents,
                "rebuilding {:?} changed canonical document ids/count",
                input.file_path
            );
            anyhow::ensure!(
                [
                    result.content_hash,
                    result.text_hash,
                    result.metadata_hash,
                    result.source_hash
                ] == book.stamps,
                "rebuilding {:?} changed canonical text or metadata fingerprints",
                input.file_path
            );
            rebuilt_documents += u64::from(result.docs);
        }
        engine.commit()?;
        eprintln!("Rebuilt {rebuilt_documents}/{source_documents} documents");
    }
    engine.set_bulk_indexing(false)?;
    engine.commit()?;
    let rebuilt = catalogue(&engine.corpus_searcher())?;
    anyhow::ensure!(expected==rebuilt,"rebuilt ids, lexical text hashes, sections, references or book metadata differ from released index");
    let rebuilt_meta = json(&request.out.join("otzaria_index_meta.json"))?;
    anyhow::ensure!(
        rebuilt_meta["semantic_projection_version"] == 1,
        "rebuilt index did not record semantic projection provenance"
    );
    anyhow::ensure!(
        digest_file(&source_path.join("meta.json"))? == source_meta_sha,
        "source index changed while rebuilding"
    );
    anyhow::ensure!(
        digest_file(&request.seforim_db)? == database_sha,
        "library DB changed while rebuilding"
    );
    anyhow::ensure!(
        std::fs::read(&order_path)? == order_bytes,
        "source catalogue changed while rebuilding"
    );
    let report = BuildIndexReport {
        format: "otzaria-semantic-index-rebuild",
        version: 1,
        library_version: db_version,
        db_schema_version: db_schema,
        books: expected.len(),
        source_documents,
        rebuilt_documents,
        source_index_metadata: source_metadata,
        rebuilt_index_metadata: rebuilt_meta,
        source_index_meta_sha256: source_meta_sha,
        source_catalogue_sha256: catalogue_sha,
        source_book_order_sha256: format!("{:x}", Sha256::digest(&order_bytes)),
        database_sha256: database_sha,
        ids_text_and_metadata_verified: true,
    };
    crate::index_directory::atomic_replace(
        &request.out.join("build-provenance.json"),
        &serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

#[cfg(test)]
#[path = "semantic_index_builder_tests.rs"]
mod tests;
