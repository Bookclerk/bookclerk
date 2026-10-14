//! Process cgroup observations for host heartbeats and the media-worker preset.
//!
//! The reader never creates a cgroup. Missing controller files stay empty.
//! Tests point [`read_cgroup_sample`] at a directory of fake `cpu.max` and
//! `memory.max` files so CI does not need a delegated cgroup.

use std::path::{Path, PathBuf};

/// `memory.max` at or below this many bytes is the small-VPS ceiling.
pub const LOW_RESOURCE_MEMORY_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Parsed cgroup v2 files for the process that is actually running.
///
/// Unlimited (`max`) and missing files are [`None`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CgroupSample {
    /// `cpu.max` quota in microseconds. Empty when the file is missing or `max`.
    pub cpu_max_quota_us: Option<u64>,
    /// `cpu.max` period in microseconds. Empty when the file is missing or `max`.
    pub cpu_max_period_us: Option<u64>,
    /// `memory.max` in bytes. Empty when the file is missing or `max`.
    pub memory_max_bytes: Option<u64>,
    /// `memory.current` in bytes.
    pub memory_current_bytes: Option<u64>,
    /// `memory.stat` field `anon`.
    pub memory_anon_bytes: Option<u64>,
}

/// Logical CPUs from [`std::thread::available_parallelism`], or empty on failure.
#[must_use]
pub fn logical_cpu_count() -> Option<i64> {
    let count = std::thread::available_parallelism().ok()?.get();
    i64::try_from(count).ok()
}

/// Directory of this process's cgroup v2 node (`/proc/self/cgroup` `0::` path).
///
/// Returns [`None`] when the file is missing, has no v2 entry, or the
/// corresponding `/sys/fs/cgroup` directory does not exist.
#[must_use]
pub fn process_cgroup_dir() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        for line in raw.lines() {
            let Some(path) = line.strip_prefix("0::") else {
                continue;
            };
            let rel = path.trim_start_matches('/');
            let dir = if rel.is_empty() {
                PathBuf::from("/sys/fs/cgroup")
            } else {
                Path::new("/sys/fs/cgroup").join(rel)
            };
            return dir.is_dir().then_some(dir);
        }
        None
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Reads `cpu.max`, `memory.max`, `memory.current`, and `memory.stat` in `dir`.
///
/// Effective `memory.max` is the minimum ceiling among `dir` and ancestors
/// that still publish `memory.max` or `cpu.max`. Effective `cpu.max` is the
/// tighter quota/period ratio on that same walk. `memory.current` and `anon`
/// stay the leaf values. The walk stops when a parent has neither controller
/// file, so a temporary test directory does not escape into `/tmp`.
///
/// Does not create `dir` or any controller file.
#[must_use]
pub fn read_cgroup_sample(dir: &Path) -> CgroupSample {
    let mut cpu = read_cpu_max(&dir.join("cpu.max"));
    let mut memory_max_bytes = read_memory_ceiling(&dir.join("memory.max"));
    let mut cursor = dir.to_path_buf();
    while let Some(parent) = cursor.parent().map(Path::to_path_buf) {
        if parent == cursor {
            break;
        }
        let parent_memory = parent.join("memory.max");
        let parent_cpu = parent.join("cpu.max");
        if !parent_memory.is_file() && !parent_cpu.is_file() {
            break;
        }
        if parent_memory.is_file() {
            if let Some(ceiling) = read_memory_ceiling(&parent_memory) {
                memory_max_bytes = Some(
                    memory_max_bytes
                        .map(|current| current.min(ceiling))
                        .unwrap_or(ceiling),
                );
            }
        }
        if parent_cpu.is_file() {
            cpu = tighter_cpu(cpu, read_cpu_max(&parent_cpu));
        }
        cursor = parent;
    }
    CgroupSample {
        cpu_max_quota_us: cpu.0,
        cpu_max_period_us: cpu.1,
        memory_max_bytes,
        memory_current_bytes: read_u64_file(&dir.join("memory.current")),
        memory_anon_bytes: read_memory_stat_field(&dir.join("memory.stat"), "anon"),
    }
}

