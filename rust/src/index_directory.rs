//! tantivy's `MmapDirectory`, with `atomic_write` (how `meta.json` and `.managed.json` are
//! replaced) done on Windows by [`atomic_replace`], whose rename open readers do not block.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{info, warn};
use tantivy::directory::error::{
    DeleteError, LockError, OpenDirectoryError, OpenReadError, OpenWriteError,
};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, FileSlice, Lock, MmapDirectory, WatchCallback,
    WatchHandle, WritePtr,
};

/// 1,888 ms in all: outlasts a scanner holding the file, and bounds a refusal that is not
/// transient.
const REPLACE_RETRY_DELAYS: [Duration; 14] = [
    Duration::from_millis(1),
    Duration::from_millis(2),
    Duration::from_millis(5),
    Duration::from_millis(10),
    Duration::from_millis(20),
    Duration::from_millis(50),
    Duration::from_millis(100),
    Duration::from_millis(200),
    Duration::from_millis(250),
    Duration::from_millis(250),
    Duration::from_millis(250),
    Duration::from_millis(250),
    Duration::from_millis(250),
    Duration::from_millis(250),
];

#[derive(Clone, Debug)]
pub(crate) struct IndexDirectory {
    inner: MmapDirectory,
    #[cfg(windows)]
    root: PathBuf,
}

impl IndexDirectory {
    pub(crate) fn open(path: &Path) -> Result<Self, OpenDirectoryError> {
        let inner = MmapDirectory::open(path)?;
        Ok(Self {
            inner,
            // Canonical, as `MmapDirectory` resolves its paths.
            #[cfg(windows)]
            root: path
                .canonicalize()
                .map_err(|error| OpenDirectoryError::wrap_io_error(error, path.to_path_buf()))?,
        })
    }
}

impl Directory for IndexDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(path)
    }

    fn open_read(&self, path: &Path) -> Result<FileSlice, OpenReadError> {
        self.inner.open_read(path)
    }

    // Not retried: a file a reader still maps stays undeletable until it is released, and
    // tantivy's garbage collection already tries again on its next pass.
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

    // tantivy's own replaces through MoveFileExW, which any open handle on the target refuses.
    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        #[cfg(windows)]
        {
            atomic_replace(&self.root.join(path), data)
        }
        #[cfg(not(windows))]
        {
            self.inner.atomic_write(path, data)
        }
    }

    fn sync_directory(&self) -> io::Result<()> {
        self.inner.sync_directory()
    }

    fn acquire_lock(&self, lock: &Lock) -> Result<DirectoryLock, LockError> {
        self.inner.acquire_lock(lock)
    }

    fn watch(&self, watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(watch_callback)
    }
}

/// Replaces `path` with `data`: a new file beside it, synced, renamed over it. Readers see
/// the old content or the new, never part of either.
pub(crate) fn atomic_replace(path: &Path, data: &[u8]) -> io::Result<()> {
    replace(path, data).map(|_| ())
}

