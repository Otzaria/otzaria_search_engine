//! Line text of official books, read from the library database (`seforim.db`) at search
//! time instead of the Tantivy doc store.
//!
//! A document indexed with `TextStorage::LibraryDb` keeps its text in the inverted index
//! only. Its display text is the database row it was indexed from: book `id:<bookId>`
//! (the `filePath`), row = the document's `segment`-th row of the book in `lineIndex`
//! order. [`LineStore`] maps that key to a row, decodes it exactly as the indexing path
//! prepared it ([`prepare_row`]) and leaves normalization and verification to the caller.
//!
//! Lock order: a caller takes its Tantivy searcher first and the line-source mutex second;
//! nothing here calls back into Dart or into the engine while holding it.

use crate::sqlite_host;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// Same ceiling as the app's `LineContentCodec.maxLineBytes`.
const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;
/// A replaced or locked file must not stall a search; the database is never written here.
const BUSY_TIMEOUT: Duration = Duration::from_millis(500);
/// Same as the app's `stripDataUrisForIndex`: shorter payloads are kept.
const MIN_DATA_URI_PAYLOAD: usize = 64;

/// Where one document's text lives in the library database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct LineKey {
    pub book_id: i64,
    /// 0-based position of the row among the book's rows ordered by `lineIndex`.
    pub ordinal: u64,
}

/// One looked-up row.
#[derive(Debug)]
pub(crate) enum RowText {
    /// The text the indexing path fed to `normalize_text_for_indexing` for this row.
    Found(String),
    /// The book has no row at that ordinal (or no longer exists).
    Missing,
    /// The row exists but cannot be decoded (corrupt frame, unknown dictionary).
    Unreadable,
}

/// Result of one window: rows in the order of the requested keys, plus each touched book's
/// current row count (for books verified at book level).
pub(crate) struct WindowRows {
    pub rows: Vec<RowText>,
    pub book_rows: HashMap<i64, u64>,
}

/// How a book's ordinals map to rows.
enum BookRows {
    /// `lineIndex` is exactly `0..rows`: ordinal == lineIndex.
    Contiguous { rows: u64 },
    /// Anything else: the row ids in `lineIndex` order.
    Sparse { line_ids: Vec<i64> },
}

impl BookRows {
    fn len(&self) -> u64 {
        match self {
            BookRows::Contiguous { rows } => *rows,
            BookRows::Sparse { line_ids } => line_ids.len() as u64,
        }
    }
}

/// Decodes `line_content` BLOBs: a zstd frame of one dictionary from `zstd_dict`, chosen by
/// the dictionary id in the frame header (no checksum).
struct ZstdCodec {
    dicts: HashMap<u32, zstd::zstd_safe::DDict<'static>>,
    dctx: zstd::zstd_safe::DCtx<'static>,
}

impl ZstdCodec {
    fn load(conn: &Connection) -> Result<Self> {
        let mut dicts = HashMap::new();
        let mut stmt = conn.prepare("SELECT dict FROM zstd_dict")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let bytes: Vec<u8> = row.get(0).context("zstd_dict.dict is not a BLOB")?;
            let id = zstd::zstd_safe::get_dict_id_from_dict(&bytes).map_or(0, |id| id.get());
            let ddict = zstd::zstd_safe::DDict::try_create(&bytes)
                .context("ZSTD_createDDict failed for a zstd_dict row")?;
            dicts.insert(id, ddict);
        }
        let dctx = zstd::zstd_safe::DCtx::try_create().context("ZSTD_createDCtx failed")?;
        Ok(Self { dicts, dctx })
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let size = match zstd::zstd_safe::get_frame_content_size(frame) {
            Ok(Some(size)) if size <= MAX_LINE_BYTES => size as usize,
            other => anyhow::bail!("invalid zstd frame (content size {other:?})"),
        };
        // SAFETY: reads at most `frame.len()` bytes of a valid slice.
        let dict_id = unsafe {
            zstd::zstd_safe::zstd_sys::ZSTD_getDictID_fromFrame(frame.as_ptr().cast(), frame.len())
        };
        let ddict = self
            .dicts
            .get(&dict_id)
            .with_context(|| format!("dictionary {dict_id} is missing from zstd_dict"))?;
        let mut out: Vec<u8> = Vec::with_capacity(size);
        let written = self
            .dctx
            .decompress_using_ddict(&mut out, frame, ddict)
            .map_err(|code| {
                anyhow::anyhow!(
                    "zstd decode failed: {}",
                    zstd::zstd_safe::get_error_name(code)
                )
            })?;
        if written != size {
            anyhow::bail!("zstd frame declared {size} bytes and produced {written}");
        }
        Ok(out)
    }
}

