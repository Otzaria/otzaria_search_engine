//! Native indexing measurements; reset only while no batch is running.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static BUFFERED: AtomicU64 = AtomicU64::new(0);
static PEAK_BUFFERED: AtomicU64 = AtomicU64::new(0);
static SPILLED: AtomicU64 = AtomicU64::new(0);
static LOCK_WAIT: AtomicU64 = AtomicU64::new(0);
static LOCK_HOLD: AtomicU64 = AtomicU64::new(0);
static SOURCE_READ: AtomicU64 = AtomicU64::new(0);
static PREPARE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub struct IndexingDiagnostics {
    pub buffered_bytes: u64,
    /// Source capacities plus prepared-document upper bounds rounded up to 64 KiB.
    /// Excludes SQLite, normalized line windows and the writer; not process RSS.
    pub peak_buffered_bytes: u64,
    pub spilled_bytes: u64,
    pub source_lock_wait_micros: u64,
    pub source_lock_hold_micros: u64,
    pub source_read_micros: u64,
    pub prepare_micros: u64,
}

pub fn snapshot() -> IndexingDiagnostics {
    IndexingDiagnostics {
        buffered_bytes: BUFFERED.load(Ordering::Relaxed),
        peak_buffered_bytes: PEAK_BUFFERED.load(Ordering::Relaxed),
        spilled_bytes: SPILLED.load(Ordering::Relaxed),
        source_lock_wait_micros: LOCK_WAIT.load(Ordering::Relaxed) / 1000,
        source_lock_hold_micros: LOCK_HOLD.load(Ordering::Relaxed) / 1000,
        source_read_micros: SOURCE_READ.load(Ordering::Relaxed),
        prepare_micros: PREPARE.load(Ordering::Relaxed),
    }
}

pub fn reset() {
    for counter in [
        &PEAK_BUFFERED,
        &SPILLED,
        &LOCK_WAIT,
        &LOCK_HOLD,
        &SOURCE_READ,
        &PREPARE,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    PEAK_BUFFERED.store(BUFFERED.load(Ordering::Relaxed), Ordering::Relaxed);
}

#[derive(Default, Debug)]
pub(crate) struct BufferedBytes(u64);

impl BufferedBytes {
    /// Keep diagnostic atomics out of the document-per-line hot path. The exact
    /// local byte count still enforces spilling; diagnostics conservatively charge
    /// complete 64 KiB slabs, including the current partial slab.
    pub(crate) fn set_document_bytes(&mut self, bytes: usize) {
        const SLAB: usize = 64 << 10;
        self.set(bytes.saturating_add(SLAB - 1) / SLAB * SLAB);
    }

    pub(crate) fn set(&mut self, bytes: usize) {
        let bytes = bytes as u64;
        if bytes == self.0 {
            return;
        }
        if bytes >= self.0 {
            let current = BUFFERED.fetch_add(bytes - self.0, Ordering::Relaxed) + bytes - self.0;
            PEAK_BUFFERED.fetch_max(current, Ordering::Relaxed);
        } else {
            BUFFERED.fetch_sub(self.0 - bytes, Ordering::Relaxed);
        }
        self.0 = bytes;
    }
}

impl Drop for BufferedBytes {
    fn drop(&mut self) {
        BUFFERED.fetch_sub(self.0, Ordering::Relaxed);
    }
}

pub(crate) fn spilled(bytes: u64) {
    SPILLED.fetch_add(bytes, Ordering::Relaxed);
}
pub(crate) fn source_read(duration: Duration) {
    SOURCE_READ.fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
}
pub(crate) fn prepare(duration: Duration) {
    PREPARE.fetch_add(duration.as_micros() as u64, Ordering::Relaxed);
}
pub(crate) fn source_lock(wait: Duration, hold: Duration) {
    LOCK_WAIT.fetch_add(wait.as_nanos() as u64, Ordering::Relaxed);
    LOCK_HOLD.fetch_add(hold.as_nanos() as u64, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_accounting_reserves_partial_slabs_and_releases_them() {
        let mut tracked = BufferedBytes::default();
        tracked.set_document_bytes(1);
        assert_eq!(tracked.0, 64 << 10);
        tracked.set_document_bytes((64 << 10) - 1);
        assert_eq!(tracked.0, 64 << 10);
        tracked.set_document_bytes((64 << 10) + 1);
        assert_eq!(tracked.0, 128 << 10);
        tracked.set_document_bytes(64 << 20);
        assert_eq!(tracked.0, 64 << 20);
        tracked.set_document_bytes(0);
        assert_eq!(tracked.0, 0);
    }
}
