use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

const DIRECTORY: &str = ".otzaria-retired-segments";
const PREFIX: &str = "segment-";
const SLOT_FILE: &str = "file";
const RETRY_DELAYS: [Duration; 8] = [
    Duration::from_millis(100),
    Duration::from_millis(400),
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(32),
];

// One background cleanup per index path, even where a writer is reopened. The value
// records a new retirement that may have arrived during the latest sweep.
static CLEANUPS: Mutex<BTreeMap<PathBuf, bool>> = Mutex::new(BTreeMap::new());

pub(super) fn is_transient_delete_error(error: &io::Error) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_USER_MAPPED_FILE: i32 = 1224;
    matches!(
        error.raw_os_error(),
        Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_USER_MAPPED_FILE)
    )
}

pub(super) fn is_segment_file(path: &Path) -> bool {
    if path.components().count() != 1 {
        return false;
    }
    let Some((id, suffix)) = path.to_str().and_then(|name| name.split_once('.')) else {
        return false;
    };
    if id.len() != 32 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    matches!(
        suffix,
        "idx" | "pos" | "term" | "store" | "fast" | "fieldnorm"
    ) || suffix
        .strip_suffix(".del")
        .is_some_and(|opstamp| opstamp.parse::<u64>().is_ok())
}

pub(super) fn retire(root: &Path, path: &Path) -> io::Result<()> {
    let directory = root.join(DIRECTORY);
    fs::create_dir_all(&directory)?;
    if !is_retirement_directory(&directory)? {
        return Err(io::Error::other("retired-segment directory is a redirect"));
    }
    // A freshly allocated directory means the rename can never replace another file.
    // The original Tantivy name can immediately be forgotten by its managed directory.
    let slot = tempfile::Builder::new()
        .prefix(PREFIX)
        .tempdir_in(&directory)?;
    fs::rename(root.join(path), slot.path().join(SLOT_FILE))?;
    let _ = slot.keep();
    schedule(directory);
    Ok(())
}

#[cfg(windows)]
pub(super) fn retry_pending(root: &Path) {
    let directory =
        std::path::absolute(root.join(DIRECTORY)).unwrap_or_else(|_| root.join(DIRECTORY));
    if is_retirement_directory(&directory).unwrap_or_default()
        && fs::read_dir(&directory).is_ok_and(|mut entries| entries.next().is_some())
    {
        schedule(directory);
    }
}

fn schedule(directory: PathBuf) {
    let mut active = CLEANUPS.lock().unwrap();
    if active.insert(directory.clone(), true).is_some() {
        return;
    }
    let worker_directory = directory.clone();
    if let Err(error) = thread::Builder::new()
        .name("otzaria-retired-files".to_string())
        .spawn(move || clean(worker_directory))
    {
        active.remove(&directory);
        log::warn!("could not start cleanup of {directory:?}: {error}");
    }
}

fn clean(directory: PathBuf) {
    let mut delay = 0;
    loop {
        thread::sleep(RETRY_DELAYS[delay]);
        *CLEANUPS.lock().unwrap().get_mut(&directory).unwrap() = false;
        let pending = sweep(&directory, |path| fs::remove_file(path));
        let mut active = CLEANUPS.lock().unwrap();
        if active[&directory] {
            delay = 0;
        } else if pending {
            // An active search can keep a mapping for minutes. Keep one sleeping
            // worker with bounded frequency until genuinely transient refusals end.
            delay = (delay + 1).min(RETRY_DELAYS.len() - 1);
        } else {
            active.remove(&directory);
            break;
        }
    }
}

fn sweep(directory: &Path, mut delete: impl FnMut(&Path) -> io::Result<()>) -> bool {
    match is_retirement_directory(directory) {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => return retry_delete(directory, &error),
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => return retry_delete(directory, &error),
    };
    let mut pending = false;
    for entry in entries {
        let Ok(entry) = entry else {
            pending = true;
            continue;
        };
        let slot = entry.path();
        if !entry.file_name().to_string_lossy().starts_with(PREFIX) {
            continue;
        }
        match is_retirement_directory(&slot) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                pending |= retry_delete(&slot, &error);
                continue;
            }
        }
        match delete(&slot.join(SLOT_FILE)) {
            Ok(()) => {
                let _ = fs::remove_dir(slot);
            }
            // The slot may still be waiting for its rename; only remove it if we
            // successfully deleted its contents.
            Err(error) => pending |= retry_delete(&slot, &error),
        }
    }
    pending
}

fn retry_delete(path: &Path, error: &io::Error) -> bool {
    if is_transient_delete_error(error) {
        return true;
    }
    if error.kind() != io::ErrorKind::NotFound {
        log::debug!("could not remove retired segment {path:?}: {error}");
    }
    false
}

fn is_retirement_directory(path: &Path) -> io::Result<bool> {
    let metadata = path.symlink_metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(false);
        }
    }
    Ok(metadata.file_type().is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retires_a_segment_without_touching_a_reused_original_name() {
        let root = tempfile::tempdir().unwrap();
        let name = Path::new("a5c4dfcbdfe645089129e308e26d5523.17.del");
        assert!(is_segment_file(name));
        assert!(!is_segment_file(Path::new(".tantivy-meta.lock")));
        assert!(!is_segment_file(Path::new("meta.json")));
        let original = root.path().join(name);
        fs::write(&original, b"retired").unwrap();
        retire(root.path(), name).unwrap();
        assert!(!original.exists());
        fs::write(&original, b"new contents").unwrap();

        let timeout = std::time::Instant::now() + Duration::from_secs(5);
        let retired = root.path().join(DIRECTORY);
        while fs::read_dir(&retired).unwrap().next().is_some() {
            assert!(std::time::Instant::now() < timeout);
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read(original).unwrap(), b"new contents");
    }

    #[test]
    fn a_refused_deletion_is_left_for_the_next_sweep() {
        let directory = tempfile::tempdir().unwrap();
        let slot = directory.path().join("segment-first");
        fs::create_dir(&slot).unwrap();
        let file = slot.join(SLOT_FILE);
        fs::write(&file, b"retired").unwrap();
        let refused = |_: &Path| Err(io::Error::from_raw_os_error(1224));
        assert!(sweep(directory.path(), refused));
        assert!(file.is_file());
        assert!(!sweep(directory.path(), |path| fs::remove_file(path)));
        assert!(!slot.exists());
    }

    #[test]
    fn only_windows_sharing_and_mapping_refusals_are_retried() {
        for code in [5, 32, 1224] {
            assert!(is_transient_delete_error(&io::Error::from_raw_os_error(
                code
            )));
        }
        for code in [2, 3, 19, 1117] {
            assert!(!is_transient_delete_error(&io::Error::from_raw_os_error(
                code
            )));
        }
    }
}
