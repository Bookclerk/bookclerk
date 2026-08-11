//! Bookclerk artifact target names and host selection.

use serde::{Deserialize, Serialize};

use crate::error::{CatalogError, Result};

/// Canonical Bookclerk install targets (not raw rustc triples).
pub const TARGETS: &[&str] = &[
    "linux-x64-gnu",
    "linux-arm64-gnu",
    "macos-x64",
    "macos-arm64",
    "windows-x64",
    "windows-arm64",
];

/// Map a rustc triple (or Bookclerk target) to a Bookclerk target name.
pub fn normalize_target(triple_or_target: &str) -> Option<&'static str> {
    match triple_or_target {
        "linux-x64-gnu" | "x86_64-unknown-linux-gnu" => Some("linux-x64-gnu"),
        "linux-arm64-gnu" | "aarch64-unknown-linux-gnu" => Some("linux-arm64-gnu"),
        "macos-x64" | "x86_64-apple-darwin" => Some("macos-x64"),
        "macos-arm64" | "aarch64-apple-darwin" => Some("macos-arm64"),
        "windows-x64" | "x86_64-pc-windows-msvc" => Some("windows-x64"),
        "windows-arm64" | "aarch64-pc-windows-msvc" => Some("windows-arm64"),
        _ => None,
    }
}

/// Rustc triple corresponding to a Bookclerk target (for packaging).
#[must_use]
pub fn rust_triple(bookclerk_target: &str) -> Option<&'static str> {
    match bookclerk_target {
        "linux-x64-gnu" => Some("x86_64-unknown-linux-gnu"),
        "linux-arm64-gnu" => Some("aarch64-unknown-linux-gnu"),
        "macos-x64" => Some("x86_64-apple-darwin"),
        "macos-arm64" => Some("aarch64-apple-darwin"),
        "windows-x64" => Some("x86_64-pc-windows-msvc"),
        "windows-arm64" => Some("aarch64-pc-windows-msvc"),
        _ => None,
    }
}

/// Host Bookclerk target for the machine compiling this code.
#[must_use]
pub fn host_bookclerk_target() -> &'static str {
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "linux-x64-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "linux-arm64-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "macos-x64"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "macos-arm64"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "windows-x64"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "windows-arm64"
    } else {
        "unknown"
    }
}

/// Whether the target uses zip archives.
#[must_use]
pub fn uses_zip(bookclerk_target: &str) -> bool {
    bookclerk_target.starts_with("windows-")
}

/// Archive format for a target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArchiveFormat {
    /// Gzip-compressed tar archive (default for non-Windows targets).
    TarGz,
    /// Zip archive (default for Windows targets).
    Zip,
}

impl ArchiveFormat {
    /// Returns the conventional archive format for a Bookclerk target id.
    #[must_use]
    pub fn for_target(bookclerk_target: &str) -> Self {
        if uses_zip(bookclerk_target) {
            Self::Zip
        } else {
            Self::TarGz
        }
    }

    /// File extension without dot (for example `m4b` or `mp3`).
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::Zip => "zip",
        }
    }
}

/// Select an artifact entry matching the host (or override) target.
pub fn select_target<'a, T>(
    artifacts: &'a [T],
    target_of: impl Fn(&T) -> &str,
    prefer: Option<&str>,
) -> Result<&'a T> {
    let want = prefer.unwrap_or_else(|| host_bookclerk_target());
    let want_norm = normalize_target(want).unwrap_or(want);
    if want_norm == "unknown" {
        return Err(CatalogError::message(
            "unsupported host target; pass an explicit --target",
        ));
    }
    artifacts
        .iter()
        .find(|a| normalize_target(target_of(a)).unwrap_or_else(|| target_of(a)) == want_norm)
        .ok_or_else(|| {
            let available: Vec<&str> = artifacts.iter().map(target_of).collect();
            CatalogError::message(format!(
                "no artifact for target `{want_norm}`; available: {available:?}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_rust_triples() {
        assert_eq!(
            normalize_target("x86_64-unknown-linux-gnu"),
            Some("linux-x64-gnu")
        );
        assert_eq!(
            normalize_target("x86_64-pc-windows-msvc"),
            Some("windows-x64")
        );
        assert!(uses_zip("windows-x64"));
        assert!(!uses_zip("linux-x64-gnu"));
    }

    #[test]
    fn selects_matching_artifact() {
        struct A(&'static str);
        let arts = [A("linux-x64-gnu"), A("windows-x64")];
        let got = select_target(&arts, |a| a.0, Some("windows-x64")).unwrap();
        assert_eq!(got.0, "windows-x64");
        assert!(select_target(&arts, |a| a.0, Some("macos-arm64")).is_err());
    }
}
