//! npm registry adapter — search + packument `bookclerk` field.

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::RegistryAdapter;
use crate::catalog::{CatalogHit, SearchQuery, CATALOG_DTO_SCHEMA_VERSION};
use crate::coordinate::{PackageCoordinate, RegistrySource};
use crate::error::{CatalogError, Result};
use crate::kind::RuntimeIdentity;
use crate::manifest::BookclerkPackageManifest;

/// User-Agent sent on registry search and packument requests.
const UA: &str = concat!(
    "bookclerk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/fritz-fritz/bookclerk; plugin-catalog)"
);

/// npm registry adapter.
pub struct NpmAdapter {
    /// Base URL of the package registry (for example crates.io or npm).
    pub registry_url: String,
}

impl Default for NpmAdapter {
    fn default() -> Self {
        Self {
            registry_url: "https://registry.npmjs.org".into(),
        }
    }
}

impl RegistryAdapter for NpmAdapter {
    fn source_kind(&self) -> &'static str {
        "npm"
    }

    fn search(&self, q: &SearchQuery) -> Result<Vec<CatalogHit>> {
        let size = q.limit.clamp(1, 250);
        let text = match q.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(extra) => format!("keywords:bookclerk-plugin {extra}"),
            None => "keywords:bookclerk-plugin".into(),
        };
        let url = format!(
            "{}/-/v1/search?text={}&size={size}",
            self.registry_url.trim_end_matches('/'),
            urlencoding_encode(&text)
        );
        let body: NpmSearchResponse = http_get_json(&url)?;
        let mut hits = Vec::new();
        for obj in body.objects {
            let p = obj.package;
            hits.push(CatalogHit {
                schema_version: CATALOG_DTO_SCHEMA_VERSION,
                coordinate: Some(PackageCoordinate {
                    source: RegistrySource::Npm {
                        registry_url: self.registry_url.clone(),
                    },
                    name: p.name.clone(),
                    version: p.version.clone(),
                }),
                source_kind: "npm".into(),
                package_name: p.name,
                version: p.version,
                description: p.description,
                downloads: None,
                repository: None,
                homepage: p.links.and_then(|l| l.homepage),
                documentation: None,
                runtime: None,
                manifest: None,
            });
        }
        Ok(hits)
    }

    fn fetch_manifest(&self, coord: &PackageCoordinate) -> Result<BookclerkPackageManifest> {
        let url = format!("{}/{}", self.registry_url.trim_end_matches('/'), coord.name);
        let packument: Value = http_get_json(&url)?;
        let version = packument
            .pointer(&format!("/versions/{}", coord.version))
            .ok_or_else(|| {
                CatalogError::message(format!(
                    "npm package `{}` has no version {}",
                    coord.name, coord.version
                ))
            })?;
        let bookclerk = version.get("bookclerk").ok_or_else(|| {
            CatalogError::message("npm package.json missing top-level `bookclerk` manifest field")
        })?;
        let mut manifest: BookclerkPackageManifest = serde_json::from_value(bookclerk.clone())?;
        manifest.coordinate = Some(coord.clone());
        // Optional: surface registry tarball integrity as advisory only.
        if let Some(integrity) = version.pointer("/dist/integrity").and_then(|v| v.as_str()) {
            tracing::debug!(%integrity, "npm dist.integrity present for packument tarball");
        }
        manifest.validate_for_install()?;
        if let Some(runtime) = Some(manifest.runtime()) {
            let _ = RuntimeIdentity::new(runtime.kind, runtime.id);
        }
        Ok(manifest)
    }

    fn list_versions(&self, name: &str) -> Result<Vec<String>> {
        let url = format!("{}/{name}", self.registry_url.trim_end_matches('/'));
        let packument: Value = http_get_json(&url)?;
        let versions = packument
            .get("versions")
            .and_then(|v| v.as_object())
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        Ok(versions)
    }
}

/// GETs `url` as JSON with the catalog User-Agent; maps non-success HTTP into `CatalogError`.
fn http_get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T> {
    let mut response = ureq::get(url)
        .header("User-Agent", UA)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| CatalogError::message(format!("npm request failed: {e}")))?;
    if !response.status().is_success() {
        return Err(CatalogError::message(format!(
            "npm HTTP {} for {url}",
            response.status()
        )));
    }
    response
        .body_mut()
        .read_json()
        .map_err(|e| CatalogError::message(e.to_string()))
}

/// Percent-encodes a search `text` query using `+` for spaces (npm `/-/v1/search`).
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[derive(Debug, Deserialize)]
/// npm `/-/v1/search` envelope.
struct NpmSearchResponse {
    #[serde(default)]
    /// Hit wrappers from the registry search response.
    objects: Vec<NpmSearchObject>,
}

#[derive(Debug, Deserialize)]
/// One npm search hit wrapper.
struct NpmSearchObject {
    /// Package identity and links for this search hit.
    package: NpmPackage,
}

#[derive(Debug, Deserialize)]
/// Subset of an npm search package object used to build a catalog hit.
struct NpmPackage {
    /// Scoped or unscoped package name on the registry.
    name: String,
    /// Latest version string returned by search.
    version: String,
    #[serde(default)]
    /// Optional package description from the registry.
    description: Option<String>,
    #[serde(default)]
    /// Optional homepage and related links.
    links: Option<NpmLinks>,
}

#[derive(Debug, Deserialize)]
/// Link bag from an npm search package object.
struct NpmLinks {
    #[serde(default)]
    /// Package homepage URL when the registry provides one.
    homepage: Option<String>,
}
