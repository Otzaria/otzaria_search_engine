//! Writer teardown must finish background I/O before releasing its directory lock.

use std::io;
use std::path::Path;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use tantivy::collector::Count;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};
use tantivy::merge_policy::NoMergePolicy;
use tantivy::query::TermQuery;
use tantivy::schema::{IndexRecordOption, Schema, STORED, TEXT};
use tantivy::{doc, Directory, Index, IndexWriter, ReloadPolicy, TantivyDocument, Term};

#[derive(Debug, Default)]
struct GateState {
    armed: bool,
    entered: bool,
    released: bool,
}

#[derive(Debug, Default)]
struct MergeGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl MergeGate {
    fn arm(&self) {
        self.state.lock().unwrap().armed = true;
    }

    fn pause_if_armed(&self) {
        let mut state = self.state.lock().unwrap();
        if !state.armed || state.entered {
            return;
        }
        state.entered = true;
        self.changed.notify_all();
        while !state.released {
            state = self.changed.wait(state).unwrap();
        }
    }

    fn wait_until_entered(&self) -> bool {
        let state = self.state.lock().unwrap();
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(10), |state| !state.entered)
            .unwrap();
        state.entered
    }

    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.changed.notify_all();
    }
}

struct ReleaseMergeOnDrop(Arc<MergeGate>);

impl Drop for ReleaseMergeOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Clone, Copy, Debug)]
enum MergeOutcome {
    Finish,
    Error,
    Panic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PauseAt {
    MergeOutput,
    CommittedMetadata,
}

#[derive(Clone, Copy, Debug)]
enum ReplacementMethod {
    Remove,
    Rename,
}

#[derive(Clone, Debug)]
struct PausedMergeDirectory {
    inner: MmapDirectory,
    gate: Arc<MergeGate>,
    outcome: MergeOutcome,
    pause_at: PauseAt,
}

impl Directory for PausedMergeDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.inner.delete(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        let output = self.inner.open_write(path)?;
        if self.pause_at == PauseAt::MergeOutput
            && std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with("merge_thread_"))
        {
            // IndexMerger already owns the input segment mmaps, and `output`
            // is an open file. Hold both until the test releases the gate.
            self.gate.pause_if_armed();
            match self.outcome {
                MergeOutcome::Finish => {}
                MergeOutcome::Error => {
                    return Err(OpenWriteError::wrap_io_error(
                        io::Error::other("injected merge write failure"),
                        path.to_owned(),
                    ));
                }
                MergeOutcome::Panic => panic!("injected merge write panic"),
            }
        }
        Ok(output)
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if self.pause_at == PauseAt::CommittedMetadata
            && path == Path::new("meta.json")
            && std::thread::current().name() == Some("segment_updater")
        {
            // An accepted merge has already passed save_metas' is_alive check.
            // Killing the updater cannot retract this in-progress write.
            self.gate.pause_if_armed();
        }
        self.inner.atomic_write(path, data)
    }

    fn sync_directory(&self) -> io::Result<()> {
        self.inner.sync_directory()
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        self.inner.acquire_lock(lock)
    }

    fn watch(&self, callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(callback)
    }
}

#[test]
fn dropping_writer_waits_for_merge_and_discards_pending_changes() {
    assert_shutdown_drains_merge(PauseAt::MergeOutput, MergeOutcome::Finish, None);
}

#[test]
fn dropping_writer_waits_for_failed_merge_and_discards_pending_changes() {
    assert_shutdown_drains_merge(PauseAt::MergeOutput, MergeOutcome::Error, None);
}

#[test]
fn dropping_writer_waits_for_panicking_merge_and_discards_pending_changes() {
    assert_shutdown_drains_merge(PauseAt::MergeOutput, MergeOutcome::Panic, None);
}

#[test]
fn dropping_writer_finishes_metadata_before_removing_and_replacing_index() {
    assert_shutdown_drains_merge(
        PauseAt::CommittedMetadata,
        MergeOutcome::Finish,
        Some(ReplacementMethod::Remove),
    );
}

#[test]
fn dropping_writer_finishes_metadata_before_renaming_and_replacing_index() {
    assert_shutdown_drains_merge(
        PauseAt::CommittedMetadata,
        MergeOutcome::Finish,
        Some(ReplacementMethod::Rename),
    );
}

