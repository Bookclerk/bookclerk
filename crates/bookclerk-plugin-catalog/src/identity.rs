//! Provenance-qualified plugin identity.
//!
//! A bare `plugin.toml` `id` (`sqlite`, `local`, `audible`) is a **display
//! alias**, not a security principal. Durable ownership uses [`PluginKey`]:
//! canonical provenance + package coordinate. The alias is not part of the
//! key, so a same-package rename keeps identity stable. Version and content
//! hashes are **not** part of the key; they belong on [`ArtifactIdentity`].
//!
//! Aliases are unique within **one host plugin namespace** (`$BOOKCLERK_FILES_DIR`),
//! conceptually `(HostId, alias) → PluginKey`. A future stable `HostId` may
//! be persisted locally (for example `$FILES_DIR/host.json`) for shared-database
//! placement; this crate does not implement HostId, cluster inventory, or
//! database advisory locks. Two hosts with distinct files dirs may install the
//! same alias and PluginKey independently.
//!
//! Platform trust is host-controlled ([`PluginProvenance::PlatformBundled`])
//! only after the host install ledger records the exact PluginKey and payload
//! digests for a known platform artifact. A third-party package that declares
//! `id = "sqlite"` does not receive platform defaults. `receipt.json` is
//! metadata; it cannot manufacture platform authority.

use std::fmt;
use std::path::{Component, Path};
use std::str::FromStr;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

use crate::coordinate::{PackageCoordinate, RegistrySource};
use crate::error::{CatalogError, Result};
use bookclerk_plugin_manifest::validate_plugin_id;

/// Product name recorded on Bookclerk-shipped platform artifacts.
pub const PLATFORM_PRODUCT: &str = "bookclerk";

/// Hex characters of SHA-256 used in [`PluginKey::fs_id`] (128 bits).
pub const PLUGIN_KEY_FS_ID_HEX_CHARS: usize = 32;

/// Default Cargo registry origin used in canonical `cargo:` PluginKeys.
pub const CRATES_IO_INDEX: &str = "https://crates.io";

/// Known installer-shipped platform packages. Host-controlled; never read from
/// plugin-authored manifest fields other than matching `id` after provenance
/// is already established.
pub const PLATFORM_ARTIFACTS: &[PlatformArtifact] = &[
    PlatformArtifact {
        package_name: "bookclerk-plugin-database-sqlite",
        manifest_id: "sqlite",
        allowed_entrypoints: &["databaseAdapter"],
        allowed_bindings: &["config", "work_fs"],
    },
    PlatformArtifact {
        package_name: "bookclerk-plugin-destination-local",
        manifest_id: "local",
        allowed_entrypoints: &["storage"],
        allowed_bindings: &["config", "work_fs"],
    },
];

/// Host-controlled first-party database adapter identities.
///
/// Secret injection, library-path grants, and host-private connect params
/// require this exact package + manifest id **and** a ledger-backed
/// provenance. A third-party tree that merely reuses the `sqlite` /
/// `postgres` / `d1` alias never matches.
pub const FIRST_PARTY_DATABASE_ADAPTERS: &[PlatformArtifact] = &[
    PlatformArtifact {
        package_name: "bookclerk-plugin-database-sqlite",
        manifest_id: "sqlite",
        allowed_entrypoints: &["databaseAdapter"],
        allowed_bindings: &["config", "work_fs"],
    },
    PlatformArtifact {
        package_name: "bookclerk-plugin-database-postgres",
        manifest_id: "postgres",
        allowed_entrypoints: &["databaseAdapter"],
        allowed_bindings: &["config", "work_fs"],
    },
    PlatformArtifact {
        package_name: "bookclerk-plugin-database-d1",
        manifest_id: "d1",
        allowed_entrypoints: &["databaseAdapter"],
        allowed_bindings: &["config", "work_fs"],
    },
];

/// One Bookclerk-shipped platform package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlatformArtifact {
    /// Cargo package name staged by `install-platform`.
    pub package_name: &'static str,
    /// Manifest alias (`sqlite`, `local`).
    pub manifest_id: &'static str,
    /// Entrypoints the installer envelope permits.
    pub allowed_entrypoints: &'static [&'static str],
    /// Host bindings the installer envelope permits.
    pub allowed_bindings: &'static [&'static str],
}

