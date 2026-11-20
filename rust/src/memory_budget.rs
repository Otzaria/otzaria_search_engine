//! Sizes the index writer and segment merges by the memory the machine can still commit.
//!
//! Tantivy's allocations are infallible: when the OS refuses one, Rust aborts the whole
//! host process (`handle_alloc_error`, 0xc0000409 on Windows). The engine cannot catch that,
//! so it must not start work whose footprint the machine cannot back.

/// What the process can still allocate, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SystemMemory {
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
mod windows {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct MemoryStatusEx {
        pub length: u32,
        pub memory_load: u32,
        pub total_phys: u64,
        pub avail_phys: u64,
        pub total_page_file: u64,
        pub avail_page_file: u64,
        pub total_virtual: u64,
        pub avail_virtual: u64,
        pub avail_extended_virtual: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct ExtendedLimit {
        pub per_process_user_time_limit: i64,
        pub per_job_user_time_limit: i64,
        pub limit_flags: u32,
        pub minimum_working_set_size: usize,
        pub maximum_working_set_size: usize,
        pub active_process_limit: u32,
        pub affinity: usize,
        pub priority_class: u32,
        pub scheduling_class: u32,
        pub io_counters: [u64; 6],
        pub process_memory_limit: usize,
        pub job_memory_limit: usize,
        pub peak_process_memory_used: usize,
        pub peak_job_memory_used: usize,
    }

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct JobMemoryUsage {
        pub job_memory: u64,
        pub peak_job_memory_used: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct Counters {
        pub cb: u32,
        pub page_fault_count: u32,
        pub peak_working_set_size: usize,
        pub working_set_size: usize,
        pub quota_peak_paged_pool_usage: usize,
        pub quota_paged_pool_usage: usize,
        pub quota_peak_non_paged_pool_usage: usize,
        pub quota_non_paged_pool_usage: usize,
        pub pagefile_usage: usize,
        pub peak_pagefile_usage: usize,
        pub private_usage: usize,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub(super) fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
        pub(super) fn QueryInformationJobObject(
            job: *mut c_void,
            class: i32,
            info: *mut c_void,
            length: u32,
            returned: *mut u32,
        ) -> i32;
        pub(super) fn GetCurrentProcess() -> *mut c_void;
        pub(super) fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut Counters,
            cb: u32,
        ) -> i32;
    }

