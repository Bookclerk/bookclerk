//! Plugin registry taxonomy: crates.io naming + install metadata.
//!
//! Third-party plugins are still prebuilt executables (see `docs/plugins.md`).
//! crates.io is a **discovery index**; operators install release archives
//! without a Rust toolchain (`docs/plugin-registry.md`).

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::manifest::PluginKind;
use crate::{PluginError, Result};

/// Required crates.io name prefix: `bookclerk-plugin-{kind}-{id}`.
pub const CRATE_NAME_PREFIX: &str = "bookclerk-plugin-";

/// Keyword every published plugin crate should include.
pub const REGISTRY_KEYWORD: &str = "bookclerk-plugin";

/// Shared crates.io keyword (`bookclerk`) expected on published plugin crates.
pub const PRODUCT_KEYWORD: &str = "bookclerk";

/// Kind-specific crates.io keyword (`bookclerk-source`, …).
#[must_use]
pub fn kind_keyword(kind: PluginKind) -> &'static str {
    match kind {
        PluginKind::Source => "bookclerk-source",
        PluginKind::Integration => "bookclerk-integration",
        PluginKind::Output => "bookclerk-output",
        PluginKind::Database => "bookclerk-database",
    }
}

/// Parsed crates.io crate name for a Bookclerk plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCrateName {
    /// Plugin kind encoded in the crates.io name.
    pub kind: PluginKind,
    /// Plugin id (`[a-z0-9_]{2,32}`), globally unique across kinds.
    pub id: String,
}

impl PluginCrateName {
    /// Format as `bookclerk-plugin-{kind}-{id}`.
    #[must_use]
    pub fn crate_name(&self) -> String {
        format!("{CRATE_NAME_PREFIX}{}-{}", self.kind.as_str(), self.id)
    }

    /// Parse `bookclerk-plugin-{kind}-{id}`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn parse(name: &str) -> Result<Self> {
        let rest = name.strip_prefix(CRATE_NAME_PREFIX).ok_or_else(|| {
            PluginError::message(format!(
                "crate name `{name}` must start with `{CRATE_NAME_PREFIX}`"
            ))
        })?;
        let (kind_str, id) = rest.split_once('-').ok_or_else(|| {
            PluginError::message(format!(
                "crate name `{name}` must be `{CRATE_NAME_PREFIX}{{kind}}-{{id}}`"
            ))
        })?;
        let kind = parse_kind(kind_str).ok_or_else(|| {
            PluginError::message(format!(
                "crate name `{name}`: unknown kind `{kind_str}` \
                 (expected source|integration|output|database)"
            ))
        })?;
        validate_plugin_id(id)?;
        Ok(Self {
            kind,
            id: id.to_string(),
        })
    }
}

impl fmt::Display for PluginCrateName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.crate_name())
    }
}

/// Validate plugin id segment used in crate names and `plugin.toml`.
///
/// Delegates to [`bookclerk_plugin_manifest::validate_plugin_id`] (strict
/// `[a-z0-9_]{2,32}` grammar; globally unique across kinds).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn validate_plugin_id(id: &str) -> Result<()> {
    bookclerk_plugin_manifest::validate_plugin_id(id)
        .map_err(|e| PluginError::message(e.to_string()))
}

/// Maps a crates.io / manifest kind string onto [`PluginKind`]; unknown values yield `None`.
fn parse_kind(s: &str) -> Option<PluginKind> {
    match s {
        "source" => Some(PluginKind::Source),
        "integration" => Some(PluginKind::Integration),
        "output" => Some(PluginKind::Output),
        "database" => Some(PluginKind::Database),
        _ => None,
    }
}

/// `[package.metadata.bookclerk]` published on crates.io.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookclerkPackageMetadata {
    /// Plugin ABI / package metadata API version (must be >= 1).
    pub api_version: u32,
    /// Plugin kind encoded in the crates.io name.
    pub kind: PluginKind,
    /// Plugin id (`[a-z0-9_]{2,32}`), globally unique across kinds.
    pub id: String,
    /// Optional human-readable name for catalog UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Template with `{tag}`, `{version}`, `{target}`, `{crate}` placeholders.
    ///
    /// Any HTTPS host works (GitHub/GitLab/Codeberg releases, S3/R2, CDN,
    /// self-hosted). Combined with the conventional archive filename unless
    /// [`Self::artifact_url`] is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_base_url: Option<String>,
    /// Full download URL template (overrides base + conventional filename).
    ///
    /// Placeholders: `{tag}`, `{version}`, `{target}`, `{crate}`, `{ext}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_url: Option<String>,
    /// Optional path inside the release archive where `plugin.toml` lives.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_root: Option<String>,
    /// Optional minimum Bookclerk host version required to run this plugin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_host: Option<String>,
}

