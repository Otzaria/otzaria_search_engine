//! Line text of official books, read from the library database (`seforim.db`) at search
//! time instead of the Tantivy doc store.
//!
//! A document indexed with `TextStorage::LibraryDb` keeps its text in the inverted index
//! only. Its display text is the database row it was indexed from: book `id:<bookId>`
//! (the `filePath`), row = the document's `segment`-th row of the book in `lineIndex`
//! order. [`LineStore`] maps that key to a row, decodes it exactly as the indexing path
//! prepared it ([`prepare_row`]) and leaves verification ([`line_check`]) and
//! normalization to the caller.
//!
//! Lock order: a caller takes its Tantivy searcher first and the line-source mutex second;
//! nothing here calls back into Dart or into the engine while holding it.

use crate::sqlite_host;
use anyhow::{Context, Result};
use lru::LruCache;
use rusqlite::config::DbConfig;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Same ceiling as the app's `LineContentCodec.maxLineBytes`.
const MAX_LINE_BYTES: u64 = 16 * 1024 * 1024;
/// How long a read waits for a writer before its window is served `Unavailable`.
const BUSY_TIMEOUT: Duration = Duration::from_millis(100);
/// After the database answered busy, windows skip it for this long.
const BUSY_BACKOFF: Duration = Duration::from_secs(1);
/// Upper bound on the cached ordinal-to-rowid maps: 4 bytes a row, ~4M rows.
const ROW_MAP_BYTES: usize = 16 * 1024 * 1024;
/// SQLite page cache of the connection, in KiB: a window's index and content pages stay
/// cached for the next one.
const PAGE_CACHE_KIB: i64 = 8 * 1024;
/// Rows found at `lineIndex = ordinal` that passed their check, remembered (16 bytes each).
const AT_LINE_INDEX_ENTRIES: usize = 64 * 1024;
/// Same as the app's `stripDataUrisForIndex`: shorter payloads are kept.
const MIN_DATA_URI_PAYLOAD: usize = 64;
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// The `lineCheck` of a line: CRC-32 of the exact text the indexing path normalized
/// ([`prepare_row`]'s output), so a change to anything in it — spacing, punctuation,
/// nikud, markup — is seen.
pub(crate) fn line_check(prepared: &str) -> u32 {
    crc32fast::hash(prepared.as_bytes())
}

/// Where one document's text lives in the library database.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct LineKey {
    pub book_id: i64,
    /// 0-based position of the row among the book's rows ordered by `lineIndex`.
    pub ordinal: u64,
}

/// One looked-up row.
#[derive(Debug, Clone)]
pub(crate) enum RowText {
    /// The text the indexing path fed to `normalize_text_for_indexing` for this row.
    Found(String),
    /// The book has no row at that ordinal (or no longer exists).
    Missing,
    /// The row exists but cannot be decoded (corrupt frame, unknown dictionary).
    Unreadable,
}

/// A book's row ids in `lineIndex` order (ties by rowid, as the app reads them).
enum RowIds {
    Narrow(Vec<u32>),
    Wide(Vec<i64>),
}

impl RowIds {
    fn new(ids: Vec<i64>) -> Self {
        if ids.iter().all(|&id| u32::try_from(id).is_ok()) {
            RowIds::Narrow(ids.into_iter().map(|id| id as u32).collect())
        } else {
            RowIds::Wide(ids)
        }
    }

    fn get(&self, ordinal: u64) -> Option<i64> {
        let ordinal = usize::try_from(ordinal).ok()?;
        match self {
            RowIds::Narrow(ids) => ids.get(ordinal).map(|&id| i64::from(id)),
            RowIds::Wide(ids) => ids.get(ordinal).copied(),
        }
    }

