//! [`SearchEngine::add_library_books`]: books read from their database and prepared on
//! several threads at once, enqueued in input order on the calling thread.
//!
//! Source text and prepared documents spill to temporary files before growing beyond
//! their per-book buffers. A bounded reservation is taken before any book is read.
//!
//! [`SearchEngine::add_library_books`]: crate::api::search_engine::SearchEngine::add_library_books

use crate::api::library_indexing::{
    IndexingCancellationToken, LibraryBookInput, LibraryBookResult, LibraryBookSource,
    LibraryBookStatus,
};
use crate::api::search_engine::{
    line_source_book, PreparedTextBook, SearchEngine, TextBookMeta, TextStorage,
};
#[cfg(test)]
use crate::line_source::IndexingText;
use crate::line_source::{self, Source, SourceUnavailable};
use anyhow::Result;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Condvar, Mutex};
use std::time::{Duration, Instant};
use tantivy::IndexWriter;

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
#[cfg(test)]
pub(crate) fn read_library_book(
    source: Source,
    book_id: i64,
    db_title: Option<&str>,
) -> std::result::Result<Option<IndexingText>, SourceUnavailable> {
    // The app puts `line_note` bodies back only in books of the library (`_dbBookNotes`).
    let with_notes = source == Source::Library;
    let session = line_source::begin_scan(source)?;
    let started = Instant::now();
    let result = session.read(|store| {
        store.read_book_for_indexing_buffered(
            book_id,
            db_title,
            with_notes,
            &|| !session.is_current(),
            None,
            1 << 20,
            false,
        )
    });
    crate::indexing_diagnostics::source_read(started.elapsed());
    result
}

pub(crate) fn library_book_fingerprint(
    source: Source,
    book_id: i64,
    db_title: Option<&str>,
) -> Result<Option<u64>> {
    let session = line_source::begin_scan(source).map_err(|unavailable| {
        anyhow::anyhow!("book {book_id} of {source:?} cannot be read: {unavailable:?}")
    })?;
    let text = session
        .read(|store| {
            store.read_book_for_indexing_buffered(
                book_id,
                db_title,
                source == Source::Library,
                &|| !session.is_current(),
                None,
                0,
                true,
            )
        })
        .map_err(|unavailable| {
            anyhow::anyhow!("book {book_id} of {source:?} cannot be read: {unavailable:?}")
        })?;
    text.filter(|text| !text.text.is_empty())
        .map(|text| text.text.fingerprint())
        .transpose()
}

