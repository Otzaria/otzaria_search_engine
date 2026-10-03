//! tantivy's `MmapDirectory`, with `atomic_write` (how `meta.json` and `.managed.json` are
//! replaced) retried while Windows briefly refuses to replace a file another handle holds.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use log::{info, warn};
use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, FileSlice, Lock, MmapDirectory, WatchCallback,
    WatchHandle, WritePtr,
};

/// 1,888 ms in all: outlasts a scanner or the reader's meta-file watcher, and bounds a
/// refusal that is not transient.
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
}

impl IndexDirectory {
    pub(crate) fn new(inner: MmapDirectory) -> Self {
        Self { inner }
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

    // Safe to repeat: each attempt writes its own temporary file, and the target is
    // untouched until a rename succeeds.
    fn atomic_write(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut failed = 0usize;
        let written = retry(
            || {
                let attempt = self.inner.atomic_write(path, data);
                failed += usize::from(attempt.is_err());
                attempt
            },
            is_transient_replace_error,
            &REPLACE_RETRY_DELAYS,
            thread::sleep,
        );
        match &written {
            Ok(()) if failed > 0 => info!("replaced {path:?} after {failed} refused attempt(s)"),
            Err(error) if failed > REPLACE_RETRY_DELAYS.len() => {
                warn!("gave up replacing {path:?} after {failed} refused attempts: {error}")
            }
            _ => {}
        }
        written
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

/// `MoveFileExW`'s refusals while another handle holds the file it replaces (ACCESS_DENIED,
/// whatever the share mode) or the one it moves (SHARING_VIOLATION). Permanent elsewhere.
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
}