    fn bytes(&self) -> usize {
        std::mem::size_of::<(i64, Self)>()
            + match self {
                RowIds::Narrow(ids) => ids.capacity() * 4,
                RowIds::Wide(ids) => ids.capacity() * 8,
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

/// UTF-8 bytes of a stored value; `Err` with the reason when it cannot be decoded.
fn row_bytes<'a>(
    value: ValueRef<'a>,
    codec: &mut Option<ZstdCodec>,
) -> std::result::Result<Cow<'a, [u8]>, String> {
    match value {
        ValueRef::Text(bytes) => Ok(Cow::Borrowed(bytes)),
        // NULL content reads as an empty line, like the app's reader.
        ValueRef::Null => Ok(Cow::Borrowed(&[])),
        ValueRef::Blob(frame) => match codec.as_mut() {
            Some(codec) => codec
                .decode(frame)
                .map(Cow::Owned)
                .map_err(|err| format!("{err:#}")),
            None => Err("a BLOB row in a database without zstd_dict".to_string()),
        },
        _ => Err("a row that is neither TEXT nor BLOB".to_string()),
    }
}

/// What the database holds; re-read when another connection commits.
struct Layout {
    /// `line_content` exists (schema 6); otherwise the text is `line.content`.
    split: bool,
    codec: Option<ZstdCodec>,
}

impl Layout {
    fn read(conn: &Connection, path: &Path) -> Result<Self> {
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
            Some(ZstdCodec::load(conn)?)
        } else {
            None
        };
        Ok(Self { split, codec })
    }

    /// The content column of every row of one book, in `lineIndex` order.
    fn book_sql(&self) -> &'static str {
        if self.split {
            "SELECT lc.content FROM line l LEFT JOIN line_content lc ON lc.id = l.id \
             WHERE l.bookId = ?1 ORDER BY l.lineIndex, l.id"
        } else {
            "SELECT content FROM line WHERE bookId = ?1 ORDER BY lineIndex, id"
        }
    }
}

/// A read-only connection to one library database, with the per-book caches valid for it.
pub(crate) struct LineStore {
    conn: Connection,
    path: PathBuf,
    layout: Layout,
    /// `PRAGMA data_version` the caches below were built under.
    data_version: i64,
    /// Ordinal-to-rowid maps of the books whose rows were not all at `lineIndex = ordinal`,
    /// bounded by [`ROW_MAP_BYTES`], least recently used out first.
    books: LruCache<i64, RowIds>,
    book_bytes: usize,
    /// The row at `lineIndex = ordinal` of keys whose check it passed.
    at_line_index: HashMap<LineKey, i64>,
    /// Whether the indexing path saw `data:` anywhere in the book; only asked about books
    /// whose first row starts with a BOM.
    data_uri_books: HashMap<i64, bool>,
}