pub(crate) enum Outcome {
    Prepared(Box<PreparedTextBook>),
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
#[cfg(test)]
pub(crate) fn prepare(engine: &SearchEngine, book: &LibraryBookInput) -> Outcome {
    prepare_checked(engine, book, engine.library_preparation_policy(), &|| false)
}

fn prepare_checked(
    engine: &SearchEngine,
    book: &LibraryBookInput,
    policy: crate::indexing_buffers::PreparationPolicy,
    stop: &dyn Fn() -> bool,
) -> Outcome {
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
    let session = match line_source::begin_scan(source) {
        Ok(session) => session,
        Err(unavailable) => {
            return Outcome::Done(LibraryBookResult {
                error: Some(format!("{unavailable:?}")),
                ..result(book, LibraryBookStatus::Unavailable)
            })
        }
    };
    let generation = session.generation();
    let read_started = Instant::now();
    let read = session.read(|store| {
        store.read_book_for_indexing_buffered(
            book.book_id,
            book.db_title.as_deref(),
            source == Source::Library,
            &|| stop() || !session.is_current(),
            Some(engine.preparation_directory()),
            policy.text_bytes,
            false,
        )
    });
    crate::indexing_diagnostics::source_read(read_started.elapsed());
    drop(session);
    if stop() {
        return Outcome::Done(result(book, LibraryBookStatus::Cancelled));
    }
    let mut text = match read {
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
    let document_budget = policy.document_bytes;
    match engine.prepare_text_book_buffer(
        &meta,
        &mut text.text,
        text_storage,
        &mut text.data_uri_rows,
        document_budget,
        stop,
    ) {
        Ok(mut prepared) => {
            prepared.source_generation = Some((source, generation));
            Outcome::Prepared(Box::new(prepared))
        }
        Err(_) if stop() => Outcome::Done(result(book, LibraryBookStatus::Cancelled)),
        Err(error) => Outcome::Done(failed(book, format!("{error:#}"))),
    }
}

/// Reservations for source/document buffers, and the next input allowed to start.
struct Budget {
    state: Mutex<(usize, usize)>,
    changed: Condvar,
    limit: usize,
}

impl Budget {
    /// Reserves in input order, before reading; later books cannot starve the next one.
    fn admit(&self, index: usize, bytes: usize, stop: &dyn Fn() -> bool) -> bool {
        let mut state = self.state.lock().unwrap();
        loop {
            if stop() {
                return false;
            }
            let (in_flight, next) = *state;
            if index == next && in_flight.saturating_add(bytes) <= self.limit {
                state.0 += bytes;
                state.1 += 1;
                self.changed.notify_all();
                return true;
            }
            state = self
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    fn release(&self, bytes: usize) {
        let mut state = self.state.lock().unwrap();
        state.0 -= bytes;
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
    let policy = engine.library_preparation_policy();
    let reservation = policy.reservation();
    let budget = Budget {
        state: Mutex::new((0, 0)),
        changed: Condvar::new(),
        limit: reservation * policy.book_workers,
    };
    let mut results: Vec<Option<LibraryBookResult>> = vec![None; books.len()];
    let (tx, rx) = mpsc::channel::<(usize, Outcome, usize)>();
    let mut docs_total = 0u64;
    let mut enqueue_time = Duration::ZERO;

    let enqueued = std::thread::scope(|scope| -> Result<()> {
        for _ in 0..policy.book_workers.min(books.len()) {
            let tx = tx.clone();
            let (books, next_book, budget, stop) = (&books, &next_book, &budget, &stop);
            scope.spawn(move || loop {
                let index = next_book.fetch_add(1, Ordering::Relaxed);
                let Some(book) = books.get(index) else {
                    break;
                };
                let admitted = budget.admit(index, reservation, stop);
                let outcome = if !admitted {
                    Outcome::Done(result(book, LibraryBookStatus::Cancelled))
                } else {
                    // A panic would leave its book unsent and every later one waiting
                    // in the budget for it: it fails that book instead.
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        prepare_checked(engine, book, policy, stop)
                    }))
                    .unwrap_or_else(|_| Outcome::Done(failed(book, "preparing the book panicked")))
                };
                if tx
                    .send((index, outcome, if admitted { reservation } else { 0 }))
                    .is_err()
                {
                    break;
                }
            });
        }
        drop(tx);

        let mut waiting: BTreeMap<usize, (Outcome, usize)> = BTreeMap::new();
        let mut next = 0usize;
        let enqueue_in_order = |waiting: &mut BTreeMap<usize, (Outcome, usize)>,
                                next: &mut usize,
                                results: &mut Vec<Option<LibraryBookResult>>,
                                docs_total: &mut u64,
                                enqueue_time: &mut Duration|
         -> Result<()> {
            while let Some((outcome, reservation)) = waiting.remove(next) {
                let book = &books[*next];
                let done = match outcome {
                    Outcome::Done(done) => done,
                    Outcome::Prepared(_) if cancelled() => {
                        result(book, LibraryBookStatus::Cancelled)
                    }
                    Outcome::Prepared(prepared)
                        if prepared
                            .source_generation
                            .is_some_and(|(source, generation)| {
                                let status = line_source::status(source);
                                status.generation != generation || status.suspend_depth != 0
                            }) =>
                    {
                        result(book, LibraryBookStatus::Unavailable)
                    }
                    Outcome::Prepared(mut prepared) => {
                        let enqueue_started = Instant::now();
                        let docs = prepared.enqueue(writer)?;
                        *enqueue_time += enqueue_started.elapsed();
                        *docs_total += u64::from(docs);
                        LibraryBookResult {
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
                        }
                    }
                };
                results[*next] = Some(done);
                *next += 1;
                budget.release(reservation);
            }
            Ok(())
        };
        for (index, outcome, reservation) in rx.iter() {
            waiting.insert(index, (outcome, reservation));
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
