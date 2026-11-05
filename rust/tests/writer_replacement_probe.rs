//! Observational probe for replacing an index after writer Drop while an already
//! accepted merge is paused at its metadata write. No corruption is assumed:
//! print the actual replacement, metadata, and reopened-query results.

use std::io;
use std::path::Path;
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::Duration;

use anyhow::{ensure, Context, Result};
use tantivy::collector::Count;
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};
use tantivy::merge_policy::NoMergePolicy;
use tantivy::query::TermQuery;
use tantivy::schema::{Field, IndexRecordOption, Schema, STORED, TEXT};
use tantivy::{doc, Directory, Index, IndexWriter, ReloadPolicy, TantivyDocument, Term};

#[derive(Debug, Default)]
struct GateState {
    armed: bool,
    entered: bool,
    released: bool,
}

#[derive(Debug, Default)]
struct MetadataGate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl MetadataGate {
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

struct ReleaseMetadataOnDrop(Arc<MetadataGate>);

impl Drop for ReleaseMetadataOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[derive(Clone, Debug)]
struct PausedMetadataDirectory {
    inner: MmapDirectory,
    gate: Arc<MetadataGate>,
}

impl Directory for PausedMetadataDirectory {
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
        self.inner.open_write(path)
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        if path == Path::new("meta.json")
            && std::thread::current().name() == Some("segment_updater")
        {
            // SegmentUpdater::save_metas already passed its is_alive() check.
            // Hold the accepted write, rather than assuming that kill cancels it.
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

#[derive(Clone, Copy, Debug)]
enum ReplacementMethod {
    Remove,
    Rename,
}

struct Replacement {
    index: Index,
    metadata_before_release: Vec<u8>,
}

fn sentinel_count(index: &Index, field: Field) -> Result<usize> {
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()?;
    let query = TermQuery::new(
        Term::from_field_text(field, "sentinel"),
        IndexRecordOption::Basic,
    );
    Ok(reader.searcher().search(&query, &Count)?)
}

fn install_replacement(root: &Path, schema: Schema, field: Field) -> Result<Replacement> {
    std::fs::create_dir(root).context("create replacement directory")?;
    let index = Index::create_in_dir(root, schema).context("create replacement index")?;
    let mut writer: IndexWriter<TantivyDocument> =
        index.writer_with_num_threads(1, 15_000_000)?;
    writer.set_merge_policy(Box::new(NoMergePolicy));
    writer.add_document(doc!(field => "replacement sentinel"))?;
    writer.commit()?;
    drop(writer);
    ensure!(sentinel_count(&index, field)? == 1, "replacement was not committed");
    let metadata_before_release = std::fs::read(root.join("meta.json"))?;
    Ok(Replacement {
        index,
        metadata_before_release,
    })
}

#[test]
fn probe_remove_and_recreate_during_accepted_merge_metadata_write() {
    observe_replacement(ReplacementMethod::Remove);
}

#[test]
fn probe_rename_and_recreate_during_accepted_merge_metadata_write() {
    observe_replacement(ReplacementMethod::Rename);
}

fn observe_replacement(method: ReplacementMethod) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("index");
    let retired = temp.path().join("retired");
    std::fs::create_dir(&root).unwrap();
    let gate = Arc::new(MetadataGate::default());
    let directory = PausedMetadataDirectory {
        inner: MmapDirectory::open(&root).unwrap(),
        gate: gate.clone(),
    };
    let mut schema = Schema::builder();
    let field = schema.add_text_field("text", TEXT | STORED);
    let schema = schema.build();
    let index = Index::create(directory, schema.clone(), Default::default()).unwrap();
    let mut writer: IndexWriter<TantivyDocument> =
        index.writer_with_num_threads(1, 15_000_000).unwrap();
    writer.set_merge_policy(Box::new(NoMergePolicy));
    for value in ["original first", "original second"] {
        writer.add_document(doc!(field => value)).unwrap();
        writer.commit().unwrap();
    }
    let segments = index.searchable_segment_ids().unwrap();
    assert_eq!(segments.len(), 2);
    gate.arm();
    let merge = writer.merge(&segments);
    let _release_on_unwind = ReleaseMetadataOnDrop(gate.clone());
    assert!(
        gate.wait_until_entered(),
        "merge did not reach the accepted meta.json write"
    );

    let (started_tx, started_rx) = mpsc::channel();
    let (closed_tx, closed_rx) = mpsc::channel();
    let closing = std::thread::spawn(move || {
        let _ = started_tx.send(());
        drop(writer);
        let _ = closed_tx.send(());
    });
    started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    let closed_before_release = closed_rx.recv_timeout(Duration::from_secs(2)).is_ok();
    // Match disposal: the caller retains neither a reader nor an old Index.
    drop(index);
    let replacement = if closed_before_release {
        (|| -> Result<Replacement> {
            match method {
                ReplacementMethod::Remove => {
                    std::fs::remove_dir_all(&root).context("remove old directory")?;
                }
                ReplacementMethod::Rename => {
                    std::fs::rename(&root, &retired).context("rename old directory")?;
                }
            }
            install_replacement(&root, schema, field)
        })()
    } else {
        Err(anyhow::anyhow!("close has not completed; replacement skipped"))
    };

    // Finish all background work before assertions or reading the observations.
    gate.release();
    closing.join().unwrap();
    let merge_result = merge.wait();
    match replacement {
        Ok(replacement) => {
            let metadata_after_release = std::fs::read(root.join("meta.json"));
            let metadata_unchanged = metadata_after_release
                .as_ref()
                .is_ok_and(|bytes| bytes == &replacement.metadata_before_release);
            let reopened_sentinel = Index::open_in_dir(&root)
                .map_err(anyhow::Error::from)
                .and_then(|index| sentinel_count(&index, field));
            eprintln!(
                "replacement_probe method={method:?}: \
                 close_completed_before_release={closed_before_release}; \
                 replacement_sentinel_before_release=1; \
                 metadata_unchanged={metadata_unchanged}; \
                 reopened_sentinel={reopened_sentinel:?}; \
                 old_merge_result={merge_result:?}"
            );
            drop(replacement.index);
        }
        Err(error) => {
            eprintln!(
                "replacement_probe method={method:?}: \
                 close_completed_before_release={closed_before_release}; \
                 replacement_installation_error={error:#}; \
                 old_merge_result={merge_result:?}"
            );
        }
    }
    // A real cleanup assertion on every platform, independent of whether the
    // replacement was accepted or refused while the merge still held resources.
    std::fs::remove_dir_all(temp.path()).unwrap();
}
