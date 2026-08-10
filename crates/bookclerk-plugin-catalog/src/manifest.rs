//! Canonical Bookclerk package manifest (schema_version = 1).

use serde::{Deserialize, Serialize};

use crate::coordinate::PackageCoordinate;
use crate::error::{CatalogError, Result};
use crate::kind::{PluginKind, RuntimeIdentity};
use crate::target::{normalize_target, ArchiveFormat};

/// Product wire protocol for Workers RPC (`api_version = 1`).
pub const PROTOCOL_WORKERS_RPC: &str = "workers-rpc";

/// Current package manifest schema.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// Publisher-requested sandbox (informational until host approves).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxRequest {
    #[serde(default = "default_network")]
    pub network: String,
}

impl Default for SandboxRequest {
    fn default() -> Self {
        Self {
            network: default_network(),
        }
    }
}

fn default_network() -> String {
    "outbound".into()
}

/// Optional documentation / support links.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PackageLinks {
    pub repository: Option<String>,
    pub documentation: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub support: Option<String>,
}

/// Optional publisher signing identity (when available).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PublisherIdentity {
    pub name: Option<String>,
    pub url: Option<String>,
    pub key_id: Option<String>,
}

/// One precompiled artifact for a Bookclerk target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactTarget {
    /// Bookclerk target (`linux-x64-gnu`, …) or legacy rustc triple.
    pub target: String,
    /// HTTPS download URL (or `file:` for fixtures).
    pub url: String,
    /// Lowercase hex SHA-256 of the archive bytes (required for unattended install).
    pub archive_sha256: String,
    /// Path inside the archive to the plugin root (default `.`).
    #[serde(default = "default_root")]
    pub archive_root: String,
    /// Relative path to the executable inside the archive root.
    pub executable: String,
    /// Optional SHA-256 of the extracted executable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable_sha256: Option<String>,
}

fn default_root() -> String {
    ".".into()
}

impl ArtifactTarget {
    #[must_use]
    pub fn bookclerk_target(&self) -> String {
        normalize_target(&self.target)
            .unwrap_or(self.target.as_str())
            .to_string()
    }

    #[must_use]
    pub fn archive_format(&self) -> ArchiveFormat {
        ArchiveFormat::for_target(&self.bookclerk_target())
    }
}

/// Canonical install-grade package metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookclerkPackageManifest {
    pub schema_version: u32,
    /// Optional wire label. Prefer absent; when present use `workers-rpc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    pub api_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version_max: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_bookclerk: Option<String>,
    pub kind: PluginKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Filled by adapters; may be omitted in static index entries that nest under name/version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinate: Option<PackageCoordinate>,
    pub artifacts: Vec<ArtifactTarget>,
    #[serde(default)]
    pub sandbox: SandboxRequest,
    #[serde(default)]
    pub links: PackageLinks,
    #[serde(default)]
    pub yanked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<PublisherIdentity>,
}

impl BookclerkPackageManifest {
    #[must_use]
    pub fn runtime(&self) -> RuntimeIdentity {
        RuntimeIdentity::new(self.kind, self.id.clone())
    }

    /// Effective protocol string for receipts / wire defaults.
    #[must_use]
    pub fn effective_protocol(&self) -> String {
        normalize_protocol(self.protocol.as_deref()).unwrap_or_else(|_| PROTOCOL_WORKERS_RPC.into())
    }

    /// Validate schema + require digests for install-grade manifests.
    pub fn validate_for_install(&self) -> Result<()> {
        if self.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(CatalogError::message(format!(
                "unsupported schema_version {}; expected {MANIFEST_SCHEMA_VERSION}",
                self.schema_version
            )));
        }
        let _ = normalize_protocol(self.protocol.as_deref())?;
        if self.api_version == 0 {
            return Err(CatalogError::message("api_version must be >= 1"));
        }
        if self.id.trim().is_empty() {
            return Err(CatalogError::message("manifest id is required"));
        }
        if self.yanked {
            return Err(CatalogError::message(format!(
                "package `{}` version is yanked",
                self.id
            )));
        }
        if self.artifacts.is_empty() {
            return Err(CatalogError::message(
                "install requires at least one artifact with archive_sha256",
            ));
        }
        for art in &self.artifacts {
            if art.url.trim().is_empty() {
                return Err(CatalogError::message("artifact url is required"));
            }
            validate_sha256_hex_field("archive_sha256", &art.archive_sha256)?;
            if art.executable.trim().is_empty() {
                return Err(CatalogError::message("artifact executable is required"));
            }
            if let Some(exe) = &art.executable_sha256 {
                validate_sha256_hex_field("executable_sha256", exe)?;
            }
        }
        Ok(())
    }

    /// Parse from JSON.
    pub fn from_json(text: &str) -> Result<Self> {
        Ok(serde_json::from_str(text)?)
    }

    /// Parse from TOML (`[package.metadata.bookclerk]` style flat table or root).
    pub fn from_toml(text: &str) -> Result<Self> {
        Ok(toml::from_str(text)?)
    }
}