/// A read-only connection to one library database, with the per-book caches valid for it.
pub(crate) struct LineStore {
    conn: Connection,
    /// `line_content` exists (schema 6); otherwise the text is `line.content`.
    split: bool,
    codec: Option<ZstdCodec>,
    books: HashMap<i64, Option<BookRows>>,
    /// Whether the indexing path saw `data:` anywhere in the book; only asked about books
    /// whose first row starts with a BOM.
    data_uri_books: HashMap<i64, bool>,
}

impl LineStore {
    pub fn open(path: &Path) -> Result<Self> {
        sqlite_host::ensure_ready()?;
        let conn = Connection::open_with_flags(
            sqlite_uri(path),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening the library database {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA query_only=1")?;
        let has_table = |name: &str| -> Result<bool> {
            Ok(conn
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [name],
                    |_| Ok(()),
                )
                .optional()?
                .is_some())
        };
        if !has_table("line")? {
            anyhow::bail!("{} has no `line` table", path.display());
        }
        let split = has_table("line_content")?;
        let codec = if has_table("zstd_dict")? {
            Some(ZstdCodec::load(&conn)?)
        } else {
            None
        };
        Ok(Self {
            conn,
            split,
            codec,
            books: HashMap::new(),
            data_uri_books: HashMap::new(),
        })
    }

    /// Closes the connection now, so the file can be replaced as soon as this returns.
    pub fn close(self) {
        if let Err((_, err)) = self.conn.close() {
            log::warn!("closing the library database failed: {err}");
        }
    }

    fn content_sql(&self, by: &str) -> String {
        if self.split {
            format!(
                "SELECT lc.content FROM line l LEFT JOIN line_content lc ON lc.id = l.id \
                 WHERE {by}"
            )
        } else {
            format!("SELECT l.content FROM line l WHERE {by}")
        }
    }

    /// Runs `f` inside one read transaction, so a window sees one snapshot.
    fn in_read_txn<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        self.conn.execute_batch("BEGIN")?;
        let result = f(self);
        let end = self
            .conn
            .execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" });
        let value = result?;
        end?;
        Ok(value)
    }

    /// Fetches every key in one read transaction. Keys may repeat and come in any order.
    pub fn fetch_window(&mut self, keys: &[LineKey]) -> Result<WindowRows> {
        self.in_read_txn(|store| {
            let mut order: Vec<usize> = (0..keys.len()).collect();
            order.sort_by_key(|&i| keys[i]);
            let mut rows: Vec<Option<RowText>> = (0..keys.len()).map(|_| None).collect();
            let mut book_rows = HashMap::new();
            for i in order {
                let key = keys[i];
                rows[i] = Some(store.fetch_row(key)?);
                let count = store.book_row_count(key.book_id)?;
                book_rows.insert(key.book_id, count);
            }
            Ok(WindowRows {
                rows: rows
                    .into_iter()
                    .map(|r| r.expect("every key fetched"))
                    .collect(),
                book_rows,
            })
        })
    }