/// [`atomic_replace`], returning how many times the rename was refused before it succeeded.
fn replace(path: &Path, data: &[u8]) -> io::Result<usize> {
    let (temporary, mut file) = create_beside(path)?;
    let written = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);
    let mut refused = 0usize;
    let renamed = written.and_then(|()| {
        retry(
            || {
                let attempt = rename(&temporary, path);
                refused += usize::from(attempt.is_err());
                attempt
            },
            is_transient_replace_error,
            &REPLACE_RETRY_DELAYS,
            thread::sleep,
        )
    });
    match renamed {
        Ok(()) => {
            if refused > 0 {
                info!("replaced {path:?} after {refused} refused attempt(s)");
            }
            Ok(refused)
        }
        Err(error) => {
            if refused > REPLACE_RETRY_DELAYS.len() {
                warn!("gave up replacing {path:?} after {refused} refused attempts: {error}");
            }
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

fn create_beside(path: &Path) -> io::Result<(PathBuf, fs::File)> {
    static CREATED: AtomicUsize = AtomicUsize::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?
        .to_string_lossy();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut collisions = 0;
    loop {
        let n = CREATED.fetch_add(1, Ordering::Relaxed);
        let temporary =
            path.with_file_name(format!(".{name}.{}-{stamp}-{n}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && collisions < 8 => {
                collisions += 1
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_RENAME: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Makes this thread's next [`atomic_replace`] fail where a crash before the rename would.
#[cfg(test)]
pub(crate) fn fail_next_rename() {
    FAIL_NEXT_RENAME.with(|fail| fail.set(true));
}

// std's rename falls back to POSIX semantics on ERROR_ACCESS_DENIED, so a handle that shares
// delete access (tantivy's watcher and readers) no longer blocks it.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_RENAME.with(|fail| fail.replace(false)) {
        return Err(io::Error::other("rename failed by the test"));
    }
    fs::rename(from, to)
}

/// The refusals of a rename over a file another handle holds: ACCESS_DENIED, or
/// SHARING_VIOLATION when it does not share delete access. Permanent elsewhere.
fn is_transient_replace_error(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        const ERROR_ACCESS_DENIED: i32 = 5;
        const ERROR_SHARING_VIOLATION: i32 = 32;
        matches!(
            error.raw_os_error(),
            Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
        )
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

/// Runs `operation` until it succeeds, fails with an error `transient` rejects, or is
/// refused once more than `delays` has entries; the last error is returned as is.
fn retry<T>(
    mut operation: impl FnMut() -> io::Result<T>,
    transient: impl Fn(&io::Error) -> bool,
    delays: &[Duration],
    mut sleep: impl FnMut(Duration),
) -> io::Result<T> {
    let mut delays = delays.iter();
    loop {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if transient(&error) => match delays.next() {
                Some(&delay) => sleep(delay),
                None => return Err(error),
            },
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const DELAYS: [Duration; 4] = [
        Duration::from_millis(1),
        Duration::from_millis(2),
        Duration::from_millis(4),
        Duration::from_millis(8),
    ];

    fn refused(attempt: usize) -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusal {attempt}"),
        )
    }

    fn is_refused(error: &io::Error) -> bool {
        error.kind() == io::ErrorKind::PermissionDenied
    }

    #[test]
    fn retry_succeeds_once_the_refusals_stop() {
        for refusals in 0..=DELAYS.len() {
            let attempts = Cell::new(0);
            let mut slept = Vec::new();
            let result = retry(
                || {
                    attempts.set(attempts.get() + 1);
                    if attempts.get() <= refusals {
                        Err(refused(attempts.get()))
                    } else {
                        Ok(attempts.get())
                    }
                },
                is_refused,
                &DELAYS,
                |delay| slept.push(delay),
            );
            assert_eq!(result.unwrap(), refusals + 1);
            assert_eq!(slept, DELAYS[..refusals]);
        }
    }

    #[test]
    fn retry_returns_the_last_refusal_once_the_delays_run_out() {
        let attempts = Cell::new(0);
        let mut slept = Vec::new();
        let result: io::Result<()> = retry(
            || {
                attempts.set(attempts.get() + 1);
                Err(refused(attempts.get()))
            },
            is_refused,
            &DELAYS,
            |delay| slept.push(delay),
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), format!("refusal {}", DELAYS.len() + 1));
        assert_eq!(attempts.get(), DELAYS.len() + 1);
        assert_eq!(slept, DELAYS);
    }

    #[test]
    fn retry_returns_an_error_it_does_not_accept_at_once() {
        let attempts = Cell::new(0);
        let mut slept = Vec::new();
        let result: io::Result<()> = retry(
            || {
                attempts.set(attempts.get() + 1);
                if attempts.get() == 1 {
                    Err(refused(1))
                } else {
                    Err(io::Error::new(io::ErrorKind::NotFound, "gone"))
                }
            },
            is_refused,
            &DELAYS,
            |delay| slept.push(delay),
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(attempts.get(), 2);
        assert_eq!(slept, DELAYS[..1]);
    }

    #[test]
    fn only_a_windows_refusal_to_replace_is_transient() {
        assert!(!is_transient_replace_error(&io::Error::new(
            io::ErrorKind::NotFound,
            "gone"
        )));
        // Not from the OS, so not the rename's.
        assert!(!is_transient_replace_error(&refused(1)));
        #[cfg(windows)]
        {
            assert!(is_transient_replace_error(&io::Error::from_raw_os_error(5)));
            assert!(is_transient_replace_error(&io::Error::from_raw_os_error(
                32
            )));
            // ERROR_FILE_NOT_FOUND, ERROR_DISK_FULL.
            assert!(!is_transient_replace_error(&io::Error::from_raw_os_error(
                2
            )));
            assert!(!is_transient_replace_error(&io::Error::from_raw_os_error(
                112
            )));
        }
        #[cfg(not(windows))]
        {
            // EPERM, EACCES.
            assert!(!is_transient_replace_error(&io::Error::from_raw_os_error(
                1
            )));
            assert!(!is_transient_replace_error(&io::Error::from_raw_os_error(
                13
            )));
        }
    }

    #[test]
    fn the_backoff_totals_what_its_comment_says() {
        let total: Duration = REPLACE_RETRY_DELAYS.iter().sum();
        assert_eq!(total, Duration::from_millis(1_888));
    }

    fn names(dir: &tempfile::TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn atomic_replace_creates_and_replaces_leaving_no_temporary_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("meta.json");
        atomic_replace(&target, b"old").unwrap();
        atomic_replace(&target, b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(names(&dir), ["meta.json"]);
    }

    #[test]
    fn a_replace_that_fails_before_the_rename_leaves_the_old_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("meta.json");
        atomic_replace(&target, b"old").unwrap();
        fail_next_rename();
        assert!(atomic_replace(&target, b"new").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(names(&dir), ["meta.json"]);
    }

    #[cfg(windows)]
    fn hold_without_delete_sharing(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ | FILE_SHARE_WRITE.
        OpenOptions::new()
            .read(true)
            .share_mode(0x1 | 0x2)
            .open(path)
            .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn a_reader_sharing_delete_access_does_not_block_the_replace() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("meta.json");
        atomic_replace(&target, b"old").unwrap();
        let held = fs::File::open(&target).unwrap();
        assert_eq!(replace(&target, b"new").unwrap(), 0);
        drop(held);
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[cfg(windows)]
    #[test]
    fn a_holder_without_delete_sharing_is_waited_out() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("meta.json");
        atomic_replace(&target, b"old").unwrap();
        let held = hold_without_delete_sharing(&target);
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        let refused = replace(&target, b"new").unwrap();
        release.join().unwrap();
        assert!(refused > 0);
        assert_eq!(fs::read(&target).unwrap(), b"new");
        assert_eq!(names(&dir), ["meta.json"]);
    }

    #[cfg(windows)]
    #[test]
    fn a_holder_without_delete_sharing_past_the_retries_fails_the_replace() {
        let dir = tempfile::TempDir::new().unwrap();
        let target = dir.path().join("meta.json");
        atomic_replace(&target, b"old").unwrap();
        let held = hold_without_delete_sharing(&target);
        let error = replace(&target, b"new").unwrap_err();
        drop(held);
        assert!(is_transient_replace_error(&error), "{error}");
        assert_eq!(fs::read(&target).unwrap(), b"old");
        assert_eq!(names(&dir), ["meta.json"]);
    }
}