impl PlatformArtifact {
    /// [`PluginKey`] for this platform package.
    ///
    /// # Panics
    ///
    /// Panics if a baked-in [`PLATFORM_ARTIFACTS`] package name or manifest id
    /// fails the PluginKey grammar. Those rows are compile-time constants.
    #[must_use]
    pub fn plugin_key(self) -> PluginKey {
        PluginKey::platform(self.package_name, self.manifest_id)
            .expect("platform artifact ids are valid")
    }
}

/// Looks up a known platform artifact by package name + manifest id.
#[must_use]
pub fn platform_artifact(
    package_name: &str,
    manifest_id: &str,
) -> Option<&'static PlatformArtifact> {
    PLATFORM_ARTIFACTS
        .iter()
        .find(|a| a.package_name == package_name && a.manifest_id == manifest_id)
}

/// True when `key` is a known Bookclerk platform [`PluginKey`].
#[must_use]
pub fn is_platform_plugin_key(key: &PluginKey) -> bool {
    PLATFORM_ARTIFACTS.iter().any(|a| a.plugin_key() == *key)
}

/// Host-controlled first-party destination identities (`local`, `s3`).
///
/// Typed `[output.local]` / `[output.s3]` secrets and extra jail grants require
/// this exact package on `platform:` or the canonical crates.io `cargo:` index
/// **and** a verified artifact. A third-party tree that merely reuses the
/// `local` / `s3` alias never matches. The live manifest alias is **not**
/// consulted here.
pub const FIRST_PARTY_DESTINATIONS: &[PlatformArtifact] = &[
    PlatformArtifact {
        package_name: "bookclerk-plugin-destination-local",
        manifest_id: "local",
        allowed_entrypoints: &["storage"],
        allowed_bindings: &["config", "work_fs"],
    },
    PlatformArtifact {
        package_name: "bookclerk-plugin-destination-s3",
        manifest_id: "s3",
        allowed_entrypoints: &["storage"],
        allowed_bindings: &["config", "secrets"],
    },
];

/// True when `key` is a host-controlled first-party package on `platform:` or
/// the canonical crates.io `cargo:` index.
fn first_party_scheme_ok(key: &PluginKey) -> bool {
    match key.scheme() {
        ProvenanceScheme::Platform => true,
        ProvenanceScheme::Cargo => key
            .canonical()
            .starts_with(&format!("cargo:{CRATES_IO_INDEX}#")),
        _ => false,
    }
}

/// True when `key` matches an exact package name in `table` on a first-party
/// scheme. Path / npm / PyPI / third-party cargo indexes never inherit
/// host-private behavior from an alias.
fn first_party_named(key: &PluginKey, table: &[PlatformArtifact]) -> bool {
    table.iter().any(|a| a.package_name == key.package()) && first_party_scheme_ok(key)
}

/// True when `key` names a host-controlled first-party database adapter.
///
/// Matches exact package name on `platform:` or the canonical crates.io
/// `cargo:` index. Callers that inject host-private state must also require
/// verified provenance and the expected alias on the discovered plugin.
#[must_use]
pub fn is_first_party_database_adapter(key: &PluginKey) -> bool {
    first_party_named(key, FIRST_PARTY_DATABASE_ADAPTERS)
}

/// True when `key` names a host-controlled first-party destination (`local` / `s3`).
#[must_use]
pub fn is_first_party_destination(key: &PluginKey) -> bool {
    first_party_named(key, FIRST_PARTY_DESTINATIONS)
}

/// Provenance scheme in a canonical [`PluginKey`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvenanceScheme {
    /// crates.io / Cargo registry.
    Cargo,
    /// npm registry.
    Npm,
    /// PyPI.
    Pypi,
    /// Static HTTPS plugin index.
    Registry,
    /// Bookclerk-shipped installer artifact.
    Platform,
    /// Local filesystem install / development tree.
    Path,
}