    /// Every row of `book_id` in order (the build tools' whole-book path), in one
    /// transaction. `None` when the book does not exist.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    pub fn fetch_book(&mut self, book_id: i64) -> Result<Option<Vec<RowText>>> {
        self.in_read_txn(|store| {
            let count = store.book_row_count(book_id)?;
            if count == 0 {
                return Ok(None);
            }
            (0..count)
                .map(|ordinal| store.fetch_row(LineKey { book_id, ordinal }))
                .collect::<Result<Vec<_>>>()
                .map(Some)
        })
    }

    /// The book's current row count (0 when it does not exist).
    pub fn book_row_count(&mut self, book_id: i64) -> Result<u64> {
        Ok(self.book_rows(book_id)?.map_or(0, BookRows::len))
    }

    fn book_rows(&mut self, book_id: i64) -> Result<Option<&BookRows>> {
        if !self.books.contains_key(&book_id) {
            let (count, distinct, min, max): (i64, i64, Option<i64>, Option<i64>) = self
                .conn
                .prepare_cached(
                    "SELECT count(*), count(DISTINCT lineIndex), min(lineIndex),                      max(lineIndex) FROM line WHERE bookId = ?1",
                )?
                .query_row([book_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?;
            let rows = if count == 0 {
                None
            } else if count == distinct && min == Some(0) && max == Some(count - 1) {
                Some(BookRows::Contiguous { rows: count as u64 })
            } else {
                // Ties in lineIndex keep rowid order, as the index walk the app reads in does.
                let mut stmt = self.conn.prepare_cached(
                    "SELECT id FROM line WHERE bookId = ?1 ORDER BY lineIndex, id",
                )?;
                let line_ids = stmt
                    .query_map([book_id], |r| r.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Some(BookRows::Sparse { line_ids })
            };
            self.books.insert(book_id, rows);
        }
        Ok(self.books[&book_id].as_ref())
    }

    /// The stored value of one row; `None` when the book has no row at that ordinal.
    fn read_value(&mut self, key: LineKey) -> Result<Option<RawValue>> {
        let target = match self.book_rows(key.book_id)? {
            None => return Ok(None),
            Some(book) if key.ordinal >= book.len() => return Ok(None),
            Some(BookRows::Contiguous { .. }) => None,
            Some(BookRows::Sparse { line_ids }) => Some(line_ids[key.ordinal as usize]),
        };
        let value = match target {
            None => {
                let sql = self.content_sql("l.bookId = ?1 AND l.lineIndex = ?2");
                let mut stmt = self.conn.prepare_cached(&sql)?;
                stmt.query_row(rusqlite::params![key.book_id, key.ordinal as i64], |r| {
                    Ok(raw_value(r.get_ref(0)?))
                })
                .optional()?
            }
            Some(id) => {
                let sql = self.content_sql("l.id = ?1");
                let mut stmt = self.conn.prepare_cached(&sql)?;
                stmt.query_row([id], |r| Ok(raw_value(r.get_ref(0)?)))
                    .optional()?
            }
        };
        Ok(value)
    }

    /// UTF-8 bytes of a stored value; `Err` with the reason when it cannot be decoded.
    fn decode_value(&mut self, value: RawValue) -> std::result::Result<Vec<u8>, String> {
        match value {
            RawValue::Text(bytes) => Ok(bytes),
            // NULL content reads as an empty line, like the app's reader.
            RawValue::Null => Ok(Vec::new()),
            RawValue::Blob(frame) => match self.codec.as_mut() {
                Some(codec) => codec.decode(&frame).map_err(|err| format!("{err:#}")),
                None => Err("a BLOB row in a database without zstd_dict".to_string()),
            },
            RawValue::Other => Err("a row that is neither TEXT nor BLOB".to_string()),
        }
    }

    fn fetch_row(&mut self, key: LineKey) -> Result<RowText> {
        let Some(value) = self.read_value(key)? else {
            return Ok(RowText::Missing);
        };
        let bytes = match self.decode_value(value) {
            Ok(bytes) => bytes,
            Err(reason) => {
                log::warn!("line {key:?} cannot be decoded: {reason}");
                return Ok(RowText::Unreadable);
            }
        };
        let strip_bom = key.ordinal == 0
            && bytes.starts_with(&[0xEF, 0xBB, 0xBF])
            && self.book_contains_data_uri(key.book_id)?;
        Ok(RowText::Found(prepare_row(&bytes, strip_bom)))
    }

    /// The app decodes a book that contains `data:` anywhere as one string, which drops the
    /// BOM at the start of its first row; a clean book crosses as bytes and keeps it.
    fn book_contains_data_uri(&mut self, book_id: i64) -> Result<bool> {
        if let Some(&known) = self.data_uri_books.get(&book_id) {
            return Ok(known);
        }
        let sql = self.content_sql("l.bookId = ?1");
        // Streamed: an illustrated book can hold hundreds of MB of embedded images.
        let mut codec = self.codec.take();
        let scan = (|| -> Result<bool> {
            use rusqlite::types::ValueRef;
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let mut rows = stmt.query([book_id])?;
            while let Some(row) = rows.next()? {
                let hit = match row.get_ref(0)? {
                    ValueRef::Text(bytes) => contains_data_scheme(bytes),
                    ValueRef::Blob(frame) => codec
                        .as_mut()
                        .and_then(|c| c.decode(frame).ok())
                        .is_some_and(|bytes| contains_data_scheme(&bytes)),
                    _ => false,
                };
                if hit {
                    return Ok(true);
                }
            }
            Ok(false)
        })();
        self.codec = codec;
        let found = scan?;
        self.data_uri_books.insert(book_id, found);
        Ok(found)
    }
}

enum RawValue {
    Null,
    Text(Vec<u8>),
    Blob(Vec<u8>),
    Other,
}

fn raw_value(value: rusqlite::types::ValueRef<'_>) -> RawValue {
    use rusqlite::types::ValueRef;
    match value {
        ValueRef::Null => RawValue::Null,
        ValueRef::Text(bytes) => RawValue::Text(bytes.to_vec()),
        ValueRef::Blob(bytes) => RawValue::Blob(bytes.to_vec()),
        _ => RawValue::Other,
    }
}

/// `file:` URI for a read-only open. `%`, `?` and `#` are the characters SQLite's URI
/// parser would otherwise interpret.
fn sqlite_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    let mut escaped = String::with_capacity(raw.len() + 16);
    for c in raw.chars() {
        match c {
            '%' => escaped.push_str("%25"),
            '?' => escaped.push_str("%3f"),
            '#' => escaped.push_str("%23"),
            c => escaped.push(c),
        }
    }
    format!("file:{escaped}?mode=ro")
}

/// The row exactly as the indexing path handed it to the line splitter: UTF-8 decoded
/// lossily, the first row's BOM dropped when the app decoded the book as one string, and
/// embedded `data:` URIs removed.
pub(crate) fn prepare_row(bytes: &[u8], strip_leading_bom: bool) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = if strip_leading_bom {
        text.strip_prefix('\u{FEFF}').unwrap_or(&text)
    } else {
        &text
    };
    strip_data_uris_for_index(text)
}

