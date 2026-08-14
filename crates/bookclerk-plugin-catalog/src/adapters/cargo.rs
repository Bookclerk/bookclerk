//! crates.io adapter — search via API; hydrate via inert `.crate` download.

use std::io::Read;

use flate2::read::GzDecoder;
use serde::Deserialize;
use tar::Archive;

use crate::adapters::RegistryAdapter;
use crate::catalog::{CatalogHit, SearchQuery};
use crate::coordinate::PackageCoordinate;
use crate::error::{CatalogError, Result};
use crate::kind::PluginKind;
use crate::manifest::BookclerkPackageManifest;

/// crates.io User-Agent identifying this Bookclerk catalog client.
const UA: &str = concat!(
    "bookclerk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/fritz-fritz/bookclerk; plugin-catalog)"
);

/// Required crate-name prefix (`bookclerk-plugin-{kind}-{id}`).
const PREFIX: &str = "bookclerk-plugin-";

/// crates.io discovery + `.crate` metadata hydration.
pub struct CargoAdapter {
    /// Base URL of the package registry (for example crates.io or npm).
    pub registry_url: String,
}

impl Default for CargoAdapter {
    fn default() -> Self {
        Self {
            registry_url: "https://crates.io".into(),
        }
    }
}

impl RegistryAdapter for CargoAdapter {
    fn source_kind(&self) -> &'static str {
        "cargo"
    }

    fn search(&self, q: &SearchQuery) -> Result<Vec<CatalogHit>> {
        let per_page = q.limit.clamp(1, 100);
        let query = match q.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(extra) => format!("bookclerk-plugin {extra}"),
            None => "bookclerk-plugin".into(),
        };
        let url = format!(
            "{}/api/v1/crates?q={}&per_page={per_page}&sort=downloads",
            self.registry_url.trim_end_matches('/'),
            urlencoding_encode(&query)
        );
        let body: CratesSearchResponse = http_get_json(&url)?;
        let mut out = Vec::new();
        for c in body.crates {
            if !c.name.starts_with(PREFIX) {
                continue;
            }
            let Some((kind, id)) = parse_crate_name(&c.name) else {
                continue;
            };
            out.push(CatalogHit::from_cargo_search(
                c.name,
                c.max_version.unwrap_or_default(),
                Some(kind),
                Some(id),
                c.description,
                c.downloads.unwrap_or(0),
                c.repository,
                c.homepage,
                c.documentation,
            ));
        }
        Ok(out)
    }

    fn fetch_manifest(&self, coord: &PackageCoordinate) -> Result<BookclerkPackageManifest> {
        // Prefer not downloading every version for search — only exact install.
        // Use the registry download API so alternate Cargo registries work
        // (not only static.crates.io).
        let url = format!(
            "{}/api/v1/crates/{name}/{version}/download",
            self.registry_url.trim_end_matches('/'),
            name = coord.name,
            version = coord.version
        );
        let bytes = http_get_bytes(&url)?;
        let meta = parse_bookclerk_metadata_from_crate(&bytes)?;
        let mut manifest = meta;
        manifest.coordinate = Some(coord.clone());
        // Legacy template-only metadata cannot install without digests.
        if manifest.artifacts.is_empty() {
            return Err(CatalogError::message(
                "crates.io package has no digest-pinned artifacts[]; \
                 publish artifacts with archive_sha256 or use a static registry",
            ));
        }
        manifest.validate_for_install()?;
        Ok(manifest)
    }

    fn list_versions(&self, name: &str) -> Result<Vec<String>> {
        let url = format!(
            "{}/api/v1/crates/{name}",
            self.registry_url.trim_end_matches('/')
        );
        let body: CrateDetail = http_get_json(&url)?;
        Ok(body.versions.into_iter().map(|v| v.num).collect())
    }
}

/// Parse `[package.metadata.bookclerk]` from an inert `.crate` (tar.gz) archive.
pub fn parse_bookclerk_metadata_from_crate(bytes: &[u8]) -> Result<BookclerkPackageManifest> {
    let dec = GzDecoder::new(bytes);
    let mut archive = Archive::new(dec);
    for entry in archive
        .entries()
        .map_err(|e| CatalogError::message(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| CatalogError::message(e.to_string()))?;
        let path = entry
            .path()
            .map_err(|e| CatalogError::message(e.to_string()))?
            .into_owned();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name != "Cargo.toml" {
            continue;
        }
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|e| CatalogError::message(e.to_string()))?;
        return parse_metadata_bookclerk_toml(&text);
    }
    Err(CatalogError::message(
        ".crate archive did not contain Cargo.toml",
    ))
}

