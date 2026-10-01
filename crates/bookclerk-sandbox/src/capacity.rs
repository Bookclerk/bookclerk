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
/// Does not create `dir` or any controller file.
#[must_use]
pub fn read_cgroup_sample(dir: &Path) -> CgroupSample {
    let (cpu_max_quota_us, cpu_max_period_us) = read_cpu_max(&dir.join("cpu.max"));
    CgroupSample {
        cpu_max_quota_us,
        cpu_max_period_us,
        memory_max_bytes: read_memory_ceiling(&dir.join("memory.max")),
        memory_current_bytes: read_u64_file(&dir.join("memory.current")),
        memory_anon_bytes: read_memory_stat_field(&dir.join("memory.stat"), "anon"),
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
/// `fsblkcnt_t` is `u64` on Linux and a narrower type on macOS.
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
    let block = u64::try_from(stat.f_frsize).ok()?;
    let avail = u64::try_from(stat.f_bavail).ok()?;
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
    fn missing_controller_files_stay_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sample = read_cgroup_sample(dir.path());
        assert_eq!(sample, CgroupSample::default());
        assert!(!cgroup_sample_is_low_resource(&sample));
    }
}