/// Keeps the cpu.max pair with the smaller quota/period ratio.
///
/// A missing pair loses to a present one. The comparison multiplies in `u128`
/// so a large quota and period cannot overflow.
fn tighter_cpu(
    current: (Option<u64>, Option<u64>),
    parent: (Option<u64>, Option<u64>),
) -> (Option<u64>, Option<u64>) {
    match (current, parent) {
        ((None, _), parent) => parent,
        (current, (None, _)) => current,
        ((Some(cq), Some(cp)), (Some(pq), Some(pp))) if cp > 0 && pp > 0 => {
            let left = u128::from(cq).saturating_mul(u128::from(pp));
            let right = u128::from(pq).saturating_mul(u128::from(cp));
            if left <= right {
                (Some(cq), Some(cp))
            } else {
                (Some(pq), Some(pp))
            }
        }
        (current, _) => current,
    }
}

/// True when `memory.max` is at most 1 GiB or `cpu.max` is at most one core.
///
/// Missing files and the unlimited token `max` are not a low-resource signal.
#[must_use]
pub fn cgroup_sample_is_low_resource(sample: &CgroupSample) -> bool {
    if sample
        .memory_max_bytes
        .is_some_and(|bytes| bytes <= LOW_RESOURCE_MEMORY_MAX_BYTES)
    {
        return true;
    }
    match (sample.cpu_max_quota_us, sample.cpu_max_period_us) {
        (Some(quota), Some(period)) if period > 0 => quota <= period,
        _ => false,
    }
}

/// Free bytes available to unprivileged callers (`statvfs` `f_bavail`).
///
/// Returns [`None`] when the path cannot be stated. Non-Unix hosts stay empty.
#[must_use]
pub fn filesystem_free_bytes(path: &Path) -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        filesystem_free_bytes_unix(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = path;
        None
    }
}

/// `statvfs` on Linux and macOS.
///
/// `f_frsize` is `c_ulong`. `f_bavail` is `fsblkcnt_t`: `u64` on Linux and
/// `u32` on macOS. [`u64::from`] accepts both. The Linux identity conversion
/// is an allowed no-op.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unsafe_code, clippy::useless_conversion)]
fn filesystem_free_bytes_unix(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    let block = u64::from(stat.f_frsize);
    let avail = u64::from(stat.f_bavail);
    Some(avail.saturating_mul(block))
}

/// Parses `cpu.max`. The token `max` leaves both fields empty.
fn read_cpu_max(path: &Path) -> (Option<u64>, Option<u64>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let mut parts = text.split_whitespace();
    let Some(first) = parts.next() else {
        return (None, None);
    };
    if first == "max" {
        return (None, None);
    }
    let Ok(quota) = first.parse::<u64>() else {
        return (None, None);
    };
    let Some(second) = parts.next() else {
        return (None, None);
    };
    let Ok(period) = second.parse::<u64>() else {
        return (None, None);
    };
    (Some(quota), Some(period))
}

/// Parses `memory.max`. The token `max` is unlimited and stays empty.
fn read_memory_ceiling(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let token = text.split_whitespace().next()?;
    if token == "max" {
        return None;
    }
    token.parse().ok()
}

/// Parses a single integer controller file.
fn read_u64_file(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// Reads one whitespace-separated field from `memory.stat`.
fn read_memory_stat_field(path: &Path, field: &str) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() == Some(field) {
            return parts.next()?.parse().ok();
        }
    }
    None
}

/// Whole-machine memory and CPU counters from `/proc` (Linux).
///
/// The heartbeat diffs two [`CpuTimes`] samples to get utilization. Other
/// platforms report nothing and the scheduler falls back to the detected
/// capacity alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemMemory {
    /// `MemTotal` in bytes.
    pub total_bytes: Option<u64>,
    /// `MemAvailable` in bytes: what new work can use without swapping.
    pub available_bytes: Option<u64>,
}

/// Cumulative CPU time counters. Two samples give utilization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuTimes {
    /// Aggregate `/proc/stat` line: busy and total jiffies across all CPUs.
    System {
        /// Jiffies spent not idle (`user + nice + system + irq + softirq + steal`).
        busy: u64,
        /// All jiffies, idle and iowait included.
        total: u64,
    },
    /// cgroup v2 `cpu.stat` `usage_usec` of a CPU-limited cgroup.
    Cgroup {
        /// Microseconds of CPU the cgroup used since it was created.
        usage_usec: u64,
    },
}

