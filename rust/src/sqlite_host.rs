//! Which SQLite this library talks to.
//!
//! * `sqlite-bundled` (CLI binaries, tests, build tools): SQLite is compiled into the crate
//!   and is always ready.
//! * `sqlite-host` (the app): the crate contains no SQLite. rusqlite calls go through the
//!   `sqlite3_api_routines` table of the instance Dart already loaded, so both sides share
//!   one library, one page-cache configuration and one set of file locks. Dart hands the
//!   table over by registering [`entry_address`] with `sqlite3_auto_extension` and opening
//!   any connection; until then every rusqlite wrapper would hit an `assert!` inside
//!   libsqlite3-sys, so every SQLite use in this crate first calls [`ensure_ready`].

use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(all(feature = "sqlite-host", feature = "sqlite-bundled"))]
compile_error!(
    "features `sqlite-host` and `sqlite-bundled` are mutually exclusive (libsqlite3-sys \
     silently lets `loadable_extension` win); build the app with \
     `--no-default-features --features sqlite-host`"
);
#[cfg(not(any(feature = "sqlite-host", feature = "sqlite-bundled")))]
compile_error!("enable exactly one of the features `sqlite-bundled` or `sqlite-host`");

/// Set once the host's API table has been installed (always true when bundled).
static HOST_READY: AtomicBool = AtomicBool::new(cfg!(feature = "sqlite-bundled"));

/// Test seam: lets a bundled test exercise the "host API missing" path.
#[cfg(test)]
pub(crate) static SIMULATE_UNINITIALIZED: AtomicBool = AtomicBool::new(false);

/// Whether rusqlite may be called.
pub fn is_ready() -> bool {
    #[cfg(test)]
    if SIMULATE_UNINITIALIZED.load(Ordering::Acquire) {
        return false;
    }
    HOST_READY.load(Ordering::Acquire)
}

/// The defined error every SQLite use returns instead of the wrappers' assert panic.
pub fn ensure_ready() -> Result<()> {
    if is_ready() {
        return Ok(());
    }
    anyhow::bail!(
        "SQLite host API is not initialized: register sqlite_host_entry_address() with \
         sqlite3_auto_extension and open one SQLite connection before using the engine's \
         SQLite features"
    )
}

/// Address of [`otzaria_sqlite_host_entry`] for Dart's `sqlite3_auto_extension`, or 0 when
/// this build bundles its own SQLite and needs nothing from the host.
pub fn entry_address() -> usize {
    #[cfg(feature = "sqlite-host")]
    {
        otzaria_sqlite_host_entry as *const () as usize
    }
    #[cfg(not(feature = "sqlite-host"))]
    {
        0
    }
}

/// `sqlite3_auto_extension` entry point. Runs inside every `sqlite3_open*` and installs
/// the API table on the first; later calls only return `SQLITE_OK`. It never unregisters
/// itself: `sqlite3_cancel_auto_extension` from inside the callback moves the last
/// registered extension into its slot, and the open in progress would skip that one. The
/// host may cancel it after its first open if it wants to.
///
/// # Safety
/// Called by SQLite with a valid `sqlite3_api_routines` pointer that outlives the process's
/// use of the library.
#[cfg(feature = "sqlite-host")]
#[no_mangle]
pub unsafe extern "C" fn otzaria_sqlite_host_entry(
    _db: *mut rusqlite::ffi::sqlite3,
    _pz_err: *mut *mut std::ffi::c_char,
    p_api: *mut rusqlite::ffi::sqlite3_api_routines,
) -> std::ffi::c_int {
    use rusqlite::ffi;
    if !HOST_READY.load(Ordering::Acquire) {
        if p_api.is_null() || ffi::rusqlite_extension_init2(p_api).is_err() {
            return ffi::SQLITE_ERROR;
        }
        HOST_READY.store(true, Ordering::Release);
    }
    ffi::SQLITE_OK
}

#[cfg(all(test, feature = "sqlite-host"))]
mod host_tests {
    /// Without Dart nothing installed the API table: opening must be a defined error, not
    /// the libsqlite3-sys assert. Then, on Windows with `OTZARIA_HOST_SQLITE` naming a
    /// SQLite DLL (e.g. the `sqlite3.dll` an app build ships for Dart), the same
    /// registration Dart performs installs that library's API, and the library-text
    /// equivalence suite runs through it.
    #[test]
    fn host_build_uses_only_the_sqlite_it_is_handed() {
        assert!(!super::is_ready());
        assert_ne!(super::entry_address(), 0);
        let err = crate::line_source::LineStore::open(std::path::Path::new("missing.db"))
            .err()
            .expect("opening must fail without a host API");
        assert!(err.to_string().contains("not initialized"), "{err:#}");
        assert!(crate::magic::MagicDictionary::open(std::path::Path::new("x.db")).is_err());

        #[cfg(windows)]
        if let Ok(dll) = std::env::var("OTZARIA_HOST_SQLITE") {
            register_like_dart(&dll);
            assert!(super::is_ready());
            crate::external_text_tests::library_text_answers_every_query_like_stored_text();
        }
    }

    /// `sqlite3_auto_extension(entry)` followed by one `sqlite3_open`, through the
    /// foreign library's own exports, with another extension registered after ours: the
    /// open must run both.
    #[cfg(windows)]
    fn register_like_dart(dll: &str) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT_CALLS: AtomicU32 = AtomicU32::new(0);
        unsafe extern "C" fn next_extension(
            _db: *mut c_void,
            _err: *mut *mut c_char,
            _api: *const c_void,
        ) -> c_int {
            NEXT_CALLS.fetch_add(1, Ordering::SeqCst);
            0
        }
        use std::ffi::{c_char, c_int, c_void};
        #[link(name = "kernel32")]
        extern "system" {
            fn LoadLibraryW(name: *const u16) -> *mut c_void;
            fn GetProcAddress(module: *mut c_void, name: *const c_char) -> *mut c_void;
        }
        let wide: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let module = LoadLibraryW(wide.as_ptr());
            assert!(!module.is_null(), "cannot load {dll}");
            let symbol = |name: &[u8]| {
                let address = GetProcAddress(module, name.as_ptr().cast());
                assert!(!address.is_null());
                address
            };
            let auto_extension: unsafe extern "C" fn(*const c_void) -> c_int =
                std::mem::transmute(symbol(b"sqlite3_auto_extension\0"));
            let open: unsafe extern "C" fn(*const c_char, *mut *mut c_void) -> c_int =
                std::mem::transmute(symbol(b"sqlite3_open\0"));
            let close: unsafe extern "C" fn(*mut c_void) -> c_int =
                std::mem::transmute(symbol(b"sqlite3_close\0"));
            let cancel_auto_extension: unsafe extern "C" fn(*const c_void) -> c_int =
                std::mem::transmute(symbol(b"sqlite3_cancel_auto_extension\0"));
            assert_eq!(auto_extension(super::entry_address() as *const c_void), 0);
            assert_eq!(auto_extension(next_extension as *const c_void), 0);
            let mut db = std::ptr::null_mut();
            assert_eq!(open(b":memory:\0".as_ptr().cast(), &mut db), 0);
            close(db);
            assert_eq!(
                NEXT_CALLS.load(Ordering::SeqCst),
                1,
                "the next extension was skipped"
            );
            cancel_auto_extension(next_extension as *const c_void);
        }
    }
}