impl ProvenanceScheme {
    /// Canonical scheme token used in [`PluginKey`] text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Npm => "npm",
            Self::Pypi => "pypi",
            Self::Registry => "registry",
            Self::Platform => "platform",
            Self::Path => "path",
        }
    }

    /// Parses a canonical scheme token.
    fn parse(s: &str) -> Option<Self> {
        match s {
            "cargo" => Some(Self::Cargo),
            "npm" => Some(Self::Npm),
            "pypi" => Some(Self::Pypi),
            "registry" => Some(Self::Registry),
            "platform" => Some(Self::Platform),
            "path" => Some(Self::Path),
            _ => None,
        }
    }
}

impl fmt::Display for ProvenanceScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Stable identity across upgrades (not version, not artifact hash).
///
/// Canonical text (deterministic, unambiguous). Registry index URL and
/// package name are `#`-separated so a registry path cannot be confused
/// with a package path. The human alias is **not** included (it lives on
/// the manifest / receipt `runtime.id`):
///
/// - `cargo:{index_url}#{package}`
/// - `npm:{index_url}#{package}`
/// - `pypi:{index_url}#{package}`
/// - `registry:{index_url}#{package}`
/// - `platform:bookclerk/{package}`
/// - `path:file://{normalized_absolute_path}`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginKey {
    /// Canonical UTF-8 form (see type docs). Always produced by [`Self::parse`].
    canonical: String,
}

impl Serialize for PluginKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical)
    }
}

impl<'de> Deserialize<'de> for PluginKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        PluginKey::parse(&raw).map_err(de::Error::custom)
    }
}