fn contains_data_scheme(bytes: &[u8]) -> bool {
    bytes.windows(5).any(|w| w == b"data:")
}

fn is_data_uri_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b';' | b',' | b'=' | b'.' | b'-')
}

/// Port of the app's `IndexingRepository.stripDataUrisForIndex`: every `data:` followed by
/// at least [`MIN_DATA_URI_PAYLOAD`] payload characters is removed with its payload. The
/// scheme and payload are ASCII, so byte offsets agree with the app's UTF-16 scan.
pub(crate) fn strip_data_uris_for_index(text: &str) -> String {
    const SCHEME: &str = "data:";
    let bytes = text.as_bytes();
    let Some(mut match_start) = text.find(SCHEME) else {
        return text.to_string();
    };
    let mut out = String::new();
    let mut copied_up_to = 0;
    loop {
        let mut end = match_start + SCHEME.len();
        while end < bytes.len() && is_data_uri_byte(bytes[end]) {
            end += 1;
        }
        if end - match_start - SCHEME.len() >= MIN_DATA_URI_PAYLOAD {
            out.push_str(&text[copied_up_to..match_start]);
            copied_up_to = end;
        }
        match text[end..].find(SCHEME) {
            Some(next) => match_start = end + next,
            None => break,
        }
    }
    if copied_up_to == 0 {
        return text.to_string();
    }
    out.push_str(&text[copied_up_to..]);
    out
}

// ── Process-global source ──────────────────────────────────────────────────────

/// Why a window could not be served.
#[derive(Debug, Clone)]
pub(crate) enum SourceUnavailable {
    Unconfigured,
    Suspended,
    /// Opening or reading failed; the reason has been logged.
    Failed,
}

struct Global {
    path: Option<PathBuf>,
    store: Option<LineStore>,
    generation: u64,
    suspend_depth: u32,
}

static GLOBAL: Mutex<Global> = Mutex::new(Global {
    path: None,
    store: None,
    generation: 0,
    suspend_depth: 0,
});

fn global() -> MutexGuard<'static, Global> {
    // A panic inside a window leaves only caches behind; they are rebuilt on demand.
    GLOBAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Global {
    fn drop_store(&mut self) {
        if let Some(store) = self.store.take() {
            store.close();
        }
    }
}

