//! Sizes the index writer and segment merges by the memory the machine can still commit.
//!
//! Tantivy's allocations are infallible: when the OS refuses one, Rust aborts the whole
//! host process (`handle_alloc_error`, 0xc0000409 on Windows). The engine cannot catch that,
//! so it must not start work whose footprint the machine cannot back.

/// Physical memory and what the process can still allocate, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SystemMemory {
    pub total: u64,
    pub available: u64,
}

/// What the writer may use: its indexing arenas, their threads and concurrent merges.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriterBudget {
    pub heap_bytes: usize,
    pub threads: usize,
    pub merge_threads: usize,
}

/// The writer's arenas take at most this share of the available memory: segment
/// serialization roughly doubles them, and searches, SQLite and the UI share the rest.
const WRITER_SHARE_OF_AVAILABLE: u64 = 8;
/// Never below the economy budget, which tantivy runs with on any machine.
pub(crate) const MIN_WRITER_HEAP: usize = 50_000_000;
/// Smaller arenas flush tiny segments; tantivy itself needs 15MB per thread.
const MIN_HEAP_PER_THREAD: usize = 25_000_000;
/// Default merge concurrency in tantivy; each running merge needs its own memory.
const MAX_MERGE_THREADS: usize = 4;
const AVAILABLE_PER_MERGE_THREAD: u64 = 2 << 30;
/// Heap a merge needs per document: doc-id maps and fieldnorms (measured 14-26 bytes,
/// see `tests/indexing_memory_probe.rs`), with room for readers and bitsets.
pub(crate) const MERGE_BYTES_PER_DOC: u64 = 64;
/// One merge may take at most this share of the available memory.
const MERGE_SHARE_OF_AVAILABLE: u64 = 4;

pub(crate) fn writer_budget(
    ceiling_heap: usize,
    max_threads: usize,
    economy: bool,
    memory: Option<SystemMemory>,
) -> WriterBudget {
    let heap_bytes = match memory {
        Some(memory) => {
            let share =
                usize::try_from(memory.available / WRITER_SHARE_OF_AVAILABLE).unwrap_or(usize::MAX);
            ceiling_heap.min(share.max(MIN_WRITER_HEAP))
        }
        None => ceiling_heap,
    };
    let threads = max_threads.min(heap_bytes / MIN_HEAP_PER_THREAD).max(1);
    let merge_threads = match memory {
        _ if economy => 1,
        Some(memory) => usize::try_from(memory.available / AVAILABLE_PER_MERGE_THREAD)
            .unwrap_or(MAX_MERGE_THREADS)
            .clamp(1, MAX_MERGE_THREADS),
        None => MAX_MERGE_THREADS,
    };
    WriterBudget {
        heap_bytes,
        threads,
        merge_threads,
    }
}

/// The most documents one merge may combine now; `u64::MAX` when memory is unknown.
pub(crate) fn merge_doc_limit(memory: Option<SystemMemory>) -> u64 {
    memory.map_or(u64::MAX, |memory| {
        memory.available / MERGE_SHARE_OF_AVAILABLE / MERGE_BYTES_PER_DOC
    })
}

/// Preparation workers whose buffer reservations fit in a quarter of the available memory.
pub(crate) fn preparation_workers(
    workers: usize,
    reservation: usize,
    memory: Option<SystemMemory>,
) -> usize {
    match memory {
        Some(memory) if reservation > 0 => {
            let fit = memory.available / 4 / reservation as u64;
            workers
                .min(usize::try_from(fit).unwrap_or(usize::MAX))
                .max(1)
        }
        _ => workers,
    }
}

#[cfg(windows)]
pub(crate) fn system_memory() -> Option<SystemMemory> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: std::mem::size_of::<MemoryStatusEx>() as u32,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    // SAFETY: `status` is a properly sized MEMORYSTATUSEX with `length` set.
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 || status.total_phys == 0 {
        return None;
    }
    // An allocation fails when the commit limit is reached, not when RAM is full.
    let mut memory = SystemMemory {
        total: status.total_phys,
        available: status.avail_phys.min(status.avail_page_file),
    };
    if let Some(limit) = job_commit_limit() {
        let used = process_commit().unwrap_or(0);
        memory.total = memory.total.min(limit);
        memory.available = memory.available.min(limit.saturating_sub(used));
    }
    Some(memory)
}