impl LineStore {
    pub fn open(path: &Path) -> Result<Self> {
        sqlite_host::ensure_ready()?;
        // A plain path, not a `file:` URI: a UNC path (`\\server\share\...`) has no URI
        // form SQLite accepts.
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("opening the library database {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // With the library's sqlite_stat4, a plan that depends on bound values is re-prepared
        // on every new binding: the per-line lookups would each pay a full prepare.
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_QPSG, true)?;
        conn.execute_batch(&format!(
            "PRAGMA query_only=1; PRAGMA cache_size=-{PAGE_CACHE_KIB}"
        ))?;
        let data_version = data_version(&conn)?;
        let layout = Layout::read(&conn, path)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            layout,
            data_version,
            books: LruCache::unbounded(),
            book_bytes: 0,
            at_line_index: HashMap::new(),
            data_uri_books: HashMap::new(),
        })
    }

    /// Closes the connection now, so the file can be replaced as soon as this returns.
    pub fn close(self) {
        if let Err((_, err)) = self.conn.close() {
            log::warn!("closing the library database failed: {err}");
        }
    }

    /// Runs `f` inside one read transaction, so it sees one snapshot. Caches built before
    /// another connection committed are dropped first.
    fn in_read_txn<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        self.conn.execute_batch("BEGIN")?;
        let result = self.drop_caches_if_changed().and_then(|()| f(self));
        let end = self
            .conn
            .execute_batch(if result.is_ok() { "COMMIT" } else { "ROLLBACK" });
        let value = result?;
        end?;
        Ok(value)
    }

    fn drop_caches_if_changed(&mut self) -> Result<()> {
        let version = data_version(&self.conn)?;
        if version != self.data_version {
            self.books.clear();
            self.book_bytes = 0;
            self.at_line_index.clear();
            self.data_uri_books.clear();
            self.layout = Layout::read(&self.conn, &self.path)?;
            self.data_version = version;
        }
        Ok(())
    }

    /// Fetches every key in one read transaction, in the order given. Keys may repeat.
    ///
    /// `checks` holds each key's `lineCheck`, when it has one. Such a key is read first at
    /// `lineIndex = ordinal` — its row whenever the book's `lineIndex` has no gap or repeat
    /// before it — and only a row that fails the check is read again through the book's
    /// ordinal map, built then. Keys without a check always go through the map.
    pub fn fetch_window(
        &mut self,
        keys: &[LineKey],
        checks: &[Option<u32>],
    ) -> Result<Vec<RowText>> {
        assert_eq!(keys.len(), checks.len(), "one check slot per key");
        self.in_read_txn(|store| {
            let mut rows = vec![RowText::Missing; keys.len()];
            let mut targets: Vec<(i64, usize)> = Vec::with_capacity(keys.len());
            let mut by_line_index: Vec<(usize, Option<i64>)> = Vec::new();
            for (slot, (key, check)) in keys.iter().zip(checks).enumerate() {
                let id = if check.is_some() && !store.books.contains(&key.book_id) {
                    let id = match store.at_line_index.get(key) {
                        Some(&id) => Some(id),
                        None => store.row_id_at_line_index(*key)?,
                    };
                    by_line_index.push((slot, id));
                    id
                } else {
                    store.row_ids(key.book_id)?.get(key.ordinal)
                };
                if let Some(id) = id {
                    targets.push((id, slot));
                }
            }
            store.read_rows(keys, checks, &mut targets, &mut rows)?;
            let mut retry: Vec<(i64, usize)> = Vec::new();
            for (slot, id) in by_line_index {
                let key = keys[slot];
                if let (Some(id), RowText::Found(text)) = (id, &rows[slot]) {
                    if Some(line_check(text)) == checks[slot] {
                        store.remember_at_line_index(key, id);
                        continue;
                    }
                }
                store.at_line_index.remove(&key);
                rows[slot] = RowText::Missing;
                if let Some(id) = store.row_ids(key.book_id)?.get(key.ordinal) {
                    retry.push((id, slot));
                }
            }
            store.read_rows(keys, checks, &mut retry, &mut rows)?;
            Ok(rows)
        })
    }

    fn remember_at_line_index(&mut self, key: LineKey, id: i64) {
        if self.at_line_index.len() >= AT_LINE_INDEX_ENTRIES {
            self.at_line_index.clear();
        }
        self.at_line_index.insert(key, id);
    }

    /// Reads the row of each `(rowid, slot)` into `rows[slot]`.
    fn read_rows(
        &mut self,
        keys: &[LineKey],
        checks: &[Option<u32>],
        targets: &mut [(i64, usize)],
        rows: &mut [RowText],
    ) -> Result<()> {
        // Ascending rowids walk the content B-tree forward.
        targets.sort_unstable();
        let mut first_rows: Vec<(usize, Vec<u8>)> = Vec::new();
        {
            let Self { conn, layout, .. } = &mut *self;
            let mut stmt = conn.prepare_cached(if layout.split {
                "SELECT content FROM line_content WHERE id = ?1"
            } else {
                "SELECT content FROM line WHERE id = ?1"
            })?;
            for (i, &(id, slot)) in targets.iter().enumerate() {
                if i > 0 && targets[i - 1].0 == id {
                    continue;
                }
                let key = keys[slot];
                let mut query = stmt.query([id])?;
                // A line without a content row reads as empty, like the app's LEFT JOIN.
                let value = match query.next()? {
                    Some(row) => row.get_ref(0)?,
                    None => ValueRef::Null,
                };
                rows[slot] = match row_bytes(value, &mut layout.codec) {
                    Ok(bytes)
                        if key.ordinal == 0 && bytes.starts_with(BOM) && checks[slot].is_none() =>
                    {
                        first_rows.push((slot, bytes.into_owned()));
                        continue;
                    }
                    Ok(bytes) => {
                        let mut text = prepare_row(&bytes, false);
                        if key.ordinal == 0 && bytes.starts_with(BOM) {
                            // The stored check distinguishes the app's two BOM paths without
                            // reading any other row, even when the database text is stale.
                            if let Some(check) = checks[slot] {
                                if line_check(&text) != check {
                                    let without_bom =
                                        text.strip_prefix('\u{FEFF}').unwrap_or(&text);
                                    if line_check(without_bom) == check {
                                        text = without_bom.to_owned();
                                    }
                                }
                            }
                        }
                        RowText::Found(text)
                    }
                    Err(reason) => {
                        log::warn!("line {key:?} cannot be decoded: {reason}");
                        RowText::Unreadable
                    }
                };
            }
        }
        for (slot, bytes) in first_rows {
            let strip_bom = self.book_contains_data_uri(keys[slot].book_id)?;
            rows[slot] = RowText::Found(prepare_row(&bytes, strip_bom));
        }
        for i in 1..targets.len() {
            if targets[i - 1].0 == targets[i].0 {
                rows[targets[i].1] = rows[targets[i - 1].1].clone();
            }
        }
        Ok(())
    }

    /// The first row at `lineIndex = ordinal`, through idx_line_book_index alone.
    fn row_id_at_line_index(&self, key: LineKey) -> Result<Option<i64>> {
        let Ok(line_index) = i64::try_from(key.ordinal) else {
            return Ok(None);
        };
        Ok(self
            .conn
            .prepare_cached("SELECT min(id) FROM line WHERE bookId = ?1 AND lineIndex = ?2")?
            .query_row([key.book_id, line_index], |r| r.get(0))?)
    }

    /// Every row of `book_id` in order (the build tools' whole-book path), streamed by one
    /// query. `None` when the book does not exist.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    pub fn fetch_book(&mut self, book_id: i64) -> Result<Option<Vec<RowText>>> {
        self.in_read_txn(|store| {
            let Self { conn, layout, .. } = &mut *store;
            let mut stmt = conn.prepare_cached(layout.book_sql())?;
            let mut query = stmt.query([book_id])?;
            let mut rows = Vec::new();
            let mut first_row: Option<Vec<u8>> = None;
            let mut has_data_uri = false;
            while let Some(row) = query.next()? {
                let text = match row_bytes(row.get_ref(0)?, &mut layout.codec) {
                    Ok(bytes) => {
                        has_data_uri |= contains_data_scheme(&bytes);
                        if rows.is_empty() && bytes.starts_with(BOM) {
                            first_row = Some(bytes.into_owned());
                            RowText::Missing
                        } else {
                            RowText::Found(prepare_row(&bytes, false))
                        }
                    }
                    Err(reason) => {
                        log::warn!(
                            "book {book_id} row {} cannot be decoded: {reason}",
                            rows.len()
                        );
                        RowText::Unreadable
                    }
                };
                rows.push(text);
            }
            if rows.is_empty() {
                return Ok(None);
            }
            if let Some(bytes) = first_row {
                rows[0] = RowText::Found(prepare_row(&bytes, has_data_uri));
            }
            Ok(Some(rows))
        })
    }

    /// The book's current row count (0 when it does not exist).
    pub fn book_row_count(&mut self, book_id: i64) -> Result<u64> {
        let count: i64 = self
            .conn
            .prepare_cached("SELECT count(*) FROM line WHERE bookId = ?1")?
            .query_row([book_id], |r| r.get(0))?;
        Ok(count as u64)
    }

    fn row_ids(&mut self, book_id: i64) -> Result<&RowIds> {
        if !self.books.contains(&book_id) {
            // idx_line_book_index covers this: the table's pages are never read.
            let ids = self
                .conn
                .prepare_cached("SELECT id FROM line WHERE bookId = ?1 ORDER BY lineIndex, id")?
                .query_map([book_id], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let ids = RowIds::new(ids);
            self.book_bytes += ids.bytes();
            self.books.put(book_id, ids);
            while self.book_bytes > ROW_MAP_BYTES && self.books.len() > 1 {
                if let Some((_, evicted)) = self.books.pop_lru() {
                    self.book_bytes -= evicted.bytes();
                }
            }
        }
        Ok(self.books.get(&book_id).expect("cached above"))
    }

    /// The app decodes a book that contains `data:` anywhere as one string, which drops the
    /// BOM at the start of its first row; a clean book crosses as bytes and keeps it.
    fn book_contains_data_uri(&mut self, book_id: i64) -> Result<bool> {
        if let Some(&known) = self.data_uri_books.get(&book_id) {
            return Ok(known);
        }
        let found = {
            let Self { conn, layout, .. } = &mut *self;
            // Streamed: an illustrated book can hold hundreds of MB of embedded images.
            let mut stmt = conn.prepare_cached(layout.book_sql())?;
            let mut rows = stmt.query([book_id])?;
            let mut found = false;
            while let Some(row) = rows.next()? {
                if row_bytes(row.get_ref(0)?, &mut layout.codec)
                    .is_ok_and(|bytes| contains_data_scheme(&bytes))
                {
                    found = true;
                    break;
                }
            }
            found
        };
        self.data_uri_books.insert(book_id, found);
        Ok(found)
    }

    /// Bytes held by the row maps, and by the connection's page cache.
    #[cfg(test)]
    pub(crate) fn memory(&self) -> (usize, usize) {
        let (mut current, mut high) = (0, 0);
        // SAFETY: the handle is valid while the connection is.
        unsafe {
            rusqlite::ffi::sqlite3_db_status(
                self.conn.handle(),
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED,
                &mut current,
                &mut high,
                0,
            );
        }
        (self.book_bytes, current as usize)
    }
}