/// Reads `[package.metadata.bookclerk]` from a `Cargo.toml` and fills id from the package name.
fn parse_metadata_bookclerk_toml(text: &str) -> Result<BookclerkPackageManifest> {
    let value: toml::Value = toml::from_str(text)?;
    let meta = value
        .get("package")
        .and_then(|p| p.get("metadata"))
        .and_then(|m| m.get("bookclerk"))
        .ok_or_else(|| CatalogError::message("Cargo.toml missing [package.metadata.bookclerk]"))?;
    let mut manifest: BookclerkPackageManifest = meta
        .clone()
        .try_into()
        .map_err(|e| CatalogError::message(format!("invalid [package.metadata.bookclerk]: {e}")))?;
    // Fill kind/id from package name when missing artifacts-only tables use legacy fields.
    if let Some(name) = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        if let Some((kind, id)) = parse_crate_name(name) {
            if manifest.id.is_empty() {
                manifest.id = id;
            }
            let _ = kind;
        }
    }
    Ok(manifest)
}

/// Splits `bookclerk-plugin-{kind}-{id}` into a [`PluginKind`] and plugin id.
fn parse_crate_name(name: &str) -> Option<(PluginKind, String)> {
    let rest = name.strip_prefix(PREFIX)?;
    let (kind_str, id) = rest.split_once('-')?;
    let kind = PluginKind::parse(kind_str)?;
    Some((kind, id.to_string()))
}

/// GET `url` as JSON; non-success HTTP statuses become [`CatalogError`].
fn http_get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T> {
    let mut response = ureq::get(url)
        .header("User-Agent", UA)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| CatalogError::message(format!("crates.io request failed: {e}")))?;
    if !response.status().is_success() {
        return Err(CatalogError::message(format!(
            "crates.io HTTP {} for {url}",
            response.status()
        )));
    }
    response
        .body_mut()
        .read_json()
        .map_err(|e| CatalogError::message(format!("crates.io JSON decode failed: {e}")))
}

/// GET `url` as bytes, capped at 32 MiB, for an inert `.crate` download.
fn http_get_bytes(url: &str) -> Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .header("User-Agent", UA)
        .call()
        .map_err(|e| CatalogError::message(format!("download failed: {e}")))?;
    if !response.status().is_success() {
        return Err(CatalogError::message(format!(
            "HTTP {} for {url}",
            response.status()
        )));
    }
    let mut buf = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(32 * 1024 * 1024)
        .read_to_end(&mut buf)?;
    Ok(buf)
}

/// application/x-www-form-urlencoded encoding (spaces as `+`) for crates.io search queries.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
/// crates.io `/api/v1/crates` search envelope.
struct CratesSearchResponse {
    /// Search hits; names that lack the plugin prefix are dropped later.
    crates: Vec<CrateHit>,
}

#[derive(Debug, Deserialize)]
/// One crate from a crates.io search response.
struct CrateHit {
    /// crates.io package name (`bookclerk-plugin-…`).
    name: String,
    #[serde(default)]
    /// crates.io short description shown in catalog search.
    description: Option<String>,
    #[serde(default)]
    /// Lifetime download count used as a search ranking hint.
    downloads: Option<u64>,
    #[serde(default)]
    /// docs.rs or other documentation URL from the crate record.
    documentation: Option<String>,
    #[serde(default)]
    /// Source-control URL from the crate record.
    repository: Option<String>,
    #[serde(default)]
    /// Project homepage URL from the crate record.
    homepage: Option<String>,
    #[serde(default)]
    /// Highest published semver string; empty when crates.io omits it.
    max_version: Option<String>,
}

#[derive(Debug, Deserialize)]
/// crates.io crate-detail envelope used to list published versions.
struct CrateDetail {
    #[serde(default)]
    /// Published versions; `num` is the semver string.
    versions: Vec<VersionHit>,
}

#[derive(Debug, Deserialize)]
/// One published version on a crate-detail response.
struct VersionHit {
    /// Semver string as published on crates.io (`1.2.3`).
    num: String,
}