/// The commit limit of the job object this process runs in, if it sets one: the
/// Windows counterpart of a cgroup's `memory.max`.
#[cfg(windows)]
fn job_commit_limit() -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
        io_counters: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const JOB_OBJECT_LIMIT_PROCESS_MEMORY: u32 = 0x100;
    const JOB_OBJECT_LIMIT_JOB_MEMORY: u32 = 0x200;
    extern "system" {
        fn QueryInformationJobObject(
            job: isize,
            class: i32,
            info: *mut ExtendedLimit,
            length: u32,
            returned: *mut u32,
        ) -> i32;
    }
    let mut info = ExtendedLimit::default();
    // SAFETY: a null job handle queries the caller's own job; `info` is correctly sized.
    let ok = unsafe {
        QueryInformationJobObject(
            0,
            JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
            &mut info,
            std::mem::size_of::<ExtendedLimit>() as u32,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    let mut limit = None::<u64>;
    if info.limit_flags & JOB_OBJECT_LIMIT_PROCESS_MEMORY != 0 {
        limit = Some(info.process_memory_limit as u64);
    }
    if info.limit_flags & JOB_OBJECT_LIMIT_JOB_MEMORY != 0 {
        let job = info.job_memory_limit as u64;
        limit = Some(limit.map_or(job, |process| process.min(job)));
    }
    limit
}

#[cfg(windows)]
fn process_commit() -> Option<u64> {
    use std::ffi::c_void;
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn K32GetProcessMemoryInfo(process: *mut c_void, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is a PROCESS_MEMORY_COUNTERS_EX with `cb` set.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then_some(counters.private_usage as u64)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn system_memory() -> Option<SystemMemory> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kib = |key: &str| -> Option<u64> {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
            .map(|value| value << 10)
    };
    let mut memory = SystemMemory {
        total: kib("MemTotal")?,
        available: kib("MemAvailable")?,
    };
    let cgroup = |name: &str| -> Option<u64> {
        std::fs::read_to_string(format!("/sys/fs/cgroup/{name}"))
            .ok()?
            .trim()
            .parse()
            .ok()
    };
    if let Some(limit) = cgroup("memory.max") {
        memory.total = memory.total.min(limit);
        let used = cgroup("memory.current").unwrap_or(0);
        memory.available = memory.available.min(limit.saturating_sub(used));
    }
    Some(memory)
}

#[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
pub(crate) fn system_memory() -> Option<SystemMemory> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    fn memory(available: u64) -> Option<SystemMemory> {
        Some(SystemMemory {
            total: 8 * GIB,
            available,
        })
    }

    #[test]
    fn writer_heap_follows_available_memory_between_the_floor_and_ceiling() {
        let ceiling = 300_000_000;
        assert_eq!(writer_budget(ceiling, 5, false, None).heap_bytes, ceiling);
        assert_eq!(
            writer_budget(ceiling, 5, false, memory(16 * GIB)).heap_bytes,
            ceiling
        );
        assert_eq!(
            writer_budget(ceiling, 5, false, memory(GIB)).heap_bytes,
            (GIB / 8) as usize
        );
        assert_eq!(
            writer_budget(ceiling, 5, false, memory(100 << 20)).heap_bytes,
            MIN_WRITER_HEAP
        );
        // The economy budget is already the floor; memory never raises it.
        assert_eq!(
            writer_budget(MIN_WRITER_HEAP, 1, true, memory(16 * GIB)).heap_bytes,
            MIN_WRITER_HEAP
        );
    }

    #[test]
    fn writer_threads_keep_a_useful_arena_each() {
        assert_eq!(writer_budget(300_000_000, 8, false, None).threads, 8);
        assert_eq!(writer_budget(300_000_000, 5, false, memory(GIB)).threads, 5);
        assert_eq!(
            writer_budget(300_000_000, 8, false, memory(512 << 20)).threads,
            2
        );
        assert_eq!(writer_budget(MIN_WRITER_HEAP, 1, true, None).threads, 1);
        for available in [0, 1 << 20, GIB, 64 * GIB] {
            let budget = writer_budget(300_000_000, 8, false, memory(available));
            assert!(budget.heap_bytes / budget.threads >= 15_000_000);
        }
    }

    #[test]
    fn merge_concurrency_shrinks_with_available_memory() {
        assert_eq!(writer_budget(300_000_000, 5, false, None).merge_threads, 4);
        assert_eq!(
            writer_budget(300_000_000, 5, false, memory(32 * GIB)).merge_threads,
            4
        );
        assert_eq!(
            writer_budget(300_000_000, 5, false, memory(5 * GIB)).merge_threads,
            2
        );
        assert_eq!(
            writer_budget(300_000_000, 5, false, memory(GIB)).merge_threads,
            1
        );
        assert_eq!(
            writer_budget(MIN_WRITER_HEAP, 1, true, None).merge_threads,
            1
        );
    }

    #[test]
    fn one_merge_fits_in_a_quarter_of_available_memory() {
        assert_eq!(merge_doc_limit(None), u64::MAX);
        assert_eq!(merge_doc_limit(memory(0)), 0);
        let limit = merge_doc_limit(memory(2 * GIB));
        assert_eq!(limit, 2 * GIB / 4 / MERGE_BYTES_PER_DOC);
        assert!(limit * MERGE_BYTES_PER_DOC <= 2 * GIB / 4);
    }

    #[test]
    fn preparation_workers_fit_their_reservations() {
        let reservation = 96 << 20;
        assert_eq!(preparation_workers(4, reservation, None), 4);
        assert_eq!(preparation_workers(4, reservation, memory(8 * GIB)), 4);
        assert_eq!(preparation_workers(4, reservation, memory(GIB)), 2);
        assert_eq!(preparation_workers(4, reservation, memory(0)), 1);
    }

    #[test]
    fn the_running_system_reports_plausible_memory() {
        if let Some(memory) = system_memory() {
            assert!(memory.total > 0);
        }
    }
}