impl FromStr for PluginKey {
    type Err = CatalogError;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl PluginKey {
    /// Builds a key from already-canonical text. Prefer the typed constructors.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical form cannot be parsed.
    pub fn parse(raw: &str) -> Result<Self> {
        let parsed = parse_canonical(raw)?;
        Ok(Self {
            canonical: parsed.canonical,
        })
    }

    /// Platform-bundled key (`platform:bookclerk/{package}`).
    ///
    /// `manifest_id` is validated as a plugin alias but is not part of the key.
    ///
    /// # Errors
    ///
    /// Returns an error when `manifest_id` fails the plugin id grammar.
    pub fn platform(package_name: &str, manifest_id: &str) -> Result<Self> {
        validate_plugin_id(manifest_id).map_err(|e| CatalogError::message(e.to_string()))?;
        Self::from_parts(ProvenanceScheme::Platform, PLATFORM_PRODUCT, package_name)
    }

    /// Registry-sourced key (Cargo / npm / PyPI / static index).
    ///
    /// Version and the human alias are **not** part of the key. `manifest_id`
    /// is validated as a plugin alias only.
    ///
    /// # Errors
    ///
    /// Returns an error when the coordinate or id is invalid.
    pub fn from_coordinate(coordinate: &PackageCoordinate, manifest_id: &str) -> Result<Self> {
        match &coordinate.source {
            RegistrySource::Cargo { registry_url } => {
                validate_plugin_id(manifest_id)
                    .map_err(|e| CatalogError::message(e.to_string()))?;
                Self::from_parts(
                    ProvenanceScheme::Cargo,
                    &normalize_index_url(registry_url)?,
                    &coordinate.name,
                )
            }
            RegistrySource::Npm { registry_url } => {
                validate_plugin_id(manifest_id)
                    .map_err(|e| CatalogError::message(e.to_string()))?;
                Self::from_parts(
                    ProvenanceScheme::Npm,
                    &normalize_index_url(registry_url)?,
                    &coordinate.name,
                )
            }
            RegistrySource::Pypi { simple_url } => {
                validate_plugin_id(manifest_id)
                    .map_err(|e| CatalogError::message(e.to_string()))?;
                Self::from_parts(
                    ProvenanceScheme::Pypi,
                    &normalize_index_url(simple_url)?,
                    &coordinate.name,
                )
            }
            RegistrySource::Static { index_url } => {
                validate_plugin_id(manifest_id)
                    .map_err(|e| CatalogError::message(e.to_string()))?;
                Self::from_parts(
                    ProvenanceScheme::Registry,
                    &normalize_index_url(index_url)?,
                    &coordinate.name,
                )
            }
            RegistrySource::LocalArchive => {
                // Local archives still need a filesystem identity after extract.
                // Callers should use [`Self::from_install_path`] on the dest root.
                Err(CatalogError::message(
                    "local archive coordinates are not PluginKeys; use the installed path",
                ))
            }
        }
    }

    /// Filesystem / development-tree key for an install directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the path cannot be made into a `file://` URL or
    /// the id is invalid.
    pub fn from_install_path(root: &Path, manifest_id: &str) -> Result<Self> {
        validate_plugin_id(manifest_id).map_err(|e| CatalogError::message(e.to_string()))?;
        let file_url = path_to_file_url(root)?;
        Self::from_parts(ProvenanceScheme::Path, &file_url, "")
    }

    /// Formats one canonical key from already-normalized parts.
    fn from_parts(scheme: ProvenanceScheme, source: &str, package: &str) -> Result<Self> {
        if source.is_empty() && scheme != ProvenanceScheme::Path {
            return Err(CatalogError::message("plugin key source must not be empty"));
        }
        if source.contains('#') || package.contains('#') {
            return Err(CatalogError::message(
                "plugin key source and package must not contain '#'",
            ));
        }
        if package.contains('\n') || source.contains('\n') {
            return Err(CatalogError::message(
                "plugin key source and package must be single-line",
            ));
        }
        let canonical = match scheme {
            ProvenanceScheme::Registry => {
                if package.is_empty() {
                    return Err(CatalogError::message(
                        "registry plugin key needs a package name",
                    ));
                }
                format!("registry:{source}#{package}")
            }
            ProvenanceScheme::Path => {
                format!("path:{source}")
            }
            ProvenanceScheme::Platform => {
                if package.is_empty() {
                    return Err(CatalogError::message(
                        "platform plugin key needs a package name",
                    ));
                }
                format!("platform:{source}/{package}")
            }
            ProvenanceScheme::Cargo | ProvenanceScheme::Npm | ProvenanceScheme::Pypi => {
                if package.is_empty() {
                    return Err(CatalogError::message(
                        "registry plugin key needs a package name",
                    ));
                }
                format!("{}:{source}#{package}", scheme.as_str())
            }
        };
        Ok(Self { canonical })
    }

    /// Canonical textual form.
    #[must_use]
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// SHA-256 (hex) of the canonical UTF-8 bytes.
    #[must_use]
    pub fn digest_sha256(&self) -> String {
        hex::encode(Sha256::digest(self.canonical.as_bytes()))
    }

    /// Filesystem-safe directory leaf (`pk-` + 32 hex chars / 128 bits of SHA-256).
    #[must_use]
    pub fn fs_id(&self) -> String {
        format!("pk-{}", &self.digest_sha256()[..PLUGIN_KEY_FS_ID_HEX_CHARS])
    }

    /// Provenance scheme.
    ///
    /// # Panics
    ///
    /// Panics if the in-memory canonical string is not a valid PluginKey.
    /// That is a programming error: every [`PluginKey`] is produced by
    /// [`Self::parse`] or a typed constructor that goes through it.
    #[must_use]
    pub fn scheme(&self) -> ProvenanceScheme {
        parse_canonical(&self.canonical)
            .map(|p| p.scheme)
            .expect("PluginKey canonical text is always parseable")
    }

    /// Package coordinate / name when present (empty for `path:` keys).
    #[must_use]
    pub fn package(&self) -> String {
        parsed_package(&self.canonical).unwrap_or_default()
    }
}

impl fmt::Display for PluginKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical)
    }
}

/// Parsed canonical key used while validating [`PluginKey::parse`].
struct ParsedKey {
    /// Rebuilt canonical text.
    canonical: String,
    /// Scheme token.
    scheme: ProvenanceScheme,
}