fn data_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA data_version", [], |r| r.get(0))?)
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
    /// A writer holds the database; retried after [`BUSY_BACKOFF`].
    Busy,
    /// Opening or reading failed; the reason has been logged.
    Failed,
}

struct Global {
    path: Option<PathBuf>,
    store: Option<LineStore>,
    generation: u64,
    suspend_depth: u32,
    owner_ports: Vec<i64>,
    busy_until: Option<Instant>,
    library_fallbacks: u64,
}

static GLOBAL: Mutex<Global> = Mutex::new(Global {
    path: None,
    store: None,
    generation: 0,
    suspend_depth: 0,
    owner_ports: Vec::new(),
    busy_until: None,
    library_fallbacks: 0,
});

fn global() -> MutexGuard<'static, Global> {
    let mut g = GLOBAL.lock().unwrap_or_else(|poisoned| {
        // A panic inside a window can leave its read transaction open, holding the file's
        // shared lock: close the connection, and the caches with it.
        let mut g = poisoned.into_inner();
        g.drop_store();
        GLOBAL.clear_poison();
        g
    });
    g.reap_dead_owners(owner_port_is_open);
    g
}

fn owner_port_is_open(port: i64) -> bool {
    flutter_rust_bridge::for_generated::Channel::new(port).post(())
}