impl BookclerkPackageMetadata {
    /// Ensure metadata matches the crate naming taxonomy.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub fn validate_against_crate_name(&self, crate_name: &str) -> Result<()> {
        let parsed = PluginCrateName::parse(crate_name)?;
        if parsed.kind != self.kind {
            return Err(PluginError::message(format!(
                "metadata kind `{}` does not match crate name kind `{}`",
                self.kind.as_str(),
                parsed.kind.as_str()
            )));
        }
        if parsed.id != self.id {
            return Err(PluginError::message(format!(
                "metadata id `{}` does not match crate name id `{}`",
                self.id, parsed.id
            )));
        }
        validate_plugin_id(&self.id)?;
        if self.api_version == 0 {
            return Err(PluginError::message("metadata api_version must be >= 1"));
        }
        if self.artifact_base_url.is_none() && self.artifact_url.is_none() {
            // Allowed for discover-only crates; install will need one later.
        }
        Ok(())
    }

    /// Build a download URL for a release asset.
    ///
    /// Prefers the `artifact_url` metadata field when set; otherwise
    /// `{artifact_base_url}/{crate}-{version}-{target}.{ext}` with `{ext}` =
    /// `zip` for Windows targets and `tar.gz` otherwise.
    #[must_use]
    pub fn artifact_download_url(
        &self,
        crate_name: &str,
        version: &str,
        target: &str,
    ) -> Option<String> {
        let tag = if version.starts_with('v') {
            version.to_string()
        } else {
            format!("v{version}")
        };
        let ext = if target.contains("windows") {
            "zip"
        } else {
            "tar.gz"
        };
        let fill = |template: &str| {
            template
                .replace("{tag}", &tag)
                .replace("{version}", version)
                .replace("{target}", target)
                .replace("{crate}", crate_name)
                .replace("{ext}", ext)
        };
        if let Some(full) = self.artifact_url.as_deref() {
            return Some(fill(full));
        }
        let base = self.artifact_base_url.as_deref()?;
        let file = format!("{crate_name}-{version}-{target}.{ext}");
        let filled = fill(base);
        let url = if filled.ends_with('/') {
            format!("{filled}{file}")
        } else {
            format!("{filled}/{file}")
        };
        Some(url)
    }
}

/// One catalog hit from crates.io (or a curated index).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginCatalogEntry {
    /// crates.io package name (`bookclerk-plugin-{kind}-{id}`).
    pub crate_name: String,
    /// Published crate / release version string.
    pub version: String,
    /// Short crates.io description when the index provides one.
    pub description: Option<String>,
    /// Download count from the registry index, when known.
    pub downloads: u64,
    /// Documentation URL from crate metadata, when present.
    pub documentation: Option<String>,
    /// Source repository URL from crate metadata, when present.
    pub repository: Option<String>,
    /// Project homepage URL from crate metadata, when present.
    pub homepage: Option<String>,
    /// Parsed from crate name when it matches the taxonomy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parsed: Option<PluginCrateName>,
    /// Present when crates.io returned Cargo.toml metadata (install phase).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<BookclerkPackageMetadata>,
}

/// Host target triple used when selecting release assets.
#[must_use]
pub fn host_target_triple() -> &'static str {
    // Match rustc host; keep explicit for installers that ship without rustc.
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        "x86_64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        "aarch64-unknown-linux-gnu"
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        "x86_64-apple-darwin"
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        "aarch64-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        "x86_64-pc-windows-msvc"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else {
        "unknown"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_taxonomy_names() {
        let n = PluginCrateName::parse("bookclerk-plugin-source-spotify").unwrap();
        assert_eq!(n.kind, PluginKind::Source);
        assert_eq!(n.id, "spotify");
        assert_eq!(n.crate_name(), "bookclerk-plugin-source-spotify");

        let n = PluginCrateName::parse("bookclerk-plugin-integration-my_store").unwrap();
        assert_eq!(n.kind, PluginKind::Integration);
        assert_eq!(n.id, "my_store");
    }

    #[test]
    fn rejects_bad_ids_and_kinds() {
        assert!(PluginCrateName::parse("bookclerk-plugin-source-X").is_err());
        assert!(PluginCrateName::parse("bookclerk-plugin-foo-bar").is_err());
        assert!(PluginCrateName::parse("other-plugin-source-x").is_err());
        assert!(validate_plugin_id("a").is_err());
        assert!(validate_plugin_id("_ab").is_err());
    }

    #[test]
    fn metadata_must_match_crate_name() {
        let meta = BookclerkPackageMetadata {
            api_version: 1,
            kind: PluginKind::Source,
            id: "example".into(),
            display_name: Some("Example".into()),
            artifact_base_url: Some("https://cdn.example.com/plugins/{crate}/{version}".into()),
            artifact_url: None,
            archive_root: None,
            min_host: None,
        };
        meta.validate_against_crate_name("bookclerk-plugin-source-example")
            .unwrap();
        assert!(meta
            .validate_against_crate_name("bookclerk-plugin-source-other")
            .is_err());

        let url = meta
            .artifact_download_url(
                "bookclerk-plugin-source-example",
                "0.1.0",
                "x86_64-unknown-linux-gnu",
            )
            .unwrap();
        assert_eq!(
            url,
            "https://cdn.example.com/plugins/bookclerk-plugin-source-example/0.1.0/\
             bookclerk-plugin-source-example-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    #[test]
    fn full_artifact_url_template_overrides_base() {
        let meta = BookclerkPackageMetadata {
            api_version: 1,
            kind: PluginKind::Integration,
            id: "echo".into(),
            display_name: None,
            artifact_base_url: Some("https://ignored.example/".into()),
            artifact_url: Some("https://downloads.example.com/{crate}/{tag}/{target}.{ext}".into()),
            archive_root: None,
            min_host: None,
        };
        let url = meta
            .artifact_download_url(
                "bookclerk-plugin-integration-echo",
                "1.2.3",
                "x86_64-pc-windows-msvc",
            )
            .unwrap();
        assert_eq!(
            url,
            "https://downloads.example.com/bookclerk-plugin-integration-echo/v1.2.3/\
             x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn kind_keywords() {
        assert_eq!(kind_keyword(PluginKind::Output), "bookclerk-output");
    }
}