/// Parses and rebuilds a canonical key so equivalent spellings collapse.
fn parse_canonical(raw: &str) -> Result<ParsedKey> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains('\n') {
        return Err(CatalogError::message(
            "plugin key must be a single non-empty line",
        ));
    }
    let (scheme_str, rest) = raw
        .split_once(':')
        .ok_or_else(|| CatalogError::message(format!("plugin key `{raw}` missing scheme")))?;
    let scheme = ProvenanceScheme::parse(scheme_str).ok_or_else(|| {
        CatalogError::message(format!("unknown plugin key scheme `{scheme_str}`"))
    })?;
    let rebuilt = match scheme {
        ProvenanceScheme::Path => {
            if rest.contains('#') {
                return Err(CatalogError::message(
                    "path plugin key must be path:file://… (alias is not part of the key)",
                ));
            }
            PluginKey::from_parts(scheme, rest, "")?
        }
        ProvenanceScheme::Platform => {
            if rest.contains('#') {
                return Err(CatalogError::message(
                    "platform plugin key must be platform:bookclerk/pkg (alias is not part of the key)",
                ));
            }
            let (product, package) = rest.split_once('/').ok_or_else(|| {
                CatalogError::message("platform plugin key must include product/package")
            })?;
            if product != PLATFORM_PRODUCT {
                return Err(CatalogError::message(format!(
                    "unsupported platform product `{product}`"
                )));
            }
            PluginKey::from_parts(scheme, product, package)?
        }
        ProvenanceScheme::Registry => {
            let (index, package) = split_index_package(rest, "registry:<index_url>#<package>")?;
            PluginKey::from_parts(scheme, index, package)?
        }
        ProvenanceScheme::Cargo | ProvenanceScheme::Npm | ProvenanceScheme::Pypi => {
            let (index, package) =
                split_index_package(rest, &format!("{}:<index_url>#<package>", scheme.as_str()))?;
            PluginKey::from_parts(scheme, &normalize_index_url(index)?, package)?
        }
    };
    Ok(ParsedKey {
        canonical: rebuilt.canonical,
        scheme,
    })
}

/// Package coordinate extracted from a canonical key (empty for `path:`).
fn parsed_package(canonical: &str) -> Option<String> {
    let (scheme, rest) = canonical.split_once(':')?;
    match scheme {
        "path" => Some(String::new()),
        "platform" => rest.split_once('/').map(|(_, p)| p.to_string()),
        "registry" | "cargo" | "npm" | "pypi" => rest.split_once('#').map(|(_, p)| p.to_string()),
        _ => None,
    }
}

/// Splits `rest` on the single `#` into `(index_url, package)`.
fn split_index_package<'a>(s: &'a str, expected: &str) -> Result<(&'a str, &'a str)> {
    let (index, package) = s
        .split_once('#')
        .ok_or_else(|| CatalogError::message(format!("plugin key must be {expected}")))?;
    if index.contains('#') || package.contains('#') || package.is_empty() {
        return Err(CatalogError::message(format!(
            "plugin key must be {expected} (alias is not part of the key)"
        )));
    }
    Ok((index, package))
}

/// Identity of one exact installed build: [`PluginKey`] + version + hashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIdentity {
    /// Logical plugin (stable across upgrades).
    pub plugin_key: PluginKey,
    /// Package / manifest version string.
    pub version: String,
    /// SHA-256 of the installed `plugin.toml` bytes.
    pub manifest_sha256: String,
    /// Deterministic Merkle-style root over the immutable installed payload.
    pub payload_root_sha256: String,
    /// SHA-256 of the downloaded archive, when the install came from a registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive_sha256: Option<String>,
}

/// How Bookclerk established (or lost) trust in an installed tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginProvenance {
    /// Host-staged Bookclerk platform artifact; content hashes match the receipt.
    PlatformBundled,
    /// Installed from a registry/archive; receipt hashes still match.
    VerifiedInstalled,
    /// Local/dev tree with no verified receipt (or path-only identity).
    LocalDevelopment,
    /// Receipt exists but the immutable payload no longer matches.
    Modified,
}

impl PluginProvenance {
    /// True when this provenance may receive installer platform default grants.
    #[must_use]
    pub fn grants_platform_defaults(self) -> bool {
        matches!(self, Self::PlatformBundled)
    }

    /// True when discovery verified the installed payload against the host ledger.
    ///
    /// [`Self::PlatformBundled`] and [`Self::VerifiedInstalled`] qualify.
    /// [`Self::LocalDevelopment`] (no receipt) and [`Self::Modified`] never do.
    #[must_use]
    pub fn is_verified_artifact(self) -> bool {
        matches!(self, Self::PlatformBundled | Self::VerifiedInstalled)
    }
}