impl Global {
    fn reap_dead_owners(&mut self, mut is_open: impl FnMut(i64) -> bool) {
        let before = self.owner_ports.len();
        self.owner_ports.retain(|&port| is_open(port));
        if before != self.owner_ports.len()
            && self.owner_ports.is_empty()
            && self.suspend_depth == 0
        {
            self.drop_store();
            self.generation += 1;
            self.busy_until = None;
        }
    }

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
    pub library_fallbacks: u64,
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
        g.busy_until = None;
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
        g.busy_until = None;
    }
    Ok(())
}

/// Idempotent holds tied to Dart ports: the VM closes them when their isolate exits.
pub(crate) fn suspend_owned(owner_port: i64) -> Result<()> {
    let mut g = global();
    if owner_port <= 0 || !owner_port_is_open(owner_port) {
        anyhow::bail!("line-source suspension needs an open Dart owner port");
    }
    if !g.owner_ports.contains(&owner_port) {
        g.owner_ports.push(owner_port);
    }
    g.drop_store();
    Ok(())
}

pub(crate) fn resume_owned(owner_port: i64) {
    let mut g = global();
    let before = g.owner_ports.len();
    g.owner_ports.retain(|&port| port != owner_port);
    if before != g.owner_ports.len() && g.owner_ports.is_empty() && g.suspend_depth == 0 {
        g.drop_store();
        g.generation += 1;
        g.busy_until = None;
    }
}

