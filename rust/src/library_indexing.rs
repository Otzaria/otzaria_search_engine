//! [`SearchEngine::add_library_books`]: books read from their database and prepared on
//! several threads at once, enqueued in input order on the calling thread.
//!
//! The batch holds the engine by shared reference, so searches keep running while it
//! prepares; only the writer's queue is touched, and tantivy's `add_document` takes the
//! writer by shared reference too. Prepared books wait for their turn under a byte budget;
//! the book next in line is always admitted, so the budget never stalls the batch.
//!
//! [`SearchEngine::add_library_books`]: crate::api::search_engine::SearchEngine::add_library_books

use crate::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookResult, LibraryBookSource,
    LibraryBookStatus,
};
use crate::api::search_engine::{
    line_source_book, PreparedTextBook, SearchEngine, TextBookMeta, TextStorage,
};
use crate::line_source::{self, IndexingText, Source, SourceUnavailable};
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tantivy::IndexWriter;

/// Books read and prepared at once. Each also fans its lines out over rayon's pool.
const PREPARE_WORKERS: usize = 4;
/// Text bytes of prepared books waiting to be enqueued; their documents take a few times as
/// much.
/// Tests use a small budget so that a few small books fill it.
const IN_FLIGHT_BYTES: usize = if cfg!(test) { 4 << 10 } else { 64 << 20 };

/// The book whose preparation panics, for tests.
#[cfg(test)]
pub(crate) static PANIC_ON_BOOK: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(i64::MIN);

/// The line source of a book's `source` and `attached_slug`, or why they do not name one.
pub(crate) fn line_source_of(
    source: LibraryBookSource,
    attached_slug: Option<&str>,
) -> std::result::Result<Source, String> {
    match (source, attached_slug) {
        (LibraryBookSource::Library, None) => Ok(Source::Library),
        (LibraryBookSource::UserBooks, None) => Ok(Source::UserBooks),
        (LibraryBookSource::Attached, Some(slug)) => {
            Source::attached(slug).ok_or_else(|| format!("invalid attached database slug {slug:?}"))
        }
        (source, slug) => Err(format!(
            "attached_slug must be given exactly for LibraryBookSource::Attached, got \
             {source:?} with {slug:?}"
        )),
    }
}

/// A book's text as the app would hand it to `add_text_book_bytes`. A read that fails is
/// logged by the line source and comes back `SourceUnavailable::Failed`.
pub(crate) fn read_library_book(
    source: Source,
    book_id: i64,
    db_title: Option<&str>,
) -> std::result::Result<Option<IndexingText>, SourceUnavailable> {
    // The app puts `line_note` bodies back only in books of the library (`_dbBookNotes`).
    let with_notes = source == Source::Library;
    line_source::with_store_for_indexing(source, |store| {
        store.read_book_for_indexing(book_id, db_title, with_notes)
    })
}

pub(crate) enum Outcome {
    Prepared(Box<PreparedTextBook>, usize),
    Done(LibraryBookResult),
}

fn result(book: &LibraryBookInput, status: LibraryBookStatus) -> LibraryBookResult {
    LibraryBookResult {
        book_id: book.book_id,
        status,
        docs: 0,
        text_storage: book.text_storage,
        book_number: 0,
        content_hash: 0,
        text_hash: 0,
        metadata_hash: 0,
        source_hash: 0,
        error: None,
    }
}

fn failed(book: &LibraryBookInput, error: impl std::fmt::Display) -> LibraryBookResult {
    LibraryBookResult {
        error: Some(error.to_string()),
        ..result(book, LibraryBookStatus::Failed)
    }
}

