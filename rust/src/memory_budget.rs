//! Sizes the index writer and segment merges by the memory the machine can still commit.
//!
//! Tantivy's allocations are infallible: when the OS refuses one, Rust aborts the whole
//! host process (`handle_alloc_error`, 0xc0000409 on Windows). The engine cannot catch that,
//! so the engine uses available memory to size work before starting it.

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
/// The minimum per-thread memory budget accepted by Tantivy.
const MIN_WRITER_HEAP: usize = 15_000_000;
/// Smaller arenas flush tiny segments; tantivy itself needs 15MB per thread.
const MIN_HEAP_PER_THREAD: usize = 25_000_000;
/// Default merge concurrency in tantivy; each running merge needs its own memory.
const MAX_MERGE_THREADS: usize = 4;
const AVAILABLE_PER_MERGE_THREAD: u64 = 2 << 30;
/// Estimated heap per input document for doc-id maps, fieldnorms and related buffers.
/// Merge memory also depends on the contents and number of the input segments.
const MERGE_BYTES_PER_DOC: u64 = 64;
/// One merge may take at most this share of the available memory.
const MERGE_SHARE_OF_AVAILABLE: u64 = 4;

pub(crate) fn writer_budget(
    ceiling_heap: usize,
    max_threads: usize,
    economy: bool,
    memory: Option<SystemMemory>,
) -> Option<WriterBudget> {
    let heap_bytes = match memory {
        Some(memory) => {
            let share =
                usize::try_from(memory.available / WRITER_SHARE_OF_AVAILABLE).unwrap_or(usize::MAX);
            ceiling_heap.min(share)
        }
        None => ceiling_heap,
    };
    if heap_bytes < MIN_WRITER_HEAP {
        return None;
    }
    let threads = max_threads.min(heap_bytes / MIN_HEAP_PER_THREAD).max(1);
    let merge_threads = match memory {
        _ if economy => 1,
        Some(memory) => usize::try_from(memory.available / AVAILABLE_PER_MERGE_THREAD)
            .unwrap_or(MAX_MERGE_THREADS)
            .clamp(1, MAX_MERGE_THREADS),
        None => MAX_MERGE_THREADS,
    };
    Some(WriterBudget {
        heap_bytes,
        threads,
        merge_threads,
    })
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
            workers.min(usize::try_from(fit).unwrap_or(usize::MAX))
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
    let ok = unsafe { windows::GlobalMemoryStatusEx(&mut status) } != 0;
    let host =
        (ok && status.total_phys != 0).then_some(status.avail_phys.min(status.avail_page_file));
    let available = host.into_iter().chain(job_commit_room()).min()?;
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
    let mut room = None::<u64>;
    if limits.limit_flags & LIMIT_PROCESS_MEMORY != 0 {
        room = Some(process_commit().map_or(0, |used| {
            (limits.process_memory_limit as u64).saturating_sub(used)
        }));
    }
    if limits.limit_flags & LIMIT_JOB_MEMORY != 0 {
        // With a known job limit and unavailable usage, the other processes may have
        // consumed all the headroom. Do not substitute this process's private commit.
        let mut usage = windows::JobMemoryUsage::default();
        let job_room = if windows::query_job(MEMORY_USAGE_INFORMATION, &mut usage) {
            (limits.job_memory_limit as u64).saturating_sub(usage.job_memory)
        } else {
            0
        };
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
    let host = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|meminfo| meminfo_available(&meminfo));
    let cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|cgroup| {
            let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
            cgroup_v2_room(&cgroup, &mountinfo, |path| {
                std::fs::read_to_string(path).ok()
            })
        });
    let available = host.into_iter().chain(cgroup).min()?;
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

/// The process's cgroup v2 path in its cgroup namespace.
#[cfg(any(target_os = "linux", target_os = "android", all(test, unix)))]
fn cgroup_v2_path(proc_self_cgroup: &str) -> Option<&str> {
    proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
}

