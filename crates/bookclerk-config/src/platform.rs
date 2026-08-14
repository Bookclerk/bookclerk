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
