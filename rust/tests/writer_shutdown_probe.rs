//! Temporary deterministic audit of the pinned Tantivy shutdown contract.
//! This probe deliberately verifies the pre-fix behavior; it is not a production
//! regression test until the expectation is inverted after fixing the dependency.

use std::io;
use std::path::Path;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr};
use tantivy::merge_policy::NoMergePolicy;
use tantivy::schema::{Schema, STORED, TEXT};
use tantivy::{doc, Directory, Index, IndexWriter, TantivyDocument};

#[derive(Debug, Default)]
struct GateState {
    entered: bool,
    released: bool,
}

#[derive(Debug, Default)]
struct MergeGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl MergeGate {
    fn pause_first_merge_write(&self) {
        let mut state = self.state.lock().unwrap();
        if state.entered {
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

#[derive(Clone, Debug)]
struct PausedMergeDirectory {
    inner: MmapDirectory,
    gate: Arc<MergeGate>,
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
        if std::thread::current()
            .name()
            .is_some_and(|name| name.starts_with("merge_thread_"))
        {
            // IndexMerger already owns the input segment mmaps, and `output`
            // is an open file. Keep them alive until the test releases the gate.
            self.gate.pause_first_merge_write();
        }
        Ok(output)
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
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
fn probe_drop_returns_while_a_merge_still_owns_files() {
    let temp = tempfile::tempdir().unwrap();
    let gate = Arc::new(MergeGate::default());
    let directory = PausedMergeDirectory {
        inner: MmapDirectory::open(temp.path()).unwrap(),
        gate: gate.clone(),
    };
    let mut schema = Schema::builder();
    let text = schema.add_text_field("text", TEXT | STORED);
    let index = Index::create(directory, schema.build(), Default::default()).unwrap();
    let mut writer: IndexWriter<TantivyDocument> =
        index.writer_with_num_threads(1, 15_000_000).unwrap();
    writer.set_merge_policy(Box::new(NoMergePolicy));
    for value in ["committed first", "committed second"] {
        writer.add_document(doc!(text => value)).unwrap();
        writer.commit().unwrap();
    }

    let segments = index.searchable_segment_ids().unwrap();
    assert_eq!(segments.len(), 2);
    #[cfg(windows)]
    let mapped_terms = temp
        .path()
        .join(format!("{}.term", segments[0].uuid_string()));
    let merge = writer.merge(&segments);
    if !gate.wait_until_entered() {
        gate.release();
        panic!("merge did not reach the open-file barrier");
    }
    let (closed_tx, closed_rx) = mpsc::channel();
    let closing = std::thread::spawn(move || {
        drop(writer);
        closed_tx.send(()).unwrap();
    });
    let closed_before_release = closed_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    let replacement_opened_before_release = if closed_before_release {
        let replacement: tantivy::Result<IndexWriter<TantivyDocument>> =
            index.writer_with_num_threads(1, 15_000_000);
        replacement.is_ok()
    } else {
        false
    };
    // On Windows the merge's input mmaps make a concrete committed segment
    // undeletable even after the old writer's lock has been released. Capture
    // the result now, and assert only after unblocking every native worker.
    #[cfg(windows)]
    let deletion_while_merge_paused = std::fs::remove_file(&mapped_terms);
    // Always unblock cleanup before reporting any assertion failures.
    gate.release();
    closing.join().unwrap();
    let merge_result = merge.wait();
    eprintln!(
        "pinned_tantivy_drop: returned_while_merge_files_open={closed_before_release}, \
         replacement_writer_acquired_during_merge={replacement_opened_before_release}, \
         merge_result={merge_result:?}"
    );
    assert!(closed_before_release);
    assert!(replacement_opened_before_release);
    assert!(merge_result.is_err());
    #[cfg(windows)]
    {
        eprintln!("mapped file deletion during paused merge: {deletion_while_merge_paused:?}");
        assert!(
            deletion_while_merge_paused.is_err(),
            "the pinned merge should still hold the committed segment's mmap"
        );
        // Once the merge really exits, the same file is removable. This checks
        // that the earlier failure came from live ownership, not its path.
        std::fs::remove_file(&mapped_terms).unwrap();
    }
    #[cfg(not(windows))]
    {
    let reader = index.reader().unwrap();
    assert_eq!(reader.searcher().num_docs(), 2);
    }
}
