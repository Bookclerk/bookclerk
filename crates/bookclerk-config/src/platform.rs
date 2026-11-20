//! Detect host distribution / OS pretty name for diagnostics payloads.

/// Best-effort OS distribution string (e.g. `Ubuntu 24.04.1 LTS`).
#[must_use]
pub fn detect_distro() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        linux_os_release()
    }
    #[cfg(target_os = "macos")]
    {
        macos_product_version()
    }
    #[cfg(target_os = "windows")]
    {
        windows_caption()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
/// Reads `/etc/os-release` and prefers `PRETTY_NAME`, else `NAME` + `VERSION`.
fn linux_os_release() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    let mut pretty = None;
    let mut name = None;
    let mut version = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("PRETTY_NAME=") {
            pretty = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("NAME=") {
            name = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("VERSION=") {
            version = Some(unquote(v));
        } else if let Some(v) = line.strip_prefix("VERSION_ID=") {
            if version.is_none() {
                version = Some(unquote(v));
            }
        }
    }
    pretty.or_else(|| match (name, version) {
        (Some(n), Some(v)) => Some(format!("{n} {v}")),
        (Some(n), None) => Some(n),
        _ => None,
    })
}

#[cfg(target_os = "linux")]
/// Strips matching single or double quotes from an os-release value.
fn unquote(v: &str) -> String {
    let v = v.trim();
    if (v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')) {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

#[cfg(target_os = "macos")]
/// Reads the macOS product version via `sw_vers` for diagnostics.
fn macos_product_version() -> Option<String> {
    let out = std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if ver.is_empty() {
        None
    } else {
        Some(format!("macOS {ver}"))
    }
}

#[cfg(target_os = "windows")]
/// Returns a lightweight Windows OS label from the environment.
fn windows_caption() -> Option<String> {
    // Prefer env-based approximation without spawning PowerShell in hot paths.
    let ver = std::env::var("OS").ok().unwrap_or_else(|| "Windows".into());
    Some(ver)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_distro_does_not_panic() {
        let _ = detect_distro();
    }
}

/// The CPU and memory a node may use, as its cgroup limits them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct NodeLimits {
    /// CPU limit in thousandths of a CPU (`250` is a quarter of one), from
    /// cgroup v2 `cpu.max`; `None` when unlimited or unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_millis: Option<u64>,
    /// Memory limit in bytes, from cgroup v2 `memory.max`; `None` when
    /// unlimited or unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    /// CPUs the process may run on (`available_parallelism`), at least 1.
    pub cpus: u64,
}

/// This process's cgroup v2 CPU and memory limits, or `None` when it has
/// neither (or the platform has no cgroups).
#[must_use]
pub fn node_limits() -> Option<NodeLimits> {
    #[cfg(target_os = "linux")]
    {
        let dir = std::fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|text| cgroup_v2_dir(&text))?;
        limits_in(&dir)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// The limits a cgroup v2 directory's `cpu.max` and `memory.max` set, or
/// `None` when neither file sets one.
///
/// # Arguments
///
/// * `dir` - The cgroup's directory under `/sys/fs/cgroup`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn limits_in(dir: &std::path::Path) -> Option<NodeLimits> {
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
    let cpu_millis = read("cpu.max").as_deref().and_then(parse_cpu_max);
    let memory_bytes = read("memory.max").as_deref().and_then(parse_memory_max);
    if cpu_millis.is_none() && memory_bytes.is_none() {
        return None;
    }
    let cpus = std::thread::available_parallelism()
        .map_or(1, |n| u64::try_from(n.get()).unwrap_or(u64::MAX));
    Some(NodeLimits {
        cpu_millis,
        memory_bytes,
        cpus,
    })
}

/// The cgroup v2 directory a `/proc/self/cgroup` text names (its `0::`
/// line) under `/sys/fs/cgroup`; `None` without a v2 line or for a path that
/// climbs out of the hierarchy.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn cgroup_v2_dir(proc_self_cgroup: &str) -> Option<std::path::PathBuf> {
    let path = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let relative = path.trim_start_matches('/');
    if relative.split('/').any(|part| part == "..") {
        return None;
    }
    Some(std::path::Path::new("/sys/fs/cgroup").join(relative))
}

/// Reads cgroup v2 `cpu.max` (`<quota> <period>` in microseconds, or `max`)
/// as thousandths of a CPU; `None` for `max` or anything unparsable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_cpu_max(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let quota: u64 = parts.next()?.parse().ok()?;
    let period: u64 = parts.next().map_or(Some(100_000), |p| p.parse().ok())?;
    if period == 0 {
        return None;
    }
    quota.checked_mul(1000).map(|q| q / period)
}

/// Reads cgroup v2 `memory.max` (bytes, or `max`); `None` for `max` or
/// anything unparsable.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_memory_max(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

#[cfg(test)]
mod limits_tests {
    use super::*;

    #[test]
    fn cpu_max_is_thousandths_of_a_cpu() {
        assert_eq!(parse_cpu_max("25000 100000\n"), Some(250));
        assert_eq!(parse_cpu_max("200000 100000"), Some(2000));
        assert_eq!(parse_cpu_max("50000"), Some(500));
        assert_eq!(parse_cpu_max("max 100000"), None);
        assert_eq!(parse_cpu_max("1 0"), None);
        assert_eq!(parse_cpu_max(""), None);
    }

    #[test]
    fn memory_max_is_bytes_or_unlimited() {
        assert_eq!(parse_memory_max("1073741824\n"), Some(1_073_741_824));
        assert_eq!(parse_memory_max("max\n"), None);
    }

    #[test]
    fn limits_come_from_the_cgroup_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(limits_in(dir.path()), None, "no files, no limits");
        std::fs::write(dir.path().join("cpu.max"), "max 100000\n").expect("cpu.max");
        std::fs::write(dir.path().join("memory.max"), "max\n").expect("memory.max");
        assert_eq!(limits_in(dir.path()), None, "unlimited is no limit");
        std::fs::write(dir.path().join("cpu.max"), "25000 100000\n").expect("cpu.max");
        std::fs::write(dir.path().join("memory.max"), "1073741824\n").expect("memory.max");
        let limits = limits_in(dir.path()).expect("limits");
        assert_eq!(limits.cpu_millis, Some(250));
        assert_eq!(limits.memory_bytes, Some(1_073_741_824));
        assert!(limits.cpus >= 1);
        let _ = node_limits();
    }

    #[test]
    fn cgroup_dir_comes_from_the_v2_line() {
        assert_eq!(
            cgroup_v2_dir("12:pids:/x\n0::/\n"),
            Some(std::path::PathBuf::from("/sys/fs/cgroup/"))
        );
        assert_eq!(
            cgroup_v2_dir("0::/user.slice/app.scope\n"),
            Some(std::path::PathBuf::from(
                "/sys/fs/cgroup/user.slice/app.scope"
            ))
        );
        assert_eq!(cgroup_v2_dir("0::/../etc\n"), None);
        assert_eq!(cgroup_v2_dir("1:name=systemd:/\n"), None);
    }
}
