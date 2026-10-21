//! Line text of official books, read from the library database (`seforim.db`) at search
//! time instead of the Tantivy doc store; personal books (`uid:<bookId>`) likewise from the
//! user-books database, and books of an attached database (`db:<slug>:<bookId>`) from that
//! database — each its own [`Source`] with the same layout rules.
//!
//! A document indexed with `TextStorage::LibraryDb` keeps its text in the inverted index
//! only. Its display text is the database row it was indexed from: book `id:<bookId>`
//! (the `filePath`), row = the document's `segment`-th row of the book in `lineIndex`
//! order. [`LineStore`] maps that key to a row, decodes it exactly as the indexing path
//! prepared it ([`prepare_row`]) and leaves verification ([`line_check`]) and
//! normalization to the caller.
//!
//! Lock order: a caller takes its Tantivy searcher first and one source's mutex second (never
//! two at once); nothing here calls back into Dart or into the engine while holding it.

use crate::inline_notes::{BookNote, BookNotes};
use crate::sqlite_host;
use anyhow::{Context, Result};
use lru::LruCache;
use rusqlite::config::DbConfig;
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
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

#[cfg(test)]
thread_local! {
    static CODEC_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Decodes `line_content` BLOBs: a zstd frame of one dictionary from `zstd_dict`, chosen by
/// the dictionary id in the frame header (no checksum).
struct ZstdCodec {
    dicts: Arc<HashMap<u32, zstd::zstd_safe::DDict<'static>>>,
    dctx: zstd::zstd_safe::DCtx<'static>,
    /// A frame named a dictionary this codec does not hold: reload on the next commit.
    missed_dict: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ScanFileStamp {
    identity: Option<FileIdentity>,
    main: (u64, std::time::SystemTime),
    wal: Option<(u64, std::time::SystemTime)>,
}

#[derive(Clone)]
struct SharedScanCodec {
    dicts: Arc<HashMap<u32, zstd::zstd_safe::DDict<'static>>>,
    signature: (i64, i64, i64),
    file_stamp: Option<ScanFileStamp>,
}

fn scan_file_stamp(path: &Path) -> Option<ScanFileStamp> {
    let metadata = path.metadata().ok()?;
    let mut wal_path = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal = match Path::new(&wal_path).metadata() {
        Ok(metadata) => Some((metadata.len(), metadata.modified().ok()?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    Some(ScanFileStamp {
        identity: file_identity(path),
        main: (metadata.len(), metadata.modified().ok()?),
        wal,
    })
}

impl ZstdCodec {
    fn load(conn: &Connection) -> Result<Self> {
        #[cfg(test)]
        CODEC_LOADS.with(|n| n.set(n.get() + 1));
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
        Ok(Self {
            dicts: Arc::new(dicts),
            dctx,
            missed_dict: false,
        })
    }

    fn decode(&mut self, frame: &[u8]) -> Result<Vec<u8>> {
        let result = decode_frame(&self.dicts, &mut self.dctx, frame);
        if matches!(result, Err(FrameError::MissingDict(_))) {
            self.missed_dict = true;
        }
        result.map_err(anyhow::Error::from)
    }
}

#[derive(Debug)]
enum FrameError {
    MissingDict(u32),
    Invalid(String),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::MissingDict(id) => write!(f, "dictionary {id} is missing from zstd_dict"),
            FrameError::Invalid(reason) => f.write_str(reason),
        }
    }
}

impl std::error::Error for FrameError {}

fn decode_frame(
    dicts: &HashMap<u32, zstd::zstd_safe::DDict<'static>>,
    dctx: &mut zstd::zstd_safe::DCtx<'static>,
    frame: &[u8],
) -> std::result::Result<Vec<u8>, FrameError> {
    let size = match zstd::zstd_safe::get_frame_content_size(frame) {
        Ok(Some(size)) if size <= MAX_LINE_BYTES => size as usize,
        other => {
            return Err(FrameError::Invalid(format!(
                "invalid zstd frame (content size {other:?})"
            )))
        }
    };
    // SAFETY: reads at most `frame.len()` bytes of a valid slice.
    let dict_id = unsafe {
        zstd::zstd_safe::zstd_sys::ZSTD_getDictID_fromFrame(frame.as_ptr().cast(), frame.len())
    };
    let Some(ddict) = dicts.get(&dict_id) else {
        return Err(FrameError::MissingDict(dict_id));
    };
    let mut out: Vec<u8> = Vec::with_capacity(size);
    let written = dctx
        .decompress_using_ddict(&mut out, frame, ddict)
        .map_err(|code| {
            FrameError::Invalid(format!(
                "zstd decode failed: {}",
                zstd::zstd_safe::get_error_name(code)
            ))
        })?;
    if written != size {
        return Err(FrameError::Invalid(format!(
            "zstd frame declared {size} bytes and produced {written}"
        )));
    }
    Ok(out)
}

/// One row of [`LineStore::fetch_book_raw`], still encoded as stored.
pub(crate) enum RawRow {
    Text(Vec<u8>),
    Frame(Vec<u8>),
}

/// Decodes [`RawRow::Frame`] rows on any thread, outside the source's lock: the dictionaries
/// are shared with the store that read them, each caller brings its own context.
#[derive(Clone)]
pub(crate) struct FrameDecoder {
    dicts: Arc<HashMap<u32, zstd::zstd_safe::DDict<'static>>>,
}

impl FrameDecoder {
    pub(crate) fn context() -> Result<zstd::zstd_safe::DCtx<'static>> {
        zstd::zstd_safe::DCtx::try_create().context("ZSTD_createDCtx failed")
    }

    pub(crate) fn decode(
        &self,
        dctx: &mut zstd::zstd_safe::DCtx<'static>,
        frame: &[u8],
    ) -> Result<Vec<u8>> {
        Ok(decode_frame(&self.dicts, dctx, frame)?)
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
    /// `line_note` holds footnote bodies moved out of the lines (schema 7).
    notes: bool,
    codec: Option<ZstdCodec>,
    /// What `codec` was loaded from (see [`dict_signature`]).
    dict_signature: Option<(i64, i64, i64)>,
}

/// Row count, last rowid and total size of `zstd_dict`, read without the dictionaries'
/// pages: a commit to any other table must not rebuild megabytes of decoder tables.
fn dict_signature(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok(conn.query_row(
        "SELECT count(*), coalesce(max(rowid), 0), coalesce(sum(length(dict)), 0) \
         FROM zstd_dict",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

impl Layout {
    /// Reuses `previous`'s decoder when `zstd_dict` looks unchanged.
    fn read(conn: &Connection, path: &Path, previous: Option<Layout>) -> Result<Self> {
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
        // The app's `DbCapabilities.hasBookNotes`.
        let notes = has_table("line_note")?
            && conn.query_row(
                "SELECT count(*) FROM pragma_table_info('line_note') \
                 WHERE name IN ('lineId', 'noteIndex', 'charPos', 'marker', 'body')",
                [],
                |r| r.get::<_, i64>(0),
            )? == 5;
        if !has_table("zstd_dict")? {
            return Ok(Self {
                split,
                notes,
                codec: None,
                dict_signature: None,
            });
        }
        let signature = dict_signature(conn)?;
        let codec = match previous {
            Some(Layout {
                codec: Some(codec),
                dict_signature: Some(previous),
                ..
            }) if previous == signature && !codec.missed_dict => codec,
            _ => ZstdCodec::load(conn)?,
        };
        Ok(Self {
            split,
            notes,
            codec: Some(codec),
            dict_signature: Some(signature),
        })
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

/// A book read by [`LineStore::read_book_for_indexing_buffered`].
#[derive(Debug, Default)]
pub(crate) struct IndexingText {
    pub text: crate::indexing_buffers::TextBuffer,
    pub rows: u64,
    /// A row holds `\n`, so the text has more lines than the book has rows.
    pub row_has_newline: bool,
    /// The rows whose embedded `data:` images were removed.
    pub data_uri_rows: crate::indexing_buffers::OrdinalBuffer,
    /// The book has `line_note` rows, put back in its text.
    pub has_notes: bool,
}

/// The app's `_loadBookNotesInIsolate`: the book's `line_note` rows by `lineIndex`, in
/// `noteIndex` order; a row whose values are not of the expected types is skipped.
fn next_book_note(
    rows: &mut rusqlite::Rows<'_>,
    stop: &dyn Fn() -> bool,
) -> Result<Option<(i64, BookNote)>> {
    while let Some(row) = rows.next()? {
        if stop() {
            return Err(IndexingReadStopped.into());
        }
        let (ValueRef::Integer(line_index), ValueRef::Integer(_), ValueRef::Text(body)) =
            (row.get_ref(0)?, row.get_ref(1)?, row.get_ref(4)?)
        else {
            continue;
        };
        let char_pos = match row.get_ref(2)? {
            ValueRef::Integer(char_pos) => char_pos,
            _ => -1,
        };
        return Ok(Some((
            line_index,
            BookNote {
                char_pos,
                has_marker: matches!(row.get_ref(3)?, ValueRef::Text(_)),
                body: String::from_utf8_lossy(body).into_owned(),
            },
        )));
    }
    Ok(None)
}

#[derive(Debug)]
struct IndexingReadStopped;

impl std::fmt::Display for IndexingReadStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("indexing source read interrupted")
    }
}

impl std::error::Error for IndexingReadStopped {}

/// A read-only connection to one library database, with the per-book caches valid for it.
pub(crate) struct LineStore {
    conn: Connection,
    path: PathBuf,
    /// The file this connection opened, to tell a replaced file at the same path.
    identity: Option<FileIdentity>,
    scan_stamp: Option<ScanFileStamp>,
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
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, OpenMode::default())
    }

    pub(crate) fn open_with(path: &Path, mode: OpenMode) -> Result<Self> {
        Self::open_with_scan_codec(path, mode, None)
    }

    fn open_with_scan_codec(
        path: &Path,
        mode: OpenMode,
        cached: Option<SharedScanCodec>,
    ) -> Result<Self> {
        sqlite_host::ensure_ready()?;
        let scan_stamp = scan_file_stamp(path);
        // A plain path, not a `file:` URI: a UNC path (`\\server\share\...`) has no URI
        // form SQLite accepts. Only an immutable file needs the URI's parameter.
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = if mode.immutable {
            Connection::open_with_flags(immutable_uri(path), flags | OpenFlags::SQLITE_OPEN_URI)
        } else {
            Connection::open_with_flags(path, flags)
        }
        .with_context(|| format!("opening the library database {}", path.display()))?;
        if mode.untrusted {
            // The app's `openUntrustedReadOnlyDatabase`: a hostile schema runs nothing unsafe.
            conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
            conn.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA mmap_size=0")?;
        }
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // With the library's sqlite_stat4, a plan that depends on bound values is re-prepared
        // on every new binding: the per-line lookups would each pay a full prepare.
        conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_QPSG, true)?;
        conn.execute_batch(&format!(
            "PRAGMA query_only=1; PRAGMA cache_size=-{PAGE_CACHE_KIB}"
        ))?;
        let data_version = data_version(&conn)?;
        let previous = cached
            .filter(|cached| cached.file_stamp.is_some() && cached.file_stamp == scan_stamp)
            .map(|cached| {
                Ok::<_, anyhow::Error>(Layout {
                    split: false,
                    notes: false,
                    dict_signature: Some(cached.signature),
                    codec: Some(ZstdCodec {
                        dicts: cached.dicts,
                        dctx: zstd::zstd_safe::DCtx::try_create()
                            .context("ZSTD_createDCtx failed")?,
                        missed_dict: false,
                    }),
                })
            })
            .transpose()?;
        let layout = Layout::read(&conn, path, previous)?;
        Ok(Self {
            conn,
            path: path.to_path_buf(),
            identity: file_identity(path),
            scan_stamp,
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
            let previous = std::mem::replace(
                &mut self.layout,
                Layout {
                    split: false,
                    notes: false,
                    codec: None,
                    dict_signature: None,
                },
            );
            self.layout = Layout::read(&self.conn, &self.path, Some(previous))?;
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

    /// The book's text exactly as the app hands it to the indexing path: its rows in order,
    /// each decoded and cleaned like [`prepare_row`], joined with `\n`. `None` when the book
    /// has no rows, or its title is not `title` (when given): the app's read guards a book id
    /// the same way. With `with_notes`, the `line_note` bodies are put back in their lines
    /// (see [`crate::inline_notes`]), as the app does for a book of the library.
    #[cfg(test)]
    pub(crate) fn read_book_for_indexing(
        &mut self,
        book_id: i64,
        title: Option<&str>,
        with_notes: bool,
        stop: &dyn Fn() -> bool,
    ) -> Result<Option<IndexingText>> {
        self.read_book_for_indexing_buffered(book_id, title, with_notes, stop, None, false)
    }

    pub(crate) fn read_book_for_indexing_buffered(
        &mut self,
        book_id: i64,
        title: Option<&str>,
        with_notes: bool,
        stop: &dyn Fn() -> bool,
        directory: Option<&Path>,
        fingerprint_only: bool,
    ) -> Result<Option<IndexingText>> {
        self.in_read_txn(|store| {
            let Self { conn, layout, .. } = &mut *store;
            if let Some(title) = title {
                let matches = conn
                    .prepare_cached("SELECT 1 FROM book WHERE id = ?1 AND title = ?2")?
                    .query_row(rusqlite::params![book_id, title], |_| Ok(()))
                    .optional()?
                    .is_some();
                if !matches {
                    return Ok(None);
                }
            }
            let mut note_stmt = if with_notes && layout.notes {
                Some(conn.prepare_cached(
                    "SELECT l.lineIndex, n.noteIndex, n.charPos, n.marker, n.body \
                     FROM line l JOIN line_note n ON n.lineId = l.id \
                     WHERE l.bookId = ?1 ORDER BY l.lineIndex, n.noteIndex",
                )?)
            } else {
                None
            };
            let mut note_query = note_stmt
                .as_mut()
                .map(|stmt| stmt.query([book_id]))
                .transpose()?;
            let mut next_note = match &mut note_query {
                Some(query) => next_book_note(query, stop)?,
                None => None,
            };
            let has_notes = next_note.is_some();
            let mut stmt = conn.prepare_cached(layout.book_sql())?;
            let mut query = stmt.query([book_id])?;
            let mut book = IndexingText {
                text: if fingerprint_only {
                    crate::indexing_buffers::TextBuffer::fingerprinting()
                } else {
                    crate::indexing_buffers::TextBuffer::in_directory(1 << 20, directory)
                },
                data_uri_rows: crate::indexing_buffers::OrdinalBuffer::in_directory(directory),
                has_notes,
                ..IndexingText::default()
            };
            let mut first_row_bom = false;
            let mut has_data_uri = false;
            // The line of the app's joined text that this row starts.
            let mut text_line = 0i64;
            while let Some(row) = query.next()? {
                if stop() {
                    return Err(IndexingReadStopped.into());
                }
                let mut bytes =
                    row_bytes(row.get_ref(0)?, &mut layout.codec).map_err(|reason| {
                        anyhow::anyhow!(
                            "book {book_id} row {} cannot be decoded: {reason}",
                            book.rows
                        )
                    })?;
                if book.rows > 0 {
                    book.text.push_str("\n")?;
                }
                book.row_has_newline |= bytes.contains(&b'\n');
                if book.has_notes {
                    // The app decodes the book as one string to put the bodies back, which
                    // drops the BOM, and cleans `data:` off the result.
                    let raw = String::from_utf8_lossy(&bytes);
                    let row_text = if book.rows == 0 {
                        raw.strip_prefix('\u{FEFF}').unwrap_or(&raw)
                    } else {
                        &raw
                    };
                    let next_line = text_line + 1 + row_text.matches('\n').count() as i64;
                    let mut notes = BookNotes::new();
                    while next_note
                        .as_ref()
                        .is_some_and(|(line, _)| *line < next_line)
                    {
                        let (line, note) = next_note.take().expect("a pending note");
                        if line >= text_line {
                            notes.entry(line).or_default().push(note);
                        }
                        next_note =
                            next_book_note(note_query.as_mut().expect("the notes query"), stop)?;
                    }
                    let rehydrated =
                        crate::inline_notes::rehydrate_row(row_text, text_line, &notes);
                    text_line = next_line;
                    let replaced = match rehydrated {
                        Some(rehydrated) => Some(rehydrated.into_bytes()),
                        None if row_text.len() != raw.len() => Some(row_text.as_bytes().to_vec()),
                        None => None,
                    };
                    if let Some(replaced) = replaced {
                        bytes = Cow::Owned(replaced);
                    }
                }
                if contains_data_scheme(&bytes) {
                    has_data_uri = true;
                    let raw = String::from_utf8_lossy(&bytes);
                    if !fingerprint_only && strip_data_uris_for_index(&raw).len() != raw.len() {
                        book.data_uri_rows.push(book.rows as u32)?;
                    }
                }
                first_row_bom |= book.rows == 0 && bytes.starts_with(BOM);
                book.text.push_str(&prepare_row(&bytes, false))?;
                book.rows += 1;
            }
            if book.rows == 0 {
                return Ok(None);
            }
            if first_row_bom && has_data_uri {
                book.text.skip_prefix(BOM.len() as u64);
            }
            Ok(Some(book))
        })
    }

    /// Every row of `book_id` in `lineIndex` order, undecoded, with the decoder of its frames:
    /// a whole-book scan decodes in parallel after the lock is released. `None` when the book
    /// does not exist.
    #[cfg(test)]
    pub fn fetch_book_raw(
        &mut self,
        book_id: i64,
    ) -> Result<Option<(Vec<RawRow>, Option<FrameDecoder>)>> {
        self.fetch_book_raw_checked(book_id, &|| false)
    }

    pub(crate) fn fetch_book_raw_checked(
        &mut self,
        book_id: i64,
        stop: &dyn Fn() -> bool,
    ) -> Result<Option<(Vec<RawRow>, Option<FrameDecoder>)>> {
        if stop() {
            return Err(IndexingReadStopped.into());
        }
        self.in_read_txn(|store| {
            let Self { conn, layout, .. } = &mut *store;
            let mut stmt = conn.prepare_cached(layout.book_sql())?;
            let mut query = stmt.query([book_id])?;
            let mut rows = Vec::new();
            while let Some(row) = query.next()? {
                if stop() {
                    return Err(IndexingReadStopped.into());
                }
                rows.push(match row.get_ref(0)? {
                    ValueRef::Blob(frame) => RawRow::Frame(frame.to_vec()),
                    ValueRef::Text(bytes) => RawRow::Text(bytes.to_vec()),
                    _ => RawRow::Text(Vec::new()),
                });
            }
            if rows.is_empty() {
                return Ok(None);
            }
            let decoder = layout.codec.as_ref().map(|codec| FrameDecoder {
                dicts: codec.dicts.clone(),
            });
            Ok(Some((rows, decoder)))
        })
    }

    /// The rows at `ordinals` of `book_id` (positions in `lineIndex` order), undecoded, in the
    /// order given; `None` for an ordinal past the book's end. Unlike [`Self::fetch_window`],
    /// no row is checked against its `lineCheck`: a scan reads candidates, not results.
    #[cfg(test)]
    pub fn fetch_rows_raw(
        &mut self,
        book_id: i64,
        ordinals: &[u64],
    ) -> Result<(Vec<Option<RawRow>>, Option<FrameDecoder>)> {
        self.fetch_rows_raw_checked(book_id, ordinals, &|| false)
    }

    pub(crate) fn fetch_rows_raw_checked(
        &mut self,
        book_id: i64,
        ordinals: &[u64],
        stop: &dyn Fn() -> bool,
    ) -> Result<(Vec<Option<RawRow>>, Option<FrameDecoder>)> {
        if stop() {
            return Err(IndexingReadStopped.into());
        }
        self.in_read_txn(|store| {
            let ids: Vec<Option<i64>> = {
                let map = store.row_ids_checked(book_id, stop)?;
                ordinals.iter().map(|&o| map.get(o)).collect()
            };
            let Self { conn, layout, .. } = &mut *store;
            let mut stmt = conn.prepare_cached(if layout.split {
                "SELECT content FROM line_content WHERE id = ?1"
            } else {
                "SELECT content FROM line WHERE id = ?1"
            })?;
            let mut rows = Vec::with_capacity(ids.len());
            for id in ids {
                if stop() {
                    return Err(IndexingReadStopped.into());
                }
                let Some(id) = id else {
                    rows.push(None);
                    continue;
                };
                let mut query = stmt.query([id])?;
                rows.push(Some(match query.next()? {
                    Some(row) => match row.get_ref(0)? {
                        ValueRef::Blob(frame) => RawRow::Frame(frame.to_vec()),
                        ValueRef::Text(bytes) => RawRow::Text(bytes.to_vec()),
                        _ => RawRow::Text(Vec::new()),
                    },
                    None => RawRow::Text(Vec::new()),
                }));
            }
            let decoder = layout.codec.as_ref().map(|codec| FrameDecoder {
                dicts: codec.dicts.clone(),
            });
            Ok((rows, decoder))
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
        self.row_ids_checked(book_id, &|| false)
    }

    fn row_ids_checked(&mut self, book_id: i64, stop: &dyn Fn() -> bool) -> Result<&RowIds> {
        if !self.books.contains(&book_id) {
            // idx_line_book_index covers this: the table's pages are never read.
            let mut statement = self
                .conn
                .prepare_cached("SELECT id FROM line WHERE bookId = ?1 ORDER BY lineIndex, id")?;
            let mut rows = statement.query([book_id])?;
            let mut ids = Vec::new();
            while let Some(row) = rows.next()? {
                if stop() {
                    return Err(IndexingReadStopped.into());
                }
                ids.push(row.get::<_, i64>(0)?);
            }
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

/// Device and inode where a file can be replaced while open (POSIX); elsewhere an open
/// database cannot be replaced, so every path is its own identity.
#[cfg(unix)]
type FileIdentity = (u64, u64);
#[cfg(not(unix))]
type FileIdentity = ();

#[cfg(unix)]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_identity(path: &Path) -> Option<FileIdentity> {
    path.exists().then_some(())
}

/// How a source's file is opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct OpenMode {
    /// A database the app does not control (an attached library).
    pub untrusted: bool,
    /// Opened with `immutable=1`: no locks and no side files, for a WAL database or a folder
    /// without write access. SQLite then never sees a change to the file.
    pub immutable: bool,
}

/// The `file:` URI of `path` with `mode=ro&immutable=1`, like the app's `immutableDatabaseUri`.
fn immutable_uri(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    // `file:///C:/x`, `file:///home/x`, and `file:////server/share/x` (empty authority).
    let mut uri = String::from(if raw.starts_with('/') {
        "file://"
    } else {
        "file:///"
    });
    for c in raw.chars() {
        match c {
            '%' => uri.push_str("%25"),
            '?' => uri.push_str("%3F"),
            '#' => uri.push_str("%23"),
            c => uri.push(c),
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    uri
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

// ── Process-global sources ─────────────────────────────────────────────────────

/// Which database a `LibraryDb` document's text is read from, chosen by its `filePath`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Source {
    /// Official books (`id:<bookId>`) in the library database (`seforim.db`).
    Library,
    /// Personal books (`uid:<bookId>`) in the user-books database, which the app keeps
    /// writing (WAL) while the engine reads it.
    UserBooks,
    /// Books (`db:<slug>:<bookId>`) of one attached database.
    Attached(AttachedRef),
}

impl Source {
    /// Every source, the attached ones in the order their slugs were first seen.
    pub(crate) fn all() -> Vec<Source> {
        let attached = ATTACHED.read().unwrap_or_else(|p| p.into_inner());
        [Source::Library, Source::UserBooks]
            .into_iter()
            .chain(
                attached
                    .iter()
                    .map(|&cell| Source::Attached(AttachedRef(cell))),
            )
            .collect()
    }

    fn cell(self) -> &'static Mutex<Global> {
        match self {
            Source::Library => &LIBRARY,
            Source::UserBooks => &USER_BOOKS,
            Source::Attached(attached) => &attached.0.global,
        }
    }

    fn scans_done(self) -> &'static Condvar {
        match self {
            Source::Library => &LIBRARY_SCANS_DONE,
            Source::UserBooks => &USER_BOOKS_SCANS_DONE,
            Source::Attached(attached) => &attached.0.scans_done,
        }
    }

    /// The source of the attached database `slug`, registered on first use; `None` for a
    /// slug the app refuses (`BookSource.isValidSlug`).
    pub(crate) fn attached(slug: &str) -> Option<Source> {
        static SLUG: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(|| {
            regex::Regex::new(r"^[\p{L}\p{N}._-]{1,64}$").expect("valid pattern")
        });
        if let Some(found) = Self::registered_attached(slug) {
            return Some(found);
        }
        if !SLUG.is_match(slug) {
            return None;
        }
        let mut attached = ATTACHED.write().unwrap_or_else(|p| p.into_inner());
        if let Some(&cell) = attached.iter().find(|cell| &*cell.slug == slug) {
            return Some(Source::Attached(AttachedRef(cell)));
        }
        // Never freed: one small cell per slug the process ever sees.
        let cell: &'static AttachedCell = Box::leak(Box::new(AttachedCell {
            id: attached.len() as u32,
            slug: slug.into(),
            global: Mutex::new(Global::new()),
            scans_done: Condvar::new(),
        }));
        attached.push(cell);
        Some(Source::Attached(AttachedRef(cell)))
    }

    fn registered_attached(slug: &str) -> Option<Source> {
        ATTACHED
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .find(|cell| &*cell.slug == slug)
            .map(|&cell| Source::Attached(AttachedRef(cell)))
    }
}

/// One attached database's source state.
pub(crate) struct AttachedCell {
    /// Registration order.
    id: u32,
    slug: Box<str>,
    global: Mutex<Global>,
    scans_done: Condvar,
}

/// An attached source; compared by registration order, shown by its slug.
#[derive(Clone, Copy)]
pub(crate) struct AttachedRef(&'static AttachedCell);

impl PartialEq for AttachedRef {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for AttachedRef {}

impl std::hash::Hash for AttachedRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl PartialOrd for AttachedRef {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AttachedRef {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.id.cmp(&other.0.id)
    }
}

impl std::fmt::Debug for AttachedRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0.slug)
    }
}

static ATTACHED: RwLock<Vec<&'static AttachedCell>> = RwLock::new(Vec::new());

/// Why a window could not be served.
#[derive(Debug, Clone)]
pub(crate) enum SourceUnavailable {
    Unconfigured,
    Suspended,
    /// A writer holds the database; retried after [`BUSY_BACKOFF`].
    Busy,
    /// The configured file does not exist (yet); checked again after [`BUSY_BACKOFF`].
    Missing,
    /// Opening or reading failed; the reason has been logged.
    Failed,
}

struct Global {
    path: Option<PathBuf>,
    mode: OpenMode,
    store: Option<LineStore>,
    scan_codec: Option<SharedScanCodec>,
    generation: u64,
    suspend_depth: u32,
    owner_ports: Vec<i64>,
    busy_until: Option<Instant>,
    library_fallbacks: u64,
    /// A missing file was reported (once per path or reappearance).
    missing_logged: bool,
    /// The file was missing: windows do not look again before this.
    missing_until: Option<Instant>,
    /// `PRAGMA data_version` the semantic cache key last saw on the open store.
    seen_version: Option<i64>,
    /// Commits the semantic cache key has seen on the open store.
    #[cfg_attr(not(feature = "semantic-integration"), allow(dead_code))]
    content_epoch: u64,
    /// Open [`ScanSession`]s: suspending or moving the source waits for them to close.
    scans: u32,
}

impl Global {
    const fn new() -> Self {
        Global {
            path: None,
            mode: OpenMode {
                untrusted: false,
                immutable: false,
            },
            store: None,
            scan_codec: None,
            generation: 0,
            suspend_depth: 0,
            owner_ports: Vec::new(),
            busy_until: None,
            library_fallbacks: 0,
            missing_logged: false,
            missing_until: None,
            seen_version: None,
            content_epoch: 0,
            scans: 0,
        }
    }
}

static LIBRARY: Mutex<Global> = Mutex::new(Global::new());
static USER_BOOKS: Mutex<Global> = Mutex::new(Global::new());
static LIBRARY_SCANS_DONE: Condvar = Condvar::new();
static USER_BOOKS_SCANS_DONE: Condvar = Condvar::new();

fn global(source: Source) -> MutexGuard<'static, Global> {
    let cell = source.cell();
    let mut g = cell.lock().unwrap_or_else(|poisoned| {
        // A panic inside a window can leave its read transaction open, holding the file's
        // shared lock: close the connection, and the caches with it.
        let mut g = poisoned.into_inner();
        g.drop_store();
        cell.clear_poison();
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
        self.scan_codec = None;
        self.seen_version = None;
        if let Some(store) = self.store.take() {
            store.close();
            // SQLite's data_version is local to a connection. A reopened connection can
            // report the same number for different text, so its query-cache keys must
            // never reuse those of the closed connection.
            self.generation += 1;
        }
    }

    fn set_path(&mut self, path: Option<PathBuf>, mode: OpenMode) {
        self.scan_codec = None;
        if self.path != path || self.mode != mode {
            self.drop_store();
            self.path = path;
            self.mode = mode;
            self.generation += 1;
            self.missing_logged = false;
        } else if let Some(store) = &self.store {
            // The app may have replaced the file under the same path (POSIX keeps the old
            // inode open): reopen lazily from the new one.
            if file_identity(&store.path) != store.identity {
                self.drop_store();
            }
        }
        self.busy_until = None;
        self.missing_until = None;
    }
}

/// Snapshot of one source, for `line_source_status`.
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
    let mut g = global(Source::Library);
    let generation = g.generation;
    g.set_path(Some(PathBuf::from(db_path)), OpenMode::default());
    if g.generation != generation {
        wait_for_scans(g, Source::Library);
    }
    Ok(())
}

/// `None` closes the user-books database (waiting for a window reading it) and leaves its
/// documents `Unavailable` until it is configured again.
pub(crate) fn configure_user_books(db_path: Option<&str>) -> Result<()> {
    if db_path == Some("") {
        anyhow::bail!("configure_user_books_line_source needs a database path or None");
    }
    let mut g = global(Source::UserBooks);
    let generation = g.generation;
    g.set_path(db_path.map(PathBuf::from), OpenMode::default());
    if g.generation != generation {
        wait_for_scans(g, Source::UserBooks);
    }
    Ok(())
}

/// [`configure_user_books`] for the attached database `slug`, opened hardened, and with
/// `immutable=1` when asked.
pub(crate) fn configure_attached(slug: &str, db_path: Option<&str>, immutable: bool) -> Result<()> {
    if db_path == Some("") {
        anyhow::bail!("configure_attached_line_source needs a database path or None");
    }
    let source = match db_path {
        Some(_) => Source::attached(slug),
        // A slug never configured has nothing to close.
        None => match Source::registered_attached(slug) {
            Some(source) => Some(source),
            None => return Ok(()),
        },
    }
    .with_context(|| format!("invalid attached database slug {slug:?}"))?;
    let mut g = global(source);
    let generation = g.generation;
    let mode = OpenMode {
        untrusted: true,
        immutable,
    };
    g.set_path(db_path.map(PathBuf::from), mode);
    if g.generation != generation {
        wait_for_scans(g, source);
    }
    Ok(())
}

/// The state of the attached database `slug`; `None` when it was never configured.
pub(crate) fn attached_status(slug: &str) -> Option<Status> {
    Source::registered_attached(slug).map(status)
}

pub(crate) fn suspend() {
    let mut g = global(Source::Library);
    g.suspend_depth += 1;
    g.drop_store();
    wait_for_scans(g, Source::Library);
}

pub(crate) fn resume() -> Result<()> {
    let mut g = global(Source::Library);
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
    let mut g = global(Source::Library);
    if owner_port <= 0 || !owner_port_is_open(owner_port) {
        anyhow::bail!("line-source suspension needs an open Dart owner port");
    }
    if !g.owner_ports.contains(&owner_port) {
        g.owner_ports.push(owner_port);
    }
    g.drop_store();
    wait_for_scans(g, Source::Library);
    Ok(())
}

pub(crate) fn resume_owned(owner_port: i64) {
    let mut g = global(Source::Library);
    let before = g.owner_ports.len();
    g.owner_ports.retain(|&port| port != owner_port);
    if before != g.owner_ports.len() && g.owner_ports.is_empty() && g.suspend_depth == 0 {
        g.drop_store();
        g.generation += 1;
        g.busy_until = None;
    }
}

pub(crate) fn status(source: Source) -> Status {
    let g = global(source);
    Status {
        configured: g.path.is_some(),
        open: g.store.is_some(),
        suspend_depth: g.suspend_depth + g.owner_ports.len() as u32,
        generation: g.generation,
        library_fallbacks: g.library_fallbacks,
    }
}

/// One source's input to a query cache: generation, data_version (the library)
/// or commits seen (user books), suspended.
pub(crate) type SourceGeneration = (u64, Option<i64>, bool);

/// The input of every source to the semantic query cache, sampled before its lookup.
///
/// Read the connection's current data_version, not the version of the row caches: a
/// lexical window may already have refreshed those after an unpaused write. This reads
/// metadata only, without opening a database or reading/decoding any book content.
#[cfg(feature = "semantic-integration")]
pub(crate) fn query_cache_generation() -> [SourceGeneration; 3] {
    [
        source_cache_generation(Source::Library),
        user_books_cache_generation(),
        attached_cache_generation(),
    ]
}

pub(crate) fn search_snapshot_generation() -> [SourceGeneration; 3] {
    use std::hash::{Hash, Hasher};
    let mut attached = std::collections::hash_map::DefaultHasher::new();
    let mut suspended = false;
    for source in Source::all() {
        if let Source::Attached(_) = source {
            let generation = snapshot_source_generation(source);
            suspended |= generation.2;
            (source, generation).hash(&mut attached);
        }
    }
    [
        snapshot_source_generation(Source::Library),
        snapshot_source_generation(Source::UserBooks),
        (attached.finish(), None, suspended),
    ]
}

fn snapshot_source_generation(source: Source) -> SourceGeneration {
    let mut g = global(source);
    if let Some(version) = g.store.as_ref().map(|store| data_version(&store.conn)) {
        match version {
            Ok(version) => {
                if g.seen_version.is_some_and(|seen| seen != version) {
                    g.content_epoch += 1;
                }
                g.seen_version = Some(version);
            }
            Err(_) => g.generation += 1,
        }
    }
    (
        g.generation,
        Some(g.content_epoch as i64),
        g.suspend_depth > 0 || !g.owner_ports.is_empty(),
    )
}

/// Every attached source's [`source_cache_generation`], folded into one.
#[cfg(feature = "semantic-integration")]
fn attached_cache_generation() -> SourceGeneration {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let mut suspended = false;
    for source in Source::all() {
        if let Source::Attached(_) = source {
            let generation = source_cache_generation(source);
            suspended |= generation.2;
            (source, generation).hash(&mut hasher);
        }
    }
    (hasher.finish(), None, suspended)
}

/// The user-books part of the key never opens the file, and a closed or newly opened store
/// keeps the key: only a commit seen on an open store (or a reconfigure) changes it. The
/// app writes other tables of that file all the time, so an unused store must not count.
#[cfg(feature = "semantic-integration")]
fn user_books_cache_generation() -> SourceGeneration {
    let mut g = global(Source::UserBooks);
    if let Some(version) = g.store.as_ref().map(|store| data_version(&store.conn)) {
        match version {
            Ok(version) => {
                if g.seen_version.is_some_and(|seen| seen != version) {
                    g.content_epoch += 1;
                }
                g.seen_version = Some(version);
            }
            // As for the library: an unverifiable source admits no cached answer.
            Err(_) => g.generation += 1,
        }
    }
    (g.generation, Some(g.content_epoch as i64), false)
}

fn source_cache_generation(source: Source) -> SourceGeneration {
    let mut g = global(source);
    let version = if g.busy_until.is_some_and(|until| Instant::now() < until) {
        g.generation += 1;
        None
    } else {
        match g
            .store
            .as_ref()
            .map(|store| data_version(&store.conn))
            .transpose()
        {
            Ok(version) => version,
            Err(err) => {
                if is_busy(&err) {
                    // Share the window's backoff: do not wait for the same writer again
                    // when the cache miss proceeds to resolving its candidate lines.
                    g.busy_until = Some(Instant::now() + BUSY_BACKOFF);
                }
                // An unverifiable source must not admit a previously verified cached answer.
                // Give each failed check a fresh key; a successful retry can be cached again.
                g.generation += 1;
                None
            }
        }
    };
    (
        g.generation,
        version,
        g.suspend_depth > 0 || !g.owner_ports.is_empty(),
    )
}

/// גרסת המקור למטמון הסריקה; גם סריקה בחיבורים נפרדים צריכה חיבור קבוע לזיהוי כתיבות.
pub(crate) fn scan_cache_generation(source: Source) -> SourceGeneration {
    let _ = with_store(source, |_| Ok(()));
    source_cache_generation(source)
}

pub(crate) fn release_memory() {
    for source in Source::all() {
        let mut g = global(source);
        g.scan_codec = None;
        if let Some(store) = g.store.as_mut() {
            store.books.clear();
            store.book_bytes = 0;
            store.at_line_index = HashMap::new();
            store.data_uri_books = HashMap::new();
            store.conn.flush_prepared_statement_cache();
            let _ = store.conn.execute_batch("PRAGMA shrink_memory");
        }
    }
}

/// Counts a book asked for as `LibraryDb` and stored `InIndex`.
pub(crate) fn note_library_fallback(source: Source) {
    global(source).library_fallbacks += 1;
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

/// Runs `f` against the source's store, opening it on first use. Holding the mutex for the
/// window is what lets `suspend` wait for an in-flight window before closing the file.
pub(crate) fn with_store<R>(
    source: Source,
    f: impl FnOnce(&mut LineStore) -> Result<R>,
) -> std::result::Result<R, SourceUnavailable> {
    with_store_inner(source, true, f)
}

/// [`with_store`] for indexing, which may wait out one busy timeout: a window's busy
/// backoff would otherwise store whole books in the index.
pub(crate) fn with_store_for_indexing<R>(
    source: Source,
    f: impl FnOnce(&mut LineStore) -> Result<R>,
) -> std::result::Result<R, SourceUnavailable> {
    with_store_inner(source, false, f)
}

struct SourceReadLockMeasurement {
    wait: Duration,
    acquired: Instant,
}

impl Drop for SourceReadLockMeasurement {
    fn drop(&mut self) {
        crate::indexing_diagnostics::source_lock(self.wait, self.acquired.elapsed());
    }
}

fn with_store_inner<R>(
    source: Source,
    honor_backoff: bool,
    f: impl FnOnce(&mut LineStore) -> Result<R>,
) -> std::result::Result<R, SourceUnavailable> {
    let lock_started = Instant::now();
    let mut g = global(source);
    let lock_acquired = Instant::now();
    let _measurement = SourceReadLockMeasurement {
        wait: lock_started.elapsed(),
        acquired: lock_acquired,
    };
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
    if g.store.is_none()
        && honor_backoff
        && g.missing_until.is_some_and(|until| Instant::now() < until)
    {
        return Err(SourceUnavailable::Missing);
    }
    if g.store.is_none() && matches!(path.try_exists(), Ok(false)) {
        // Normal for user books: the app creates that file with the first personal book.
        if !g.missing_logged {
            log::info!("{source:?} database {} does not exist", path.display());
            g.missing_logged = true;
        }
        g.missing_until = Some(Instant::now() + BUSY_BACKOFF);
        return Err(SourceUnavailable::Missing);
    }
    let result = match g.store.as_mut() {
        Some(store) => f(store),
        None => LineStore::open_with(&path, g.mode).and_then(|store| {
            g.missing_logged = false;
            g.seen_version = Some(store.data_version);
            f(g.store.insert(store))
        }),
    };
    result.map_err(|err| {
        if is_busy(&err) {
            // The connection is fine; only the writer has to finish.
            log::warn!("{source:?} database is busy; skipping it for {BUSY_BACKOFF:?}: {err:#}");
            g.busy_until = Some(Instant::now() + BUSY_BACKOFF);
            SourceUnavailable::Busy
        } else {
            // A failed read may mean the file changed under us; reopen next time.
            log::warn!("{source:?} line source unavailable: {err:#}");
            g.drop_store();
            SourceUnavailable::Failed
        }
    })
}

/// Returns once no [`ScanSession`] of `source` is open. A session looks at the source before
/// each book it reads, so it closes within one book of a suspension or a new path.
fn wait_for_scans(mut g: MutexGuard<'static, Global>, source: Source) {
    while g.scans > 0 {
        g = source
            .scans_done()
            .wait(g)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
}

/// Read-only connections of their own for a whole-book scan, which reads many books in
/// parallel, outside the source's lock and its single connection. The source's state is
/// looked at before every read: once it is suspended or moved, reads fail
/// [`SourceUnavailable::Suspended`], and suspending waits for the session to be dropped.
pub(crate) struct ScanSession {
    source: Source,
    path: PathBuf,
    mode: OpenMode,
    generation: u64,
    idle: Mutex<Vec<LineStore>>,
}

/// Opens a scan session of `source`, refused like a window when the source cannot be read.
pub(crate) fn begin_scan(source: Source) -> std::result::Result<ScanSession, SourceUnavailable> {
    let mut g = global(source);
    if g.suspend_depth > 0 || !g.owner_ports.is_empty() {
        return Err(SourceUnavailable::Suspended);
    }
    let Some(path) = g.path.clone() else {
        return Err(SourceUnavailable::Unconfigured);
    };
    if matches!(path.try_exists(), Ok(false)) {
        return Err(SourceUnavailable::Missing);
    }
    g.scans += 1;
    Ok(ScanSession {
        source,
        path,
        mode: g.mode,
        generation: g.generation,
        idle: Mutex::new(Vec::new()),
    })
}

impl ScanSession {
    pub(crate) fn is_current(&self) -> bool {
        let started = Instant::now();
        let g = global(self.source);
        let acquired = Instant::now();
        let valid =
            g.suspend_depth == 0 && g.owner_ports.is_empty() && g.generation == self.generation;
        crate::indexing_diagnostics::source_lock(
            acquired.duration_since(started),
            acquired.elapsed(),
        );
        valid
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Runs `f` on one of the session's connections, opening one when all are busy.
    pub(crate) fn read<R>(
        &self,
        f: impl FnOnce(&mut LineStore) -> Result<R>,
    ) -> std::result::Result<R, SourceUnavailable> {
        let cached = {
            let g = global(self.source);
            if g.suspend_depth > 0 || !g.owner_ports.is_empty() || g.generation != self.generation {
                return Err(SourceUnavailable::Suspended);
            }
            g.scan_codec.clone()
        };
        let idle = self.idle.lock().unwrap_or_else(|p| p.into_inner()).pop();
        let mut store = match idle {
            Some(store) => store,
            None => {
                LineStore::open_with_scan_codec(&self.path, self.mode, cached).map_err(|err| {
                    if is_busy(&err) {
                        return SourceUnavailable::Busy;
                    }
                    log::warn!("{:?} scan connection failed: {err:#}", self.source);
                    SourceUnavailable::Failed
                })?
            }
        };
        let result = f(&mut store);
        let current_stamp = scan_file_stamp(&self.path);
        let codec = store
            .layout
            .codec
            .as_ref()
            .filter(|codec| !codec.missed_dict)
            .zip(store.layout.dict_signature)
            .filter(|_| current_stamp.is_some() && store.scan_stamp == current_stamp)
            .map(|(codec, signature)| SharedScanCodec {
                dicts: codec.dicts.clone(),
                signature,
                file_stamp: current_stamp,
            });
        {
            let mut g = global(self.source);
            if g.generation == self.generation && g.suspend_depth == 0 && g.owner_ports.is_empty() {
                g.scan_codec = codec;
            }
        }
        self.idle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(store);
        result.map_err(|err| {
            if err.is::<IndexingReadStopped>() {
                return SourceUnavailable::Suspended;
            }
            if is_busy(&err) {
                SourceUnavailable::Busy
            } else {
                log::warn!("{:?} scan read failed: {err:#}", self.source);
                SourceUnavailable::Failed
            }
        })
    }
}

impl Drop for ScanSession {
    fn drop(&mut self) {
        for store in self
            .idle
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .drain(..)
        {
            store.close();
        }
        let mut g = global(self.source);
        g.scans -= 1;
        drop(g);
        self.source.scans_done().notify_all();
    }
}

/// Serializes tests that touch the process-global source.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Resets the global source between tests.
#[cfg(test)]
pub(crate) fn reset_for_tests() {
    for source in Source::all() {
        let mut g = global(source);
        g.drop_store();
        g.path = None;
        g.suspend_depth = 0;
        g.owner_ports.clear();
        g.generation += 1;
        g.busy_until = None;
        g.missing_until = None;
        g.missing_logged = false;
    }
}

/// The books whose ordinal maps the open store holds, ascending.
#[cfg(test)]
pub(crate) fn mapped_books_for_tests() -> Vec<i64> {
    let g = global(Source::Library);
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
    global(Source::Library)
        .store
        .as_ref()
        .map(LineStore::memory)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "semantic-integration")]
    #[test]
    fn query_cache_generation_tracks_commits_and_connection_reopens() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(
            "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
             INSERT INTO line VALUES (1, 1, 0, 'before');"
        ).unwrap();
        configure(path.to_str().unwrap()).unwrap();
        let closed = source_cache_generation(Source::Library);
        assert!(closed.1.is_none());
        assert!(
            !status(Source::Library).open,
            "the cache check must not open an unused source"
        );
        let fetch = || {
            with_store(Source::Library, |store| {
                store.fetch_window(
                    &[LineKey {
                        book_id: 1,
                        ordinal: 0,
                    }],
                    &[None],
                )
            })
            .unwrap()
        };
        fetch();
        let before = source_cache_generation(Source::Library);
        assert!(before.1.is_some());
        assert_eq!(
            before,
            source_cache_generation(Source::Library),
            "unchanged queries keep their cache key"
        );
        writer
            .execute("UPDATE line SET content = 'after'", [])
            .unwrap();
        // Another window can consume the data_version change first; it must not hide
        // the commit from the query cache's later check.
        assert!(matches!(&fetch()[0], RowText::Found(text) if text == "after"));
        let after = source_cache_generation(Source::Library);
        assert_ne!(before, after);
        assert_eq!(after, source_cache_generation(Source::Library));
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let busy = source_cache_generation(Source::Library);
        assert!(busy.1.is_none());
        assert!(
            global(Source::Library).busy_until.is_some(),
            "the metadata check shares the window's backoff"
        );
        assert_ne!(
            busy,
            source_cache_generation(Source::Library),
            "unverifiable answers cannot be reused"
        );
        assert!(matches!(
            with_store(Source::Library, |_| Ok(())),
            Err(SourceUnavailable::Busy)
        ));
        writer.execute_batch("COMMIT").unwrap();
        // Advance the backoff without sleeping: a successful retry needs a new key.
        global(Source::Library).busy_until = Some(Instant::now());
        assert_ne!(busy, source_cache_generation(Source::Library));
        global(Source::Library).drop_store();
        assert_ne!(
            closed,
            source_cache_generation(Source::Library),
            "closed-source keys are not reused either"
        );
        fetch();
        assert_ne!(
            before,
            source_cache_generation(Source::Library),
            "data_version restarts on a new connection"
        );
        assert_eq!(
            before.1,
            source_cache_generation(Source::Library).1,
            "the reopened connection reuses the old SQLite counter in this fixture"
        );
        reset_for_tests();
    }

    #[cfg(feature = "semantic-integration")]
    #[test]
    fn query_cache_generation_covers_the_user_books_source() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_books.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
             INSERT INTO line VALUES (1, 1, 0, 'before');",
        ).unwrap();
        let unconfigured = query_cache_generation();
        configure_user_books(Some(path.to_str().unwrap())).unwrap();
        let configured = query_cache_generation();
        assert_eq!(
            configured[0], unconfigured[0],
            "the library source is untouched"
        );
        assert_ne!(configured[1], unconfigured[1]);
        // Configured but never read: the key opens nothing, and the app's commits keep it.
        writer
            .execute("UPDATE line SET content = 'unread'", [])
            .unwrap();
        assert_eq!(query_cache_generation(), configured);
        assert!(!status(Source::UserBooks).open);
        // Opening the store for a window does not change the key either.
        with_store(Source::UserBooks, |store| store.book_row_count(1)).unwrap();
        let opened = query_cache_generation();
        assert_eq!(opened, configured);
        writer
            .execute("UPDATE line SET content = 'after'", [])
            .unwrap();
        let written = query_cache_generation();
        assert_ne!(
            written[1], opened[1],
            "a commit to a read store is another answer"
        );
        assert_eq!(written, query_cache_generation());
        configure_user_books(None).unwrap();
        assert_ne!(query_cache_generation()[1], written[1]);
        assert!(!status(Source::UserBooks).configured);
        reset_for_tests();
    }

    #[cfg(feature = "semantic-integration")]
    #[test]
    fn query_cache_generation_covers_attached_sources() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attached.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(
            "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
             INSERT INTO line VALUES (1, 1, 0, 'before');",
        ).unwrap();
        let before = query_cache_generation();
        configure_attached("cache-key", Some(path.to_str().unwrap()), false).unwrap();
        let configured = query_cache_generation();
        assert_eq!(configured[..2], before[..2]);
        assert_ne!(configured[2], before[2]);
        let source = Source::attached("cache-key").unwrap();
        with_store(source, |store| store.book_row_count(1)).unwrap();
        let opened = query_cache_generation();
        writer
            .execute("UPDATE line SET content = 'after'", [])
            .unwrap();
        assert_ne!(
            query_cache_generation()[2],
            opened[2],
            "a commit is another answer"
        );
        configure_attached("cache-key", None, false).unwrap();
        assert_ne!(query_cache_generation()[2], opened[2]);
        reset_for_tests();
    }

    #[test]
    fn attached_sources_are_registered_once_per_valid_slug() {
        let a = Source::attached("ספרייה.1_x-y").unwrap();
        assert_eq!(Source::attached("ספרייה.1_x-y"), Some(a));
        assert!(Source::all().contains(&a));
        assert_ne!(Source::attached("other-slug"), Some(a));
        assert_eq!(format!("{a:?}"), "Attached(\"ספרייה.1_x-y\")");
        for bad in ["", "a:b", "a b", "a/b", &"x".repeat(65)] {
            assert_eq!(Source::attached(bad), None, "{bad}");
        }
        assert!(Source::attached(&"x".repeat(64)).is_some());
    }

    #[test]
    fn immutable_uris_follow_the_app() {
        assert_eq!(
            immutable_uri(Path::new("C:\\a b\\מסד#1%?.db")),
            "file:///C:/a b/מסד%231%25%3F.db?mode=ro&immutable=1"
        );
        assert_eq!(
            immutable_uri(Path::new("/home/x/a.db")),
            "file:///home/x/a.db?mode=ro&immutable=1"
        );
        assert_eq!(
            immutable_uri(Path::new("\\\\server\\share\\a.db")),
            "file:////server/share/a.db?mode=ro&immutable=1"
        );
    }

    #[test]
    fn an_untrusted_store_is_hardened_and_an_immutable_one_takes_no_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("attached.db");
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER,
                                    content TEXT);
                 INSERT INTO line VALUES (1, 1, 0, 'x');",
            )
            .unwrap();
        let trusted_schema = |store: &LineStore| -> i64 {
            store
                .conn
                .query_row("PRAGMA trusted_schema", [], |r| r.get(0))
                .unwrap()
        };
        let plain = LineStore::open(&path).unwrap();
        assert_eq!(trusted_schema(&plain), 1);
        plain.close();
        let mut store = LineStore::open_with(
            &path,
            OpenMode {
                untrusted: true,
                immutable: true,
            },
        )
        .unwrap();
        assert_eq!(trusted_schema(&store), 0);
        store
            .in_read_txn(|store| {
                let rows = store.book_row_count(1)?;
                // An immutable reader holds no shared lock: a writer is never kept waiting.
                writer.busy_timeout(Duration::ZERO).unwrap();
                writer.execute_batch("BEGIN EXCLUSIVE; COMMIT").unwrap();
                Ok(rows)
            })
            .unwrap();
    }

    #[test]
    fn a_missing_file_is_reported_once_and_rechecked_after_a_backoff() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_books.db");
        configure_user_books(Some(path.to_str().unwrap())).unwrap();
        let read = || with_store(Source::UserBooks, |store| store.book_row_count(1));
        assert!(matches!(read(), Err(SourceUnavailable::Missing)));
        assert!(global(Source::UserBooks).missing_logged);
        // Inside the backoff the file is not even looked for.
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER,
                                    content TEXT);
                 INSERT INTO line VALUES (1, 1, 0, 'x');",
            )
            .unwrap();
        assert!(matches!(read(), Err(SourceUnavailable::Missing)));
        // Indexing does not wait for the backoff, nor does a configure of the same path.
        assert_eq!(
            with_store_for_indexing(Source::UserBooks, |store| store.book_row_count(1)).ok(),
            Some(1)
        );
        assert!(!global(Source::UserBooks).missing_logged);
        configure_user_books(None).unwrap();
        std::fs::remove_file(&path).unwrap();
        configure_user_books(Some(path.to_str().unwrap())).unwrap();
        assert!(matches!(read(), Err(SourceUnavailable::Missing)));
        reset_for_tests();
    }

    #[test]
    fn a_commit_to_another_table_keeps_the_zstd_decoder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_books.db");
        let dict = crate::external_text_tests::train_dictionary();
        let frame = zstd::bulk::Compressor::with_dictionary(3, &dict)
            .unwrap()
            .compress("בראשית ברא".as_bytes())
            .unwrap();
        let writer = Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER,
                                    content TEXT);
                 CREATE TABLE zstd_dict (id INTEGER PRIMARY KEY, dict BLOB NOT NULL);
                 CREATE TABLE other (x);",
            )
            .unwrap();
        writer
            .execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [&dict])
            .unwrap();
        writer
            .execute("INSERT INTO line VALUES (1, 1, 0, ?1)", [&frame])
            .unwrap();
        let mut store = LineStore::open(&path).unwrap();
        let key = LineKey {
            book_id: 1,
            ordinal: 0,
        };
        let read = |store: &mut LineStore| match &store.fetch_window(&[key], &[None]).unwrap()[0] {
            RowText::Found(text) => text.clone(),
            other => panic!("{other:?}"),
        };
        let loads = || CODEC_LOADS.with(|n| n.get());
        let before = loads();
        assert_eq!(read(&mut store), "בראשית ברא");
        writer.execute("INSERT INTO other VALUES (1)", []).unwrap();
        assert_eq!(read(&mut store), "בראשית ברא");
        assert_eq!(
            loads(),
            before,
            "an unrelated commit rebuilt the dictionaries"
        );
        writer
            .execute("INSERT INTO zstd_dict (dict) VALUES (?1)", [&dict])
            .unwrap();
        assert_eq!(read(&mut store), "בראשית ברא");
        assert_eq!(loads(), before + 1);
    }

    #[cfg(unix)]
    #[test]
    fn configuring_the_same_path_reopens_a_replaced_file() {
        let _guard = TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("user_books.db");
        let write = |at: &Path, text: &str| {
            Connection::open(at)
                .unwrap()
                .execute_batch(&format!(
                    "CREATE TABLE line (id INTEGER PRIMARY KEY, bookId INTEGER,
                                        lineIndex INTEGER, content TEXT);
                     INSERT INTO line VALUES (1, 1, 0, '{text}');"
                ))
                .unwrap();
        };
        write(&path, "old");
        let read = || {
            with_store(Source::UserBooks, |store| {
                store.fetch_window(
                    &[LineKey {
                        book_id: 1,
                        ordinal: 0,
                    }],
                    &[None],
                )
            })
            .unwrap()
        };
        configure_user_books(Some(path.to_str().unwrap())).unwrap();
        assert!(matches!(&read()[0], RowText::Found(t) if t == "old"));
        let replacement = dir.path().join("new.db");
        write(&replacement, "new");
        std::fs::rename(&replacement, &path).unwrap();
        configure_user_books(Some(path.to_str().unwrap())).unwrap();
        assert!(matches!(&read()[0], RowText::Found(t) if t == "new"));
        reset_for_tests();
    }

    #[test]
    fn dead_owner_cleanup_preserves_live_and_anonymous_holds() {
        let mut g = Global {
            suspend_depth: 1,
            owner_ports: vec![11, 22],
            ..Global::new()
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
            with_store(Source::Library, |store| {
                store.in_read_txn(|_| -> Result<()> { panic!("inside a window") })
            })
        });
        assert!(panicked.is_err());
        // Its read transaction went with the connection: a writer gets the file at once.
        assert!(!status(Source::Library).open);
        let writer = Connection::open(&db).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE; COMMIT").unwrap();
        assert_eq!(
            with_store(Source::Library, |store| store.book_row_count(1)).ok(),
            Some(1)
        );
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

    #[test]
    fn indexing_connections_do_not_hold_the_result_source_mutex() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        Connection::open(&path).unwrap().execute_batch(
            "CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
             INSERT INTO line VALUES(1, 1, 0, 'בראשית');"
        ).unwrap();
        configure(path.to_str().unwrap()).unwrap();
        let (started, reader_started) = std::sync::mpsc::channel();
        let (release, finish_read) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let indexing = scope.spawn(move || {
                let session = begin_scan(Source::Library).unwrap();
                session
                    .read(|store| {
                        started.send(()).unwrap();
                        finish_read.recv().unwrap();
                        store.read_book_for_indexing(1, None, false, &|| !session.is_current())
                    })
                    .unwrap()
                    .unwrap()
            });
            reader_started.recv().unwrap();
            let rows = with_store(Source::Library, |store| {
                store.fetch_window(
                    &[LineKey {
                        book_id: 1,
                        ordinal: 0,
                    }],
                    &[None],
                )
            })
            .unwrap();
            assert!(matches!(&rows[0], RowText::Found(text) if text == "בראשית"));
            release.send(()).unwrap();
            assert_eq!(indexing.join().unwrap().text.len(), "בראשית".len() as u64);
        });
        reset_for_tests();
    }

    #[test]
    fn suspension_interrupts_indexing_and_closes_its_connection() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        Connection::open(&path).unwrap().execute_batch(
            "CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
             INSERT INTO line VALUES(1, 1, 0, 'בראשית');"
        ).unwrap();
        configure(path.to_str().unwrap()).unwrap();
        let (started, reader_started) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let indexing = scope.spawn(move || {
                let session = begin_scan(Source::Library).unwrap();
                session.read(|store| {
                    started.send(()).unwrap();
                    while session.is_current() {
                        std::thread::yield_now();
                    }
                    store.read_book_for_indexing(1, None, false, &|| !session.is_current())
                })
            });
            reader_started.recv().unwrap();
            suspend();
            assert!(matches!(
                indexing.join().unwrap(),
                Err(SourceUnavailable::Suspended)
            ));
            assert_eq!(global(Source::Library).scans, 0);
        });
        // An exclusive writer is proof that the interrupted transaction was released.
        Connection::open(&path)
            .unwrap()
            .execute_batch("BEGIN EXCLUSIVE; ROLLBACK")
            .unwrap();
        resume().unwrap();
        let mut text = crate::library_indexing::read_library_book(Source::Library, 1, None)
            .unwrap()
            .unwrap();
        let mut actual = String::new();
        std::io::Read::read_to_string(&mut text.text.reader().unwrap(), &mut actual).unwrap();
        assert_eq!(actual, "בראשית");
        reset_for_tests();
    }

    #[test]
    fn cancellation_in_the_row_reader_rolls_back_without_a_partial_book() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        Connection::open(&path).unwrap().execute_batch(
            "CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
             INSERT INTO line VALUES(1, 1, 0, 'ראשונה'),(2, 1, 1, 'שנייה'),(3, 1, 2, 'שלישית');"
        ).unwrap();
        let mut store = LineStore::open(&path).unwrap();
        let rows = AtomicUsize::new(0);
        let cancelled = store.read_book_for_indexing(1, None, false, &|| {
            rows.fetch_add(1, Ordering::Relaxed) >= 1
        });
        assert!(cancelled.err().unwrap().is::<IndexingReadStopped>());
        assert!(store.conn.is_autocommit());
        let book = store
            .read_book_for_indexing(1, None, false, &|| false)
            .unwrap()
            .unwrap();
        assert_eq!(book.rows, 3);
    }

    #[test]
    fn indexing_source_busy_on_open_is_retryable() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
            INSERT INTO line VALUES(1,1,0,'בראשית');").unwrap();
        configure(path.to_str().unwrap()).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let busy = crate::library_indexing::read_library_book(Source::Library, 1, None);
        assert!(matches!(busy, Err(SourceUnavailable::Busy)));
        writer.execute_batch("ROLLBACK").unwrap();
        assert!(
            crate::library_indexing::read_library_book(Source::Library, 1, None)
                .unwrap()
                .is_some()
        );
        reset_for_tests();
    }

    #[test]
    fn indexing_sessions_share_dictionaries_without_leaving_connections_open() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
            CREATE TABLE zstd_dict(id INTEGER PRIMARY KEY, dict BLOB NOT NULL);
            INSERT INTO line VALUES(1,1,0,'בראשית');").unwrap();
        let dictionary = crate::external_text_tests::train_dictionary();
        writer
            .execute("INSERT INTO zstd_dict(dict) VALUES(?1)", [&dictionary])
            .unwrap();
        configure(path.to_str().unwrap()).unwrap();
        let before = CODEC_LOADS.with(|count| count.get());
        for _ in 0..3 {
            crate::library_indexing::read_library_book(Source::Library, 1, None).unwrap();
        }
        assert_eq!(CODEC_LOADS.with(|count| count.get()), before + 1);
        assert_eq!(global(Source::Library).scans, 0);
        writer.execute_batch("BEGIN EXCLUSIVE; ROLLBACK").unwrap();
        suspend();
        resume().unwrap();
        crate::library_indexing::read_library_book(Source::Library, 1, None).unwrap();
        assert_eq!(CODEC_LOADS.with(|count| count.get()), before + 2);
        reset_for_tests();
    }

    #[test]
    fn wal_dictionary_update_of_the_same_size_reloads_before_decoding() {
        use std::io::Read;
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL;
            CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
            CREATE TABLE zstd_dict(id INTEGER PRIMARY KEY, dict BLOB NOT NULL);").unwrap();
        let dictionary = crate::external_text_tests::train_dictionary();
        let mut changed = dictionary.clone();
        let id = u32::from_le_bytes(changed[4..8].try_into().unwrap());
        changed[4..8].copy_from_slice(&id.wrapping_add(1).max(1).to_le_bytes());
        writer
            .execute("INSERT INTO zstd_dict(dict) VALUES(?1)", [&dictionary])
            .unwrap();
        let frame = zstd::bulk::Compressor::with_dictionary(3, &dictionary)
            .unwrap()
            .compress("בראשית".as_bytes())
            .unwrap();
        writer
            .execute("INSERT INTO line VALUES(1,1,0,?1)", [frame])
            .unwrap();
        configure(path.to_str().unwrap()).unwrap();
        crate::library_indexing::read_library_book(Source::Library, 1, None).unwrap();
        let before = scan_file_stamp(&path).unwrap();
        writer
            .execute("UPDATE zstd_dict SET dict=?1", [&changed])
            .unwrap();
        let frame = zstd::bulk::Compressor::with_dictionary(3, &changed)
            .unwrap()
            .compress("אור חדש".as_bytes())
            .unwrap();
        writer
            .execute("UPDATE line SET content=?1", [frame])
            .unwrap();
        let after = scan_file_stamp(&path).unwrap();
        assert_eq!(
            before.main, after.main,
            "the main database did not receive this WAL commit"
        );
        assert!(before.wal != after.wal);
        let mut book = crate::library_indexing::read_library_book(Source::Library, 1, None)
            .unwrap()
            .unwrap();
        let mut text = String::new();
        book.text
            .reader()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "אור חדש");
        reset_for_tests();
    }

    #[test]
    fn paging_snapshot_does_not_change_when_a_source_is_first_opened() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        reset_for_tests();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
            INSERT INTO line VALUES(1,1,0,'בראשית');").unwrap();
        configure(path.to_str().unwrap()).unwrap();
        let before = search_snapshot_generation();
        with_store(Source::Library, |_| Ok(())).unwrap();
        assert_eq!(before, search_snapshot_generation());
        writer.execute("UPDATE line SET content='חדש'", []).unwrap();
        assert_ne!(before, search_snapshot_generation());
        reset_for_tests();
    }
    #[test]
    fn raw_fetches_stop_between_rows_and_leave_the_connection_usable() {
        use std::cell::Cell;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.db");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE line(id INTEGER PRIMARY KEY, bookId INTEGER, lineIndex INTEGER, content TEXT);
            WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<100)
            INSERT INTO line SELECT i,1,i-1,'בראשית' FROM n;").unwrap();
        let mut store = LineStore::open(&path).unwrap();
        let checks = Cell::new(0);
        let stop = || {
            checks.set(checks.get() + 1);
            checks.get() >= 4
        };
        assert!(store
            .fetch_book_raw_checked(1, &stop)
            .err()
            .unwrap()
            .is::<IndexingReadStopped>());
        assert_eq!(checks.get(), 4);
        assert_eq!(store.fetch_book_raw(1).unwrap().unwrap().0.len(), 100);
        checks.set(0);
        assert!(store
            .fetch_rows_raw_checked(1, &[0, 1, 2, 3], &stop)
            .err()
            .unwrap()
            .is::<IndexingReadStopped>());
        assert_eq!(checks.get(), 4);
        let (rows, _) = store.fetch_rows_raw(1, &[0, 1, 2, 3]).unwrap();
        assert_eq!(rows.len(), 4);
        assert!(rows.iter().all(Option::is_some));
    }
}