    /// Queries the caller's own job object (a null handle); false when it has none.
    pub(super) fn query_job<T>(class: i32, info: &mut T) -> bool {
        // SAFETY: `info` is a correctly sized, writable struct of the class's layout.
        unsafe {
            QueryInformationJobObject(
                std::ptr::null_mut(),
                class,
                (info as *mut T).cast(),
                std::mem::size_of::<T>() as u32,
                std::ptr::null_mut(),
            ) != 0
        }
    }
}

#[cfg(windows)]
pub(crate) fn system_memory() -> Option<SystemMemory> {
    let mut status = windows::MemoryStatusEx {
        length: std::mem::size_of::<windows::MemoryStatusEx>() as u32,
        ..Default::default()
    };
    // SAFETY: `status` is a properly sized MEMORYSTATUSEX with `length` set.
    if unsafe { windows::GlobalMemoryStatusEx(&mut status) } == 0 || status.total_phys == 0 {
        return None;
    }
    // An allocation fails when the commit limit is reached, not when RAM is full.
    let mut available = status.avail_phys.min(status.avail_page_file);
    if let Some(job_room) = job_commit_room() {
        available = available.min(job_room);
    }
    Some(SystemMemory { available })
}

/// What the limits of this process's job object still leave it: the Windows
/// counterpart of a cgroup's `memory.max`. `None` when no job limits memory.
#[cfg(windows)]
fn job_commit_room() -> Option<u64> {
    const EXTENDED_LIMIT_INFORMATION: i32 = 9;
    const MEMORY_USAGE_INFORMATION: i32 = 28;
    const LIMIT_PROCESS_MEMORY: u32 = 0x100;
    const LIMIT_JOB_MEMORY: u32 = 0x200;
    let mut limits = windows::ExtendedLimit::default();
    if !windows::query_job(EXTENDED_LIMIT_INFORMATION, &mut limits) {
        return None;
    }
    let process_used = process_commit().unwrap_or(0);
    let mut room = None::<u64>;
    if limits.limit_flags & LIMIT_PROCESS_MEMORY != 0 {
        room = Some((limits.process_memory_limit as u64).saturating_sub(process_used));
    }
    if limits.limit_flags & LIMIT_JOB_MEMORY != 0 {
        // The whole job's commit; older Windows without class 28 report only this process's.
        let mut usage = windows::JobMemoryUsage::default();
        let job_used = if windows::query_job(MEMORY_USAGE_INFORMATION, &mut usage) {
            usage.job_memory
        } else {
            process_used
        };
        let job_room = (limits.job_memory_limit as u64).saturating_sub(job_used);
        room = Some(room.map_or(job_room, |process_room| process_room.min(job_room)));
    }
    room
}

#[cfg(windows)]
fn process_commit() -> Option<u64> {
    let mut counters = windows::Counters {
        cb: std::mem::size_of::<windows::Counters>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is a PROCESS_MEMORY_COUNTERS_EX with `cb` set.
    let ok = unsafe {
        windows::K32GetProcessMemoryInfo(windows::GetCurrentProcess(), &mut counters, counters.cb)
    };
    (ok != 0).then_some(counters.private_usage as u64)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub(crate) fn system_memory() -> Option<SystemMemory> {
    let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
    let mut available = meminfo_available(&meminfo)?;
    let group = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|text| cgroup_v2_path(&text).map(str::to_owned))
        .unwrap_or_default();
    let read = |name: &str| {
        std::fs::read_to_string(format!("/sys/fs/cgroup{group}/{name}")).unwrap_or_default()
    };
    if let Some(room) = cgroup_room(
        &read("memory.max"),
        &read("memory.current"),
        &read("memory.stat"),
    ) {
        available = available.min(room);
    }
    Some(SystemMemory { available })
}

#[cfg(any(target_os = "linux", target_os = "android", test))]
fn meminfo_available(meminfo: &str) -> Option<u64> {
    meminfo
        .lines()
        .find_map(|line| line.strip_prefix("MemAvailable:"))?
        .split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()
        .map(|kib| kib << 10)
}

/// The process's cgroup v2 path (the `0::` line of `/proc/self/cgroup`), "" for the root.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn cgroup_v2_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(|path| path.trim().trim_end_matches('/'))
}

/// Room left under `memory.max`. Reclaimable page cache (`inactive_file`, as Docker
/// counts it) is not usage: reading the library would otherwise leave no room at all.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn cgroup_room(max: &str, current: &str, stat: &str) -> Option<u64> {
    let limit: u64 = max.trim().parse().ok()?;
    let current: u64 = current.trim().parse().unwrap_or(0);
    let inactive_file = stat
        .lines()
        .find_map(|line| line.strip_prefix("inactive_file "))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Some(limit.saturating_sub(current.saturating_sub(inactive_file)))
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
        Some(SystemMemory { available })
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
            assert!(memory.available > 0);
        }
    }

    #[test]
    fn cgroup_room_does_not_count_reclaimable_page_cache() {
        let stat = "anon 104857600\nfile 3221225472\ninactive_file 3000000000\nactive_file 1\n";
        // 4GB limit, 3.3GB charged, of which 3GB is the library's inactive page cache.
        assert_eq!(
            cgroup_room("4294967296\n", "3326083072\n", stat),
            Some(4_294_967_296 - (3_326_083_072 - 3_000_000_000))
        );
        assert_eq!(cgroup_room("max\n", "3326083072\n", stat), None);
        assert_eq!(cgroup_room("", "", ""), None);
        assert_eq!(cgroup_room("1000", "5000", ""), Some(0));
        assert_eq!(cgroup_room("1000", "", ""), Some(1000));
    }

    #[test]
    fn cgroup_v2_path_and_meminfo_are_parsed() {
        assert_eq!(
            cgroup_v2_path("0::/system.slice/otzaria.service\n"),
            Some("/system.slice/otzaria.service")
        );
        assert_eq!(cgroup_v2_path("0::/\n"), Some(""));
        assert_eq!(cgroup_v2_path("12:memory:/legacy\n"), None);
        assert_eq!(
            meminfo_available("MemTotal:  8000000 kB\nMemAvailable:  2048 kB\n"),
            Some(2048 << 10)
        );
        assert_eq!(meminfo_available("MemTotal:  8000000 kB\n"), None);
    }
}