impl fmt::Display for PluginProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::PlatformBundled => "platform_bundled",
            Self::VerifiedInstalled => "verified_installed",
            Self::LocalDevelopment => "local_development",
            Self::Modified => "modified",
        })
    }
}

/// Evaluated identity for one discovered install directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInstallIdentity {
    /// Stable logical identity (provenance + package; not the alias).
    pub plugin_key: PluginKey,
    /// Human-facing alias from `plugin.toml` `id`.
    pub alias: String,
    /// Exact installed build.
    pub artifact: ArtifactIdentity,
    /// Host-evaluated provenance (never plugin-authored).
    pub provenance: PluginProvenance,
}

impl PluginInstallIdentity {
    /// Display alias (`plugin.toml` id). Never a security principal.
    #[must_use]
    pub fn alias(&self) -> &str {
        &self.alias
    }
}

/// Normalizes a registry index URL (scheme + host + port + path).
///
/// Query and fragment are stripped. Default ports are omitted. Host is
/// lowercased. `.` path segments are collapsed; `..` is rejected. A redundant
/// trailing slash is dropped so `https://example/a/` and `https://example/a`
/// are the same principal. Origin-only URLs (`https://crates.io/`) collapse
/// to `https://crates.io`. `file://` indexes (local static catalogs) keep
/// their filesystem path.
fn normalize_index_url(raw: &str) -> Result<String> {
    let mut url = url::Url::parse(raw).or_else(|_| url::Url::parse(&format!("https://{raw}")))?;
    if url.scheme() != "https" && url.scheme() != "http" && url.scheme() != "file" {
        return Err(CatalogError::message(format!(
            "registry index `{raw}` must be http(s) or file"
        )));
    }
    url.set_fragment(None);
    url.set_query(None);
    let mut segments = Vec::new();
    for seg in url.path().split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(CatalogError::message(format!(
                "registry index `{raw}` must not contain `..` path segments"
            )));
        }
        if seg.contains('#') {
            return Err(CatalogError::message(
                "registry index path must not contain '#'",
            ));
        }
        segments.push(seg.to_string());
    }
    if url.scheme() == "file" {
        let path = if segments.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", segments.join("/"))
        };
        return Ok(format!("file://{path}"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| CatalogError::message(format!("registry index `{raw}` missing host")))?
        .to_ascii_lowercase();
    url.set_host(Some(&host))
        .map_err(|e| CatalogError::message(e.to_string()))?;
    if (url.port() == Some(443) && url.scheme() == "https")
        || (url.port() == Some(80) && url.scheme() == "http")
    {
        let _ = url.set_port(None);
    }
    let authority = match url.port() {
        None => format!("{}://{host}", url.scheme()),
        Some(p) => format!("{}://{host}:{p}", url.scheme()),
    };
    if segments.is_empty() {
        Ok(authority)
    } else {
        Ok(format!("{authority}/{}", segments.join("/")))
    }
}

