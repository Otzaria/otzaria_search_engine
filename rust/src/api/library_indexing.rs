//! Types of [`SearchEngine::add_library_books`]: indexing books whose text the engine reads
//! itself from the library, user-books or an attached database.
//!
//! [`SearchEngine::add_library_books`]: crate::api::search_engine::SearchEngine::add_library_books

use crate::api::search_engine::TextStorage;
use flutter_rust_bridge::frb;
use std::sync::atomic::{AtomicBool, Ordering};

/// The database a book's rows are read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryBookSource {
    /// The library database configured with `configure_line_source` (`seforim.db`).
    Library,
    /// The user-books database configured with `configure_user_books_line_source`.
    UserBooks,
    /// The attached database named by `attached_slug`, configured with
    /// `configure_attached_line_source`.
    Attached,
}

/// One book of [`SearchEngine::add_library_books`]: where its rows are, and everything the
/// index records about it, exactly as `add_text_book_bytes` takes it.
///
/// [`SearchEngine::add_library_books`]: crate::api::search_engine::SearchEngine::add_library_books
#[derive(Clone, Debug)]
pub struct LibraryBookInput {
    pub source: LibraryBookSource,
    /// The attached database's slug: given exactly when `source` is `Attached`.
    pub attached_slug: Option<String>,
    /// The book's `book.id` in that database.
    pub book_id: i64,
    /// When given, the rows are read only if `book.title` is still this (the app's guard
    /// against a book id reused by a library update); otherwise the book is `Missing`.
    pub db_title: Option<String>,
    pub title: String,
    pub topics: String,
    pub file_path: String,
    pub extra_facets: Option<Vec<String>>,
    /// The book's `sourceHash` (the database's `book.contentHash`), stored as given;
    /// `None`/0 for none.
    pub source_hash: Option<u64>,
    /// As for `add_text_book_bytes`. `LibraryDb` is kept only when every row is one line
    /// (no row holds `\n`) and, in the library, the book has no `line_note` rows (their
    /// bodies are indexed back in their lines, as the app does); `file_path` must then be `id:<bookId>` (library),
    /// `uid:<bookId>` (user books) or `db:<slug>:<bookId>` (attached) of this very book.
    pub text_storage: TextStorage,
}

/// What became of one book.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LibraryBookStatus {
    /// Its documents were added (`docs` of them).
    Indexed,
    /// The book has no rows, its title did not match `db_title`, or its text is empty: the
    /// app's read finds nothing either, and falls back to the book's file.
    Missing,
    /// The database could not be read now (unconfigured, suspended, missing, busy).
    Unavailable,
    /// Reading or preparing the book failed; `error` says why. Nothing was added.
    Failed,
    /// The token was cancelled before the book was added. Nothing was added.
    Cancelled,
}

/// The outcome for one input book, in input order.
#[derive(Clone, Debug)]
pub struct LibraryBookResult {
    pub book_id: i64,
    pub status: LibraryBookStatus,
    /// Documents added (0 unless `Indexed`).
    pub docs: u32,
    /// The storage the book was indexed with (`InIndex` when `LibraryDb` was refused).
    pub text_storage: TextStorage,
    /// The number the engine gave the book (0 unless `Indexed`).
    pub book_number: u32,
    /// The book's `contentHash`, as `compute_book_fingerprint` gives it (0 unless `Indexed`).
    pub content_hash: u64,
    /// The book's `textHash`, as `compute_content_fingerprint_bytes` gives it (0 unless
    /// `Indexed`).
    pub text_hash: u64,
    /// The book's `metadataHash`, as `compute_book_metadata_fingerprint` gives it (0 unless
    /// `Indexed`).
    pub metadata_hash: u64,
    /// The `source_hash` it was given (0 unless `Indexed`).
    pub source_hash: u64,
    pub error: Option<String>,
}

/// Stops an [`SearchEngine::add_library_books`] batch between books: the books already
/// added stay in the writer (uncommitted), the rest come back `Cancelled`.
///
/// [`SearchEngine::add_library_books`]: crate::api::search_engine::SearchEngine::add_library_books
#[frb(opaque)]
#[derive(Debug, Default)]
pub struct IndexingCancellationToken {
    cancelled: AtomicBool,
}

impl IndexingCancellationToken {
    #[frb(sync)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns at once; the batch stops before its next book.
    #[frb(sync)]
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    #[frb(sync, getter)]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// The `textHash` a book of the library, user-books or an attached database (`source`
/// `Attached` with its `attached_slug`) would be indexed with by
/// [`SearchEngine::add_library_books`] — what `compute_content_fingerprint_bytes` gives for
/// the text the app reads for it — read and hashed off the Dart heap. `None` when the book
/// has no rows, its title is not `db_title`, or its text is empty (the cases
/// `add_library_books` reports `Missing`). An error when the database cannot be read now.
///
/// [`SearchEngine::add_library_books`]: crate::api::search_engine::SearchEngine::add_library_books
pub fn library_book_content_fingerprint(
    source: LibraryBookSource,
    attached_slug: Option<String>,
    book_id: i64,
    db_title: Option<String>,
) -> anyhow::Result<Option<u64>> {
    let source = crate::library_indexing::line_source_of(source, attached_slug.as_deref())
        .map_err(anyhow::Error::msg)?;
    let text = crate::library_indexing::read_library_book(source, book_id, db_title.as_deref())
        .map_err(|unavailable| {
            anyhow::anyhow!("book {book_id} of {source:?} cannot be read: {unavailable:?}")
        })?;
    Ok(text.filter(|text| !text.text.is_empty()).map(|text| {
        crate::api::search_engine::compute_content_fingerprint_bytes(text.text.into_bytes())
    }))
}