fn assert_shutdown_drains_merge(
    pause_at: PauseAt,
    outcome: MergeOutcome,
    replacement_method: Option<ReplacementMethod>,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("index");
    std::fs::create_dir(&root).unwrap();
    let gate = Arc::new(MergeGate::default());
    let directory = PausedMergeDirectory {
        inner: MmapDirectory::open(&root).unwrap(),
        gate: gate.clone(),
        outcome,
        pause_at,
    };
    let mut schema = Schema::builder();
    let text = schema.add_text_field("text", TEXT | STORED);
    let schema = schema.build();
    let index = Index::create(directory, schema.clone(), Default::default()).unwrap();
    let mut writer: IndexWriter<TantivyDocument> =
        index.writer_with_num_threads(1, 15_000_000).unwrap();
    writer.set_merge_policy(Box::new(NoMergePolicy));
    for value in ["committed first", "committed second"] {
        writer.add_document(doc!(text => value)).unwrap();
        writer.commit().unwrap();
    }

    let segments = index.searchable_segment_ids().unwrap();
    assert_eq!(segments.len(), 2);
    gate.arm();
    let merge = writer.merge(&segments);
    let _release_on_unwind = ReleaseMergeOnDrop(gate.clone());
    assert!(
        gate.wait_until_entered(),
        "merge did not reach the requested I/O barrier: {pause_at:?}"
    );

    if pause_at == PauseAt::MergeOutput {
        // Waiting for a merge must not commit either pending change. Keep the
        // late metadata cases idle so worker joins cannot mask the merge race.
        writer.add_document(doc!(text => "pending third")).unwrap();
        writer.delete_term(Term::from_field_text(text, "first"));
    }

    let (started_tx, started_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let closing = std::thread::spawn(move || {
        let _ = started_tx.send(());
        drop(writer);
        let _ = closed_tx.send(());
    });
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let closed_before_release = closed_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let replacement_opened_before_release = {
        let replacement: tantivy::Result<IndexWriter<TantivyDocument>> =
            index.writer_with_num_threads(1, 15_000_000);
        replacement.is_ok()
    };

    // Always unblock native cleanup before assertions, including on a broken
    // implementation where close returned and the replacement writer opened.
    gate.release();
    closing.join().unwrap();
    let merge_result = merge.wait();
    assert!(
        !closed_before_release,
        "writer teardown returned while its merge still owned files"
    );
    assert!(
        !replacement_opened_before_release,
        "the writer lock was released before the merge exited"
    );
    match pause_at {
        PauseAt::MergeOutput => {
            assert!(merge_result.is_err(), "the killed merge must not publish");
        }
        PauseAt::CommittedMetadata => {
            assert!(
                merge_result.is_ok(),
                "the already-accepted committed merge must finish before close"
            );
        }
    }
    if !matches!(outcome, MergeOutcome::Finish) {
        assert!(
            format!("{merge_result:?}").contains("injected merge write"),
            "the injected failure did not reach the merge result: {merge_result:?}"
        );
    }

    let mut reopened: IndexWriter<TantivyDocument> =
        index.writer_with_num_threads(1, 15_000_000).unwrap();
    reopened.set_merge_policy(Box::new(NoMergePolicy));
    reopened.commit().unwrap();
    drop(reopened);
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .unwrap();
    assert_eq!(reader.searcher().num_docs(), 2);
    for (term, expected) in [("first", 1), ("second", 1), ("pending", 0)] {
        let query = TermQuery::new(Term::from_field_text(text, term), IndexRecordOption::Basic);
        assert_eq!(
            reader.searcher().search(&query, &Count).unwrap(),
            expected,
            "unexpected persisted state for {term}"
        );
    }

    drop(reader);
    drop(index);
    if let Some(method) = replacement_method {
        match method {
            ReplacementMethod::Remove => std::fs::remove_dir_all(&root).unwrap(),
            ReplacementMethod::Rename => {
                std::fs::rename(&root, temp.path().join("retired")).unwrap();
            }
        }
        std::fs::create_dir(&root).unwrap();
        let replacement = Index::create_in_dir(&root, schema).unwrap();
        let mut writer: IndexWriter<TantivyDocument> =
            replacement.writer_with_num_threads(1, 15_000_000).unwrap();
        writer.set_merge_policy(Box::new(NoMergePolicy));
        writer
            .add_document(doc!(text => "replacement sentinel"))
            .unwrap();
        writer.commit().unwrap();
        drop(writer);
        drop(replacement);
        let fresh = Index::open_in_dir(&root).unwrap();
        let reader = fresh
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .unwrap();
        assert_eq!(reader.searcher().num_docs(), 1);
        for (term, expected) in [("sentinel", 1), ("first", 0), ("second", 0), ("pending", 0)] {
            let query = TermQuery::new(Term::from_field_text(text, term), IndexRecordOption::Basic);
            assert_eq!(
                reader.searcher().search(&query, &Count).unwrap(),
                expected,
                "unexpected replacement generation for {term}"
            );
        }
        drop(reader);
        drop(fresh);
    }
    // Strictly assert real file release on Windows as well as Unix. Do not
    // swallow a filesystem error or let TempDir's best-effort Drop hide it.
    std::fs::remove_dir_all(temp.path()).unwrap();
}