/// Translate the cgroup namespace path through each cgroup2 mount. A parent's
/// charge includes siblings, so its remaining room can be less than the leaf's.
#[cfg(any(target_os = "linux", target_os = "android", all(test, unix)))]
fn cgroup_v2_room(
    cgroup: &str,
    mountinfo: &str,
    read: impl Fn(&std::path::Path) -> Option<String>,
) -> Option<u64> {
    use std::path::{Component, Path, PathBuf};

    fn valid(path: &Path) -> bool {
        path.is_absolute() && !path.components().any(|part| part == Component::ParentDir)
    }

    fn mount_path(field: &str) -> PathBuf {
        PathBuf::from(
            field
                .replace("\\040", " ")
                .replace("\\011", "\t")
                .replace("\\012", "\n")
                .replace("\\134", "\\"),
        )
    }

    let group = Path::new(cgroup_v2_path(cgroup)?);
    if !valid(group) {
        return None;
    }
    mountinfo
        .lines()
        .filter_map(|line| {
            let (mount, filesystem) = line.split_once(" - ")?;
            if filesystem.split_whitespace().next()? != "cgroup2" {
                return None;
            }
            let mut fields = mount.split_whitespace().skip(3);
            let root = mount_path(fields.next()?);
            let point = mount_path(fields.next()?);
            if !valid(&root) || !valid(&point) {
                return None;
            }
            let leaf = point.join(group.strip_prefix(&root).ok()?);
            leaf.ancestors()
                .take_while(|dir| dir.starts_with(&point))
                .filter_map(|dir| {
                    let limit = read(&dir.join("memory.max"))?.trim().parse().ok()?;
                    let Some(current) = read(&dir.join("memory.current"))
                        .and_then(|value| value.trim().parse().ok())
                    else {
                        return Some(0);
                    };
                    let stat = read(&dir.join("memory.stat")).unwrap_or_default();
                    Some(cgroup_room(limit, current, &stat))
                })
                .min()
        })
        .min()
}