/// Reads and prepares one book; never touches the writer.
pub(crate) fn prepare(engine: &SearchEngine, book: &LibraryBookInput) -> Outcome {
    #[cfg(test)]
    if book.book_id == PANIC_ON_BOOK.load(Ordering::Relaxed) {
        panic!("preparing book {} panics", book.book_id);
    }
    let source = match line_source_of(book.source, book.attached_slug.as_deref()) {
        Ok(source) => source,
        Err(error) => return Outcome::Done(failed(book, error)),
    };
    if book.text_storage == TextStorage::LibraryDb
        && line_source_book(&book.file_path) != Some((source, book.book_id))
    {
        return Outcome::Done(failed(
            book,
            format!(
                "TextStorage::LibraryDb needs the file_path of book {} in {source:?}, got {:?}",
                book.book_id, book.file_path
            ),
        ));
    }
    let text = match read_library_book(source, book.book_id, book.db_title.as_deref()) {
        Err(SourceUnavailable::Failed) => {
            return Outcome::Done(failed(
                book,
                "the book's rows could not be read (see the log)",
            ))
        }
        Err(unavailable) => {
            return Outcome::Done(LibraryBookResult {
                error: Some(format!("{unavailable:?}")),
                ..result(book, LibraryBookStatus::Unavailable)
            })
        }
        Ok(Some(text)) if !text.text.is_empty() => text,
        Ok(_) => return Outcome::Done(result(book, LibraryBookStatus::Missing)),
    };
    // As the app: a row holding `\n` would shift every later line off its row, and a book
    // whose `line_note` bodies were put back is no longer its rows.
    let text_storage = if text.row_has_newline || text.has_notes {
        TextStorage::InIndex
    } else {
        book.text_storage
    };
    let meta = TextBookMeta {
        title: book.title.clone(),
        topics: book.topics.clone(),
        file_path: book.file_path.clone(),
        extra_facets: book.extra_facets.clone().unwrap_or_default(),
        source_hash: book.source_hash.unwrap_or(0),
    };
    match engine.prepare_text_book(&meta, &text.text, text_storage, &text.data_uri_rows) {
        Ok(prepared) => Outcome::Prepared(Box::new(prepared), text.text.len()),
        Err(error) => Outcome::Done(failed(book, format!("{error:#}"))),
    }
}

/// Bytes of prepared books not yet enqueued, and the input position enqueued next.
struct Budget {
    state: Mutex<(usize, usize)>,
    changed: Condvar,
}