/// Snapshot of the global source, for `line_source_status`.
pub(crate) struct Status {
    pub configured: bool,
    pub open: bool,
    pub suspend_depth: u32,
    pub generation: u64,
}

pub(crate) fn configure(db_path: &str) -> Result<()> {
    if db_path.is_empty() {
        anyhow::bail!("configure_line_source needs a database path");
    }
    let path = PathBuf::from(db_path);
    let mut g = global();
    if g.path.as_deref() != Some(path.as_path()) {
        g.drop_store();
        g.path = Some(path);
        g.generation += 1;
    }
    Ok(())
}

pub(crate) fn suspend() {
    let mut g = global();
    g.suspend_depth += 1;
    g.drop_store();
}

pub(crate) fn resume() -> Result<()> {
    let mut g = global();
    if g.suspend_depth == 0 {
        anyhow::bail!("resume_line_source without a matching suspend_line_source");
    }
    g.suspend_depth -= 1;
    if g.suspend_depth == 0 {
        // The file may have been replaced while suspended: nothing cached survives.
        g.drop_store();
        g.generation += 1;
    }
    Ok(())
}

pub(crate) fn status() -> Status {
    let g = global();
    Status {
        configured: g.path.is_some(),
        open: g.store.is_some(),
        suspend_depth: g.suspend_depth,
        generation: g.generation,
    }
}

/// Runs `f` against the global store, opening it on first use. Holding the mutex for the
/// window is what lets `suspend` wait for an in-flight window before closing the file.
pub(crate) fn with_store<R>(
    f: impl FnOnce(&mut LineStore) -> Result<R>,
) -> std::result::Result<R, SourceUnavailable> {
    let mut g = global();
    if g.suspend_depth > 0 {
        return Err(SourceUnavailable::Suspended);
    }
    let Some(path) = g.path.clone() else {
        return Err(SourceUnavailable::Unconfigured);
    };
    if g.store.is_none() {
        match LineStore::open(&path) {
            Ok(store) => g.store = Some(store),
            Err(err) => {
                log::warn!("line source unavailable: {err:#}");
                return Err(SourceUnavailable::Failed);
            }
        }
    }
    let store = g.store.as_mut().expect("opened above");
    match f(store) {
        Ok(value) => Ok(value),
        Err(err) => {
            // A failed read may mean the file changed under us; reopen next time.
            log::warn!("line source read failed: {err:#}");
            g.drop_store();
            Err(SourceUnavailable::Failed)
        }
    }
}

/// Serializes tests that touch the process-global source.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Resets the global source between tests.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    let mut g = global();
    g.drop_store();
    g.path = None;
    g.suspend_depth = 0;
    g.generation += 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_uris_are_stripped_like_the_app() {
        let long = "A".repeat(64);
        let short = "A".repeat(63);
        assert_eq!(
            strip_data_uris_for_index(&format!("x data:{long} y")),
            "x  y"
        );
        assert_eq!(
            strip_data_uris_for_index(&format!("x data:{short} y")),
            format!("x data:{short} y")
        );
        // The payload stops at the first character outside the set.
        assert_eq!(
            strip_data_uris_for_index(&format!("<img src=\"data:image/png;base64,{long}\">")),
            "<img src=\"\">"
        );
        // The payload run swallows the next scheme's letters up to its `:`, exactly as
        // the app's scan does.
        assert_eq!(
            strip_data_uris_for_index(&format!("data:{long}data:{short}")),
            format!(":{short}")
        );
        // A scheme inside a short payload is not searched again.
        assert_eq!(
            strip_data_uris_for_index(&format!("data:data:{long}")),
            format!("data:data:{long}")
        );
        assert_eq!(strip_data_uris_for_index("בלי כלום"), "בלי כלום");
    }

    #[test]
    fn the_first_row_bom_is_dropped_only_when_asked() {
        let bytes = b"\xEF\xBB\xBFabc";
        assert_eq!(prepare_row(bytes, false), "\u{FEFF}abc");
        assert_eq!(prepare_row(bytes, true), "abc");
        assert_eq!(prepare_row(b"a\xFFb", false), "a\u{FFFD}b");
    }

    #[test]
    fn uri_escapes_what_sqlite_would_parse() {
        assert_eq!(
            sqlite_uri(Path::new(r"C:\a b\x#1?%.db")),
            "file:C:/a b/x%231%3f%25.db?mode=ro"
        );
    }
}