/// `file://` URL for an install directory (forward slashes, no trailing slash).
fn path_to_file_url(root: &Path) -> Result<String> {
    let abs = if root.is_absolute() {
        root.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| CatalogError::message(e.to_string()))?
            .join(root)
    };
    for comp in abs.components() {
        if matches!(comp, Component::ParentDir) {
            return Err(CatalogError::message(format!(
                "refusing plugin path with `..`: {}",
                root.display()
            )));
        }
    }
    let mut url = url::Url::from_file_path(&abs).map_err(|()| {
        CatalogError::message(format!(
            "cannot convert plugin path {} to file URL",
            abs.display()
        ))
    })?;
    // Drop trailing slash on the path except for filesystem roots.
    if url.path().ends_with('/') && url.path() != "/" {
        let mut p = url.path().trim_end_matches('/').to_string();
        if p.is_empty() {
            p = "/".into();
        }
        url.set_path(&p);
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_keys_are_stable_and_distinct_from_path() {
        let sqlite = PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite").unwrap();
        assert_eq!(
            sqlite.canonical(),
            "platform:bookclerk/bookclerk-plugin-database-sqlite"
        );
        assert!(is_platform_plugin_key(&sqlite));

        let fake = PluginKey::from_install_path(Path::new("/tmp/evil-sqlite"), "sqlite").unwrap();
        assert_ne!(sqlite, fake);
        assert!(!is_platform_plugin_key(&fake));
        assert!(fake.canonical().starts_with("path:file://"));
        assert!(!fake.canonical().contains("#sqlite"));
    }

    #[test]
    fn cargo_and_npm_match_documented_forms() {
        let cargo = PackageCoordinate::parse("cargo:bookclerk-plugin-source-foo@1.2.3").unwrap();
        let key = PluginKey::from_coordinate(&cargo, "foo").unwrap();
        assert_eq!(
            key.canonical(),
            "cargo:https://crates.io#bookclerk-plugin-source-foo"
        );
        assert_eq!(key.package(), "bookclerk-plugin-source-foo");

        let npm = PackageCoordinate::parse("npm:@author/foo@9.0.0").unwrap();
        let key = PluginKey::from_coordinate(&npm, "foo").unwrap();
        assert_eq!(
            key.canonical(),
            "npm:https://registry.npmjs.org#@author/foo"
        );

        let pypi = PackageCoordinate::parse("pypi:foo==0.1.0").unwrap();
        let key = PluginKey::from_coordinate(&pypi, "foo").unwrap();
        assert_eq!(key.canonical(), "pypi:https://pypi.org#foo");

        let reg = PackageCoordinate::parse(
            "registry:https://plugins.example/index.json#author/foo@1.0.0",
        )
        .unwrap();
        let key = PluginKey::from_coordinate(&reg, "foo").unwrap();
        assert_eq!(
            key.canonical(),
            "registry:https://plugins.example/index.json#author/foo"
        );
    }

    #[test]
    fn same_origin_different_registry_paths_are_distinct_keys() {
        let team_a = PackageCoordinate {
            source: RegistrySource::Cargo {
                registry_url: "https://packages.example/team-a/".into(),
            },
            name: "widget".into(),
            version: "1.0.0".into(),
        };
        let team_b = PackageCoordinate {
            source: RegistrySource::Cargo {
                registry_url: "https://packages.example/team-b/".into(),
            },
            name: "widget".into(),
            version: "1.0.0".into(),
        };
        let a = PluginKey::from_coordinate(&team_a, "widget").unwrap();
        let b = PluginKey::from_coordinate(&team_b, "widget").unwrap();
        assert_eq!(
            a.canonical(),
            "cargo:https://packages.example/team-a#widget"
        );
        assert_eq!(
            b.canonical(),
            "cargo:https://packages.example/team-b#widget"
        );
        assert_ne!(a, b);
        let a_again = PackageCoordinate {
            source: RegistrySource::Cargo {
                registry_url: "https://packages.example/team-a/./".into(),
            },
            name: "widget".into(),
            version: "9.9.9".into(),
        };
        assert_eq!(
            PluginKey::from_coordinate(&a_again, "widget").unwrap(),
            a,
            "same registry path + different version keeps PluginKey"
        );
    }

    #[test]
    fn plugin_key_parse_rejects_noncanonical_and_serde_runs_parse() {
        assert!(PluginKey::parse("not-a-key").is_err());
        assert!(
            PluginKey::parse("cargo:https://crates.io#pkg#id").is_err(),
            "legacy alias-in-key form must fail closed"
        );
        assert!(PluginKey::parse("platform:bookclerk/pkg#id").is_err());
        let key = PluginKey::parse("cargo:https://crates.io#pkg").unwrap();
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"cargo:https://crates.io#pkg\"");
        let back: PluginKey = serde_json::from_str(&json).unwrap();
        assert_eq!(back, key);
        let err = serde_json::from_str::<PluginKey>("{\"canonical\":\"nope\"}").unwrap_err();
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn file_registry_indexes_round_trip() {
        let coord = PackageCoordinate::parse(
            "registry:file:///tmp/indexes/team-a/index.json#community/echo@1.0.0",
        )
        .unwrap();
        let key = PluginKey::from_coordinate(&coord, "echo").unwrap();
        assert_eq!(
            key.canonical(),
            "registry:file:///tmp/indexes/team-a/index.json#community/echo"
        );
        assert_eq!(PluginKey::parse(key.canonical()).unwrap(), key);
    }

    #[test]
    fn first_party_database_adapter_requires_exact_package_not_alias() {
        let sqlite = PluginKey::platform("bookclerk-plugin-database-sqlite", "sqlite").unwrap();
        assert!(is_first_party_database_adapter(&sqlite));
        let path = PluginKey::from_install_path(Path::new("/tmp/sqlite"), "sqlite").unwrap();
        assert!(!is_first_party_database_adapter(&path));
        let npm = PackageCoordinate::parse("npm:bookclerk-plugin-database-sqlite@1.0.0").unwrap();
        let npm_key = PluginKey::from_coordinate(&npm, "sqlite").unwrap();
        assert!(!is_first_party_database_adapter(&npm_key));
        let other_reg = PackageCoordinate {
            source: RegistrySource::Cargo {
                registry_url: "https://packages.example/evil/".into(),
            },
            name: "bookclerk-plugin-database-sqlite".into(),
            version: "1.0.0".into(),
        };
        let evil = PluginKey::from_coordinate(&other_reg, "sqlite").unwrap();
        assert!(!is_first_party_database_adapter(&evil));
    }

    #[test]
    fn first_party_destination_requires_exact_package_not_alias() {
        let local = PluginKey::platform("bookclerk-plugin-destination-local", "local").unwrap();
        assert!(is_first_party_destination(&local));
        let s3 = PluginKey::platform("bookclerk-plugin-destination-s3", "s3").unwrap();
        assert!(is_first_party_destination(&s3));
        let path = PluginKey::from_install_path(Path::new("/tmp/local"), "local").unwrap();
        assert!(!is_first_party_destination(&path));
        let npm = PackageCoordinate::parse("npm:bookclerk-plugin-destination-local@1.0.0").unwrap();
        let npm_key = PluginKey::from_coordinate(&npm, "local").unwrap();
        assert!(!is_first_party_destination(&npm_key));
    }

    #[test]
    fn version_is_not_part_of_plugin_key() {
        let a = PackageCoordinate::parse("cargo:bookclerk-plugin-source-foo@1.0.0").unwrap();
        let b = PackageCoordinate::parse("cargo:bookclerk-plugin-source-foo@9.9.9").unwrap();
        assert_eq!(
            PluginKey::from_coordinate(&a, "foo").unwrap(),
            PluginKey::from_coordinate(&b, "foo").unwrap()
        );
    }

    #[test]
    fn parse_round_trips() {
        let key = PluginKey::platform("bookclerk-plugin-destination-local", "local").unwrap();
        assert_eq!(PluginKey::parse(key.canonical()).unwrap(), key);
        assert_eq!(key.fs_id().len(), 3 + PLUGIN_KEY_FS_ID_HEX_CHARS); // pk- + 32 hex
        assert!(key.fs_id().starts_with("pk-"));
        assert_eq!(key.scheme(), ProvenanceScheme::Platform);
    }

    #[test]
    fn same_manifest_id_two_registries_are_distinct() {
        let cargo = PackageCoordinate::parse("cargo:sqlite-impersonator@1.0.0").unwrap();
        let npm = PackageCoordinate::parse("npm:sqlite-impersonator@1.0.0").unwrap();
        let a = PluginKey::from_coordinate(&cargo, "sqlite").unwrap();
        let b = PluginKey::from_coordinate(&npm, "sqlite").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn alias_is_not_part_of_plugin_key() {
        let coord = PackageCoordinate::parse("cargo:bookclerk-plugin-source-foo@1.0.0").unwrap();
        let echo = PluginKey::from_coordinate(&coord, "echo").unwrap();
        let renamed = PluginKey::from_coordinate(&coord, "echo2").unwrap();
        assert_eq!(echo, renamed);
        assert_eq!(
            echo.canonical(),
            "cargo:https://crates.io#bookclerk-plugin-source-foo"
        );
    }
}