pub(crate) fn status() -> Status {
    let g = global();
    Status {
        configured: g.path.is_some(),
        open: g.store.is_some(),
        suspend_depth: g.suspend_depth + g.owner_ports.len() as u32,
        generation: g.generation,
        library_fallbacks: g.library_fallbacks,
    }
}

/// Counts a book asked for as `LibraryDb` and stored `InIndex`.
pub(crate) fn note_library_fallback() {
    global().library_fallbacks += 1;
}

/// Whether `err` is SQLite reporting another connection's lock.
fn is_busy(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(e, _))
                if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        )
    })
}

/// Runs `f` against the global store, opening it on first use. Holding the mutex for the
/// window is what lets `suspend` wait for an in-flight window before closing the file.
pub(crate) fn with_store<R>(
    f: impl FnOnce(&mut LineStore) -> Result<R>,
) -> std::result::Result<R, SourceUnavailable> {
    with_store_inner(true, f)
}

/// [`with_store`] for indexing, which may wait out one busy timeout: a window's busy
/// backoff would otherwise store whole books in the index.
pub(crate) fn with_store_for_indexing<R>(
    f: impl FnOnce(&mut LineStore) -> Result<R>,
) -> std::result::Result<R, SourceUnavailable> {
    with_store_inner(false, f)
}

fn with_store_inner<R>(
    honor_backoff: bool,
    f: impl FnOnce(&mut LineStore) -> Result<R>,
) -> std::result::Result<R, SourceUnavailable> {
    let mut g = global();
    if g.suspend_depth > 0 || !g.owner_ports.is_empty() {
        return Err(SourceUnavailable::Suspended);
    }
    let Some(path) = g.path.clone() else {
        return Err(SourceUnavailable::Unconfigured);
    };
    if honor_backoff && g.busy_until.is_some_and(|until| Instant::now() < until) {
        return Err(SourceUnavailable::Busy);
    }
    g.busy_until = None;
    let result = match g.store.as_mut() {
        Some(store) => f(store),
        None => LineStore::open(&path).and_then(|store| f(g.store.insert(store))),
    };
    result.map_err(|err| {
        if is_busy(&err) {
            // The connection is fine; only the writer has to finish.
            log::warn!("library database is busy; skipping it for {BUSY_BACKOFF:?}: {err:#}");
            g.busy_until = Some(Instant::now() + BUSY_BACKOFF);
            SourceUnavailable::Busy
        } else {
            // A failed read may mean the file changed under us; reopen next time.
            log::warn!("line source unavailable: {err:#}");
            g.drop_store();
            SourceUnavailable::Failed
        }
    })
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
    g.owner_ports.clear();
    g.generation += 1;
    g.busy_until = None;
}