/// Reads `MemTotal` / `MemAvailable` from `/proc/meminfo`.
#[must_use]
pub fn read_system_memory() -> SystemMemory {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .map(|text| parse_meminfo(&text))
            .unwrap_or_default()
    }
    #[cfg(not(target_os = "linux"))]
    {
        SystemMemory::default()
    }
}

/// Parses `/proc/meminfo` text (values are in kB).
#[must_use]
pub fn parse_meminfo(text: &str) -> SystemMemory {
    let field = |name: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(name)?.strip_prefix(':')?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some(kb.saturating_mul(1024))
        })
    };
    SystemMemory {
        total_bytes: field("MemTotal"),
        available_bytes: field("MemAvailable"),
    }
}

/// Reads CPU time counters for utilization: the cgroup's own usage when the
/// process runs under a CPU-limited cgroup (a container quota is the real
/// ceiling), else the whole machine (a desktop's other applications count
/// against what Bookclerk can use).
#[must_use]
pub fn read_cpu_times(cgroup_dir: Option<&Path>, cgroup: &CgroupSample) -> Option<CpuTimes> {
    if cgroup.cpu_max_quota_us.is_some() {
        if let Some(dir) = cgroup_dir {
            if let Some(usage_usec) = std::fs::read_to_string(dir.join("cpu.stat"))
                .ok()
                .and_then(|text| parse_cpu_stat_usage(&text))
            {
                return Some(CpuTimes::Cgroup { usage_usec });
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|text| parse_proc_stat(&text))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `usage_usec` from cgroup v2 `cpu.stat` text.
#[must_use]
pub fn parse_cpu_stat_usage(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == "usage_usec").then(|| parts.next()?.parse().ok())?
    })
}

/// Aggregate `cpu` line of `/proc/stat` text.
#[must_use]
pub fn parse_proc_stat(text: &str) -> Option<CpuTimes> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    // user nice system idle iowait irq softirq steal [guest guest_nice]
    if values.len() < 4 {
        return None;
    }
    let get = |i: usize| values.get(i).copied().unwrap_or(0);
    let idle = get(3).saturating_add(get(4));
    let total: u64 = values.iter().take(8).sum();
    Some(CpuTimes::System {
        busy: total.saturating_sub(idle),
        total,
    })
}