/// Normalize optional `protocol` for Workers RPC packages.
///
/// Accepts absent / empty / `workers-rpc` only.
pub fn normalize_protocol(protocol: Option<&str>) -> Result<String> {
    let trimmed = protocol.map(str::trim).filter(|s| !s.is_empty());
    match trimmed {
        None => Ok(PROTOCOL_WORKERS_RPC.into()),
        Some(PROTOCOL_WORKERS_RPC) => Ok(PROTOCOL_WORKERS_RPC.into()),
        Some(other) => Err(CatalogError::message(format!(
            "unsupported protocol `{other}`; expected absent or `{PROTOCOL_WORKERS_RPC}`"
        ))),
    }
}

/// Validate lowercase hex SHA-256 (64 chars).
pub fn validate_sha256_hex(s: &str) -> Result<()> {
    validate_sha256_hex_field("sha256", s)
}

/// Validate a named SHA-256 hex field (64 lowercase digits).
pub fn validate_sha256_hex_field(field: &str, s: &str) -> Result<()> {
    if s.len() != 64 || !s.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        return Err(CatalogError::message(format!(
            "{field} must be 64 lowercase hex digits, got `{s}`"
        )));
    }
    Ok(())
}

/// Decode hex SHA-256 into 32 bytes.
pub fn parse_sha256_hex(s: &str) -> Result<[u8; 32]> {
    validate_sha256_hex(s)?;
    let bytes = hex::decode(s).map_err(|e| CatalogError::message(e.to_string()))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> &'static str {
        r#"{
          "schema_version": 1,
          "api_version": 1,
          "kind": "integration",
          "id": "echo",
          "display_name": "Echo",
          "artifacts": [{
            "target": "linux-x64-gnu",
            "url": "file:///tmp/echo.tar.gz",
            "archive_sha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "executable": "bookclerk-plugin-echo-native-rust"
          }],
          "sandbox": { "network": "none" }
        }"#
    }

    #[test]
    fn validates_install_grade_manifest() {
        let m = BookclerkPackageManifest::from_json(sample_json()).unwrap();
        m.validate_for_install().unwrap();
        assert_eq!(m.runtime().id, "echo");
        assert!(m.protocol.is_none());
        assert_eq!(m.effective_protocol(), PROTOCOL_WORKERS_RPC);
    }

    #[test]
    fn accepts_explicit_workers_rpc_protocol() {
        let mut m = BookclerkPackageManifest::from_json(sample_json()).unwrap();
        m.protocol = Some(PROTOCOL_WORKERS_RPC.into());
        m.validate_for_install().unwrap();
        assert_eq!(m.effective_protocol(), PROTOCOL_WORKERS_RPC);
    }

    #[test]
    fn rejects_legacy_jsonrpc_protocol() {
        let mut m = BookclerkPackageManifest::from_json(sample_json()).unwrap();
        m.protocol = Some("jsonrpc-stdio-v1".into());
        assert!(m.validate_for_install().is_err());
    }

    #[test]
    fn rejects_unknown_protocol() {
        let mut m = BookclerkPackageManifest::from_json(sample_json()).unwrap();
        m.protocol = Some("capnp-v0".into());
        assert!(m.validate_for_install().is_err());
    }

    #[test]
    fn rejects_missing_digest() {
        let mut m = BookclerkPackageManifest::from_json(sample_json()).unwrap();
        m.artifacts[0].archive_sha256 = "short".into();
        assert!(m.validate_for_install().is_err());
    }
}