/// Reclaimable page cache (`inactive_file`, as Docker counts it) is not usage.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn cgroup_room(limit: u64, current: u64, stat: &str) -> u64 {
    let inactive_file = stat
        .lines()
        .find_map(|line| line.strip_prefix("inactive_file "))
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(0);
    // A cache measurement larger than the total charge is an inconsistent sample.
    limit.saturating_sub(current.checked_sub(inactive_file).unwrap_or(current))
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
    fn writer_heap_follows_available_memory_up_to_the_ceiling() {
        let ceiling = 300_000_000;
        assert_eq!(
            writer_budget(ceiling, 5, false, None).unwrap().heap_bytes,
            ceiling
        );
        assert_eq!(
            writer_budget(ceiling, 5, false, memory(16 * GIB))
                .unwrap()
                .heap_bytes,
            ceiling
        );
        assert_eq!(
            writer_budget(ceiling, 5, false, memory(GIB))
                .unwrap()
                .heap_bytes,
            (GIB / 8) as usize
        );
        assert_eq!(writer_budget(ceiling, 5, false, memory(0)), None);
        assert_eq!(writer_budget(ceiling, 5, false, memory(100 << 20)), None);
        assert_eq!(
            writer_budget(ceiling, 5, false, memory(8 * MIN_WRITER_HEAP as u64)).unwrap(),
            WriterBudget {
                heap_bytes: MIN_WRITER_HEAP,
                threads: 1,
                merge_threads: 1
            }
        );
        let economy_ceiling = 50_000_000;
        assert_eq!(
            writer_budget(economy_ceiling, 1, true, memory(16 * GIB))
                .unwrap()
                .heap_bytes,
            economy_ceiling
        );
    }

    #[test]
    fn writer_threads_keep_a_useful_arena_each() {
        assert_eq!(
            writer_budget(300_000_000, 8, false, None).unwrap().threads,
            8
        );
        assert_eq!(
            writer_budget(300_000_000, 5, false, memory(GIB))
                .unwrap()
                .threads,
            5
        );
        assert_eq!(
            writer_budget(300_000_000, 8, false, memory(512 << 20))
                .unwrap()
                .threads,
            2
        );
        assert_eq!(writer_budget(50_000_000, 1, true, None).unwrap().threads, 1);
        for available in [128 << 20, GIB, 64 * GIB] {
            let budget = writer_budget(300_000_000, 8, false, memory(available)).unwrap();
            assert!(budget.heap_bytes / budget.threads >= MIN_WRITER_HEAP);
            assert!(budget.heap_bytes as u64 <= available / WRITER_SHARE_OF_AVAILABLE);
        }
    }

    #[test]
    fn merge_concurrency_shrinks_with_available_memory() {
        assert_eq!(
            writer_budget(300_000_000, 5, false, None)
                .unwrap()
                .merge_threads,
            4
        );
        assert_eq!(
            writer_budget(300_000_000, 5, false, memory(32 * GIB))
                .unwrap()
                .merge_threads,
            4
        );
        assert_eq!(
            writer_budget(300_000_000, 5, false, memory(5 * GIB))
                .unwrap()
                .merge_threads,
            2
        );
        assert_eq!(
            writer_budget(300_000_000, 5, false, memory(GIB))
                .unwrap()
                .merge_threads,
            1
        );
        assert_eq!(
            writer_budget(50_000_000, 1, true, None)
                .unwrap()
                .merge_threads,
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
        assert_eq!(preparation_workers(4, reservation, memory(0)), 0);
    }

    #[test]
    fn cgroup_room_does_not_count_reclaimable_page_cache() {
        let stat = "anon 104857600\nfile 3221225472\ninactive_file 3000000000\nactive_file 1\n";
        // 4GB limit, 3.3GB charged, of which 3GB is the library's inactive page cache.
        assert_eq!(
            cgroup_room(4_294_967_296, 3_326_083_072, stat),
            4_294_967_296 - (3_326_083_072 - 3_000_000_000)
        );
        assert_eq!(cgroup_room(1000, 5000, ""), 0);
    }

    #[cfg(unix)]
    #[test]
    fn cgroup_budget_uses_the_restrictive_ancestor_and_cgroup_mount_root() {
        let mountinfo = "21 0 0:6 / /unrelated rw - tmpfs tmpfs rw\n\
                         22 0 0:8 /slice /mount\\040point rw - cgroup2 none rw\n";
        let read = |path: &std::path::Path| {
            match path.to_str()? {
                "/mount point/parent/leaf/memory.max" => Some("max"),
                "/mount point/parent/memory.max" => Some("536870912"),
                "/mount point/parent/memory.current" => Some("503316480"),
                "/mount point/memory.max" => Some("1073741824"),
                "/mount point/memory.current" => Some("503316480"),
                _ => None,
            }
            .map(str::to_owned)
        };
        assert_eq!(
            cgroup_v2_room("0::/slice/parent/leaf", mountinfo, read),
            Some(32 << 20)
        );
        assert_eq!(
            cgroup_v2_room("0::/slice/parent/leaf", mountinfo, |path| {
                if path.ends_with("memory.current") {
                    None
                } else {
                    read(path)
                }
            }),
            Some(0)
        );
        assert_eq!(
            cgroup_v2_room("0::/slice/../leaf", mountinfo, |_| panic!()),
            None
        );
        assert_eq!(
            cgroup_v2_room("0::/different", mountinfo, |_| panic!()),
            None
        );
        assert_eq!(
            cgroup_v2_room("12:memory:/legacy", mountinfo, |_| panic!()),
            None
        );
    }

    #[test]
    fn meminfo_is_parsed() {
        assert_eq!(
            meminfo_available("MemTotal:  8000000 kB\nMemAvailable:  2048 kB\n"),
            Some(2048 << 10)
        );
        assert_eq!(meminfo_available("MemTotal:  8000000 kB\n"), None);
    }
}