impl Budget {
    /// Waits until `bytes` fit, or `index` is the book enqueued next; `false` once `stop`.
    fn admit(&self, index: usize, bytes: usize, stop: &dyn Fn() -> bool) -> bool {
        let mut state = self.state.lock().unwrap();
        loop {
            if stop() {
                return false;
            }
            let (in_flight, next) = *state;
            if index == next || in_flight + bytes <= IN_FLIGHT_BYTES {
                state.0 += bytes;
                return true;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    fn release(&self, bytes: usize, next: usize) {
        let mut state = self.state.lock().unwrap();
        state.0 -= bytes;
        state.1 = next;
        self.changed.notify_all();
    }
}

/// See [`SearchEngine::add_library_books`].
pub(crate) fn add_library_books(
    engine: &SearchEngine,
    writer: &IndexWriter,
    books: Vec<LibraryBookInput>,
    cancel: Option<&IndexingCancellationToken>,
) -> Result<Vec<LibraryBookResult>> {
    let started = Instant::now();
    // In input order, before the workers race for them: the same batch numbers its books
    // the same way every time.
    for book in &books {
        engine.book_number(&book.file_path)?;
    }
    let cancelled = || cancel.is_some_and(|token| token.is_cancelled());
    let abort = AtomicBool::new(false);
    let stop = || cancelled() || abort.load(Ordering::Relaxed);
    let next_book = AtomicUsize::new(0);
    let budget = Budget {
        state: Mutex::new((0, 0)),
        changed: Condvar::new(),
    };
    let mut results: Vec<Option<LibraryBookResult>> = vec![None; books.len()];
    let (tx, rx) = mpsc::channel::<(usize, Outcome)>();
    let mut docs_total = 0u64;
    let mut enqueue_time = Duration::ZERO;

    let enqueued = std::thread::scope(|scope| -> Result<()> {
        for _ in 0..PREPARE_WORKERS.min(books.len()) {
            let tx = tx.clone();
            let (books, next_book, budget, stop) = (&books, &next_book, &budget, &stop);
            scope.spawn(move || loop {
                let index = next_book.fetch_add(1, Ordering::Relaxed);
                let Some(book) = books.get(index) else {
                    break;
                };
                let outcome = if stop() {
                    Outcome::Done(result(book, LibraryBookStatus::Cancelled))
                } else {
                    // A panic would leave its book unsent and every later one waiting
                    // in the budget for it: it fails that book instead.
                    let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        prepare(engine, book)
                    }))
                    .unwrap_or_else(|_| Outcome::Done(failed(book, "preparing the book panicked")));
                    match prepared {
                        Outcome::Prepared(prepared, bytes) => {
                            if budget.admit(index, bytes, stop) {
                                Outcome::Prepared(prepared, bytes)
                            } else {
                                Outcome::Done(result(book, LibraryBookStatus::Cancelled))
                            }
                        }
                        done => done,
                    }
                };
                if tx.send((index, outcome)).is_err() {
                    break;
                }
            });
        }
        drop(tx);

        let mut waiting: BTreeMap<usize, Outcome> = BTreeMap::new();
        let mut next = 0usize;
        let enqueue_in_order = |waiting: &mut BTreeMap<usize, Outcome>,
                                next: &mut usize,
                                results: &mut Vec<Option<LibraryBookResult>>,
                                docs_total: &mut u64,
                                enqueue_time: &mut Duration|
         -> Result<()> {
            while let Some(outcome) = waiting.remove(next) {
                let book = &books[*next];
                let (done, bytes) = match outcome {
                    Outcome::Done(done) => (done, 0),
                    Outcome::Prepared(_, bytes) if cancelled() => {
                        (result(book, LibraryBookStatus::Cancelled), bytes)
                    }
                    Outcome::Prepared(mut prepared, bytes) => {
                        let enqueue_started = Instant::now();
                        let docs = prepared.enqueue(writer)?;
                        *enqueue_time += enqueue_started.elapsed();
                        *docs_total += u64::from(docs);
                        let done = LibraryBookResult {
                            book_id: book.book_id,
                            status: LibraryBookStatus::Indexed,
                            docs,
                            text_storage: prepared.text_storage,
                            book_number: prepared.book_number,
                            content_hash: prepared.content_hash,
                            text_hash: prepared.text_hash,
                            metadata_hash: prepared.metadata_hash,
                            source_hash: prepared.source_hash,
                            error: None,
                        };
                        (done, bytes)
                    }
                };
                results[*next] = Some(done);
                *next += 1;
                budget.release(bytes, *next);
            }
            Ok(())
        };
        for (index, outcome) in rx.iter() {
            waiting.insert(index, outcome);
            if let Err(error) = enqueue_in_order(
                &mut waiting,
                &mut next,
                &mut results,
                &mut docs_total,
                &mut enqueue_time,
            ) {
                abort.store(true, Ordering::Relaxed);
                budget.changed.notify_all();
                return Err(error);
            }
        }
        Ok(())
    });
    enqueued?;

    let results: Vec<LibraryBookResult> = results
        .into_iter()
        .zip(&books)
        .map(|(done, book)| done.unwrap_or_else(|| result(book, LibraryBookStatus::Cancelled)))
        .collect();
    log::info!(
        "add_library_books: {} books, {} indexed, {docs_total} docs in {:?} (enqueue {:?})",
        books.len(),
        results
            .iter()
            .filter(|r| r.status == LibraryBookStatus::Indexed)
            .count(),
        started.elapsed(),
        enqueue_time
    );
    Ok(results)
}