/// CPU in use between two samples, in millicores (1000 = one core busy).
///
/// `cores_millis` scales the machine-wide busy fraction; `elapsed` is the
/// wall time between a cgroup's two samples. Returns [`None`] when the
/// samples are of different kinds or did not advance.
#[must_use]
pub fn cpu_used_millicores(
    before: CpuTimes,
    after: CpuTimes,
    cores_millis: u64,
    elapsed: std::time::Duration,
) -> Option<u64> {
    match (before, after) {
        (
            CpuTimes::System {
                busy: b0,
                total: t0,
            },
            CpuTimes::System {
                busy: b1,
                total: t1,
            },
        ) => {
            let total = t1.checked_sub(t0).filter(|t| *t > 0)?;
            let busy = b1.saturating_sub(b0).min(total);
            Some(busy.saturating_mul(cores_millis) / total)
        }
        (CpuTimes::Cgroup { usage_usec: u0 }, CpuTimes::Cgroup { usage_usec: u1 }) => {
            let wall = u64::try_from(elapsed.as_micros()).ok().filter(|w| *w > 0)?;
            Some(u1.saturating_sub(u0).saturating_mul(1000) / wall)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_cgroup_files_parse_and_unlimited_stays_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("cpu.max"), "100000 100000\n").unwrap();
        std::fs::write(dir.path().join("memory.max"), "1073741824\n").unwrap();
        std::fs::write(dir.path().join("memory.current"), "4096\n").unwrap();
        std::fs::write(dir.path().join("memory.stat"), "anon 2048\nfile 99\n").unwrap();
        let sample = read_cgroup_sample(dir.path());
        assert_eq!(sample.cpu_max_quota_us, Some(100_000));
        assert_eq!(sample.cpu_max_period_us, Some(100_000));
        assert_eq!(sample.memory_max_bytes, Some(1_073_741_824));
        assert_eq!(sample.memory_current_bytes, Some(4096));
        assert_eq!(sample.memory_anon_bytes, Some(2048));
        assert!(cgroup_sample_is_low_resource(&sample));

        std::fs::write(dir.path().join("cpu.max"), "max\n").unwrap();
        std::fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        let open = read_cgroup_sample(dir.path());
        assert_eq!(open.cpu_max_quota_us, None);
        assert_eq!(open.cpu_max_period_us, None);
        assert_eq!(open.memory_max_bytes, None);
        assert!(!cgroup_sample_is_low_resource(&open));
    }

    #[test]
    fn half_core_is_low_resource_and_two_cores_are_not() {
        let half = CgroupSample {
            cpu_max_quota_us: Some(50_000),
            cpu_max_period_us: Some(100_000),
            memory_max_bytes: Some(LOW_RESOURCE_MEMORY_MAX_BYTES + 1),
            ..CgroupSample::default()
        };
        assert!(cgroup_sample_is_low_resource(&half));
        let two = CgroupSample {
            cpu_max_quota_us: Some(200_000),
            cpu_max_period_us: Some(100_000),
            memory_max_bytes: Some(LOW_RESOURCE_MEMORY_MAX_BYTES + 1),
            ..CgroupSample::default()
        };
        assert!(!cgroup_sample_is_low_resource(&two));
    }

    #[test]
    fn parent_ceiling_is_the_effective_limit() {
        let parent = tempfile::tempdir().expect("tempdir");
        std::fs::write(parent.path().join("memory.max"), "1073741824\n").unwrap();
        std::fs::write(parent.path().join("cpu.max"), "100000 100000\n").unwrap();
        let child = parent.path().join("child");
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("memory.max"), "2147483648\n").unwrap();
        std::fs::write(child.join("memory.current"), "10\n").unwrap();
        std::fs::write(child.join("memory.stat"), "anon 4\n").unwrap();
        let sample = read_cgroup_sample(&child);
        assert_eq!(sample.memory_max_bytes, Some(1_073_741_824));
        assert_eq!(sample.cpu_max_quota_us, Some(100_000));
        assert_eq!(sample.cpu_max_period_us, Some(100_000));
        assert_eq!(sample.memory_current_bytes, Some(10));
        assert_eq!(sample.memory_anon_bytes, Some(4));
    }

    #[test]
    fn missing_controller_files_stay_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sample = read_cgroup_sample(dir.path());
        assert_eq!(sample, CgroupSample::default());
        assert!(!cgroup_sample_is_low_resource(&sample));
    }

    #[test]
    fn meminfo_and_cpu_counters_parse() {
        let mem = super::parse_meminfo(
            "MemTotal:       16303428 kB\nMemFree:  100 kB\nMemAvailable:    8151714 kB\n",
        );
        assert_eq!(mem.total_bytes, Some(16_303_428 * 1024));
        assert_eq!(mem.available_bytes, Some(8_151_714 * 1024));
        assert_eq!(
            super::parse_cpu_stat_usage("usage_usec 1500\nuser_usec 1000\n"),
            Some(1500)
        );
        let before = super::parse_proc_stat("cpu  100 0 100 700 100 0 0 0 0 0\ncpu0 1 1 1 1\n")
            .expect("cpu line");
        let after =
            super::parse_proc_stat("cpu  200 0 200 1300 300 0 0 0 0 0\n").expect("cpu line");
        // 200 busy of 1000 jiffies on a 4-core machine is 0.8 cores.
        assert_eq!(
            super::cpu_used_millicores(before, after, 4000, std::time::Duration::from_secs(1)),
            Some(800)
        );
        let used = super::cpu_used_millicores(
            super::CpuTimes::Cgroup { usage_usec: 0 },
            super::CpuTimes::Cgroup {
                usage_usec: 500_000,
            },
            2000,
            std::time::Duration::from_secs(1),
        );
        assert_eq!(used, Some(500), "half a core over one second");
        assert_eq!(
            super::cpu_used_millicores(before, before, 4000, std::time::Duration::from_secs(1)),
            None,
            "no elapsed jiffies"
        );
    }
}