/// The books whose ordinal maps the open store holds, ascending.
#[cfg(test)]
pub(crate) fn mapped_books_for_tests() -> Vec<i64> {
    let g = global();
    let mut ids: Vec<i64> = g
        .store
        .as_ref()
        .map(|s| s.books.iter().map(|(&id, _)| id).collect())
        .unwrap_or_default();
    ids.sort_unstable();
    ids
}

/// `(row map bytes, page cache bytes)` of the open store.
#[cfg(test)]
pub(crate) fn memory_for_tests() -> Option<(usize, usize)> {
    global().store.as_ref().map(LineStore::memory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_owner_cleanup_preserves_live_and_anonymous_holds() {
        let mut g = Global {
            path: None,
            store: None,
            generation: 0,
            suspend_depth: 1,
            owner_ports: vec![11, 22],
            busy_until: None,
            library_fallbacks: 0,
        };
        g.reap_dead_owners(|port| port == 22);
        assert_eq!(g.owner_ports, vec![22]);
        assert_eq!(g.generation, 0);
        g.reap_dead_owners(|_| false);
        assert_eq!(g.suspend_depth, 1);
        assert_eq!(g.generation, 0);
        g.suspend_depth = 0;
        g.owner_ports.push(33);
        g.reap_dead_owners(|_| false);
        assert_eq!(g.generation, 1);
        g.reap_dead_owners(|_| false);
        assert_eq!(g.generation, 1);
    }

    #[test]
    fn checked_bom_row_does_not_scan_the_book() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
                            CREATE INDEX idx_line_book_index ON line(bookId, lineIndex);
                            INSERT INTO line VALUES (1, 1, 0, char(65279) || 'בראשית');
                            INSERT INTO line VALUES (2, 1, 1, 'data:short');").unwrap();
        let mut store = LineStore::open(&path).unwrap();
        let key = LineKey {
            book_id: 1,
            ordinal: 0,
        };
        for text in ["בראשית", "\u{FEFF}בראשית"] {
            let rows = store
                .fetch_window(&[key], &[Some(line_check(text))])
                .unwrap();
            assert!(matches!(&rows[0], RowText::Found(actual) if actual == text));
            assert!(
                store.data_uri_books.is_empty(),
                "checked rows must not scan other content"
            );
        }
    }

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
    fn a_panic_inside_a_window_closes_its_connection() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_tests();
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("lib.db");
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER,
                                    content TEXT);
                 INSERT INTO line VALUES (1, 1, 0, 'x');",
            )
            .unwrap();
        configure(db.to_str().unwrap()).unwrap();
        let panicked = std::panic::catch_unwind(|| {
            with_store(|store| store.in_read_txn(|_| -> Result<()> { panic!("inside a window") }))
        });
        assert!(panicked.is_err());
        // Its read transaction went with the connection: a writer gets the file at once.
        assert!(!status().open);
        let writer = Connection::open(&db).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE; COMMIT").unwrap();
        assert_eq!(with_store(|store| store.book_row_count(1)).ok(), Some(1));
        reset_for_tests();
    }

    #[test]
    fn line_check_sees_spacing_punctuation_and_nikud() {
        let base = line_check("בראשית ברא אלהים");
        for changed in [
            "בראשית  ברא אלהים",
            "בראשית ברא אלהים.",
            "בְּרֵאשִׁית ברא אלהים",
            "<b>בראשית</b> ברא אלהים",
            "",
        ] {
            assert_ne!(line_check(changed), base, "{changed}");
        }
        assert_eq!(line_check("בראשית ברא אלהים"), base);
    }
}
