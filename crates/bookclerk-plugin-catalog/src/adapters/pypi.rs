//! PyPI adapter — exact lookup only (no supported search API).

use serde::Deserialize;
use serde_json::Value;

use crate::adapters::RegistryAdapter;
use crate::catalog::{CatalogHit, SearchQuery, CATALOG_DTO_SCHEMA_VERSION};
use crate::coordinate::{PackageCoordinate, RegistrySource};
use crate::error::{CatalogError, Result};
use crate::manifest::BookclerkPackageManifest;

const UA: &str = concat!(
    "bookclerk/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/fritz-fritz/bookclerk; plugin-catalog)"
);

/// PyPI JSON API adapter. Search is not supported by PyPI; use static registries
/// for discovery and exact `pypi:name==version` coordinates for install.
pub struct PypiAdapter {
    /// PyPI simple/API base URL for this adapter.
    pub base_url: String,
}

impl Default for PypiAdapter {
    fn default() -> Self {
        Self {
            base_url: "https://pypi.org".into(),
        }
    }
}

impl RegistryAdapter for PypiAdapter {
    fn source_kind(&self) -> &'static str {
        "pypi"
    }

    fn search(&self, q: &SearchQuery) -> Result<Vec<CatalogHit>> {
        // Honest limitation: PyPI has no supported general-purpose search API.
        // If the query looks like an exact project name, perform a lookup.
        let Some(name) = q.text.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Err(CatalogError::message(
                "PyPI has no search API; use an exact project name, a static registry, \
                 or `pypi:name==version` for install",
            ));
        };
        if name.contains(' ') {
            return Err(CatalogError::message(
                "PyPI adapter only supports exact project-name lookup (no full-text search)",
            ));
        }
        let url = format!("{}/pypi/{name}/json", self.base_url.trim_end_matches('/'));
        let body: Value = http_get_json(&url)?;
        let version = body
            .pointer("/info/version")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let summary = body
            .pointer("/info/summary")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        Ok(vec![CatalogHit {
            schema_version: CATALOG_DTO_SCHEMA_VERSION,
            coordinate: Some(PackageCoordinate {
                source: RegistrySource::Pypi {
                    simple_url: self.base_url.clone(),
                },
                name: name.to_string(),
                version: version.clone(),
            }),
            source_kind: "pypi".into(),
            package_name: name.to_string(),
            version,
            description: summary,
            downloads: None,
            repository: None,
            homepage: body
                .pointer("/info/home_page")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            documentation: None,
            runtime: None,
            manifest: None,
        }])
    }

    fn fetch_manifest(&self, coord: &PackageCoordinate) -> Result<BookclerkPackageManifest> {
        // Exact-version endpoint so project_urls / metadata match the pin,
        // not whatever is currently latest on /pypi/{name}/json.
        let url = format!(
            "{}/pypi/{}/{}/json",
            self.base_url.trim_end_matches('/'),
            coord.name,
            coord.version
        );
        let body: Value = http_get_json(&url)?;
        // Prefer `[tool.bookclerk]` mirrored into project URLs or info description JSON —
        // for v1 we look for `info.project_urls["Bookclerk-Manifest"]` pointing at JSON,
        // or a `bookclerk` key under `releases[version][0].digests` companion.
        // Primary path: project_urls Bookclerk-Manifest HTTPS JSON.
        if let Some(manifest_url) = body
            .pointer("/info/project_urls/Bookclerk-Manifest")
            .and_then(|v| v.as_str())
            .or_else(|| {
                body.pointer("/info/project_urls/bookclerk-manifest")
                    .and_then(|v| v.as_str())
            })
        {
            let mut manifest: BookclerkPackageManifest = http_get_json(manifest_url)?;
            // Ensure version matches.
            if let Some(c) = manifest.coordinate.as_ref() {
                if c.version != coord.version {
                    tracing::debug!("manifest coordinate version differs; using request version");
                }
            }
            manifest.coordinate = Some(coord.clone());
            manifest.validate_for_install()?;
            return Ok(manifest);
        }
        Err(CatalogError::message(
            "PyPI project has no Bookclerk-Manifest project URL; \
             publish a digest-complete static registry entry or project_urls[\"Bookclerk-Manifest\"]",
        ))
    }

    fn list_versions(&self, name: &str) -> Result<Vec<String>> {
        let url = format!("{}/pypi/{name}/json", self.base_url.trim_end_matches('/'));
        let body: PypiJson = http_get_json(&url)?;
        Ok(body.releases.into_keys().collect())
    }
}

fn http_get_json<T: for<'de> Deserialize<'de>>(url: &str) -> Result<T> {
    let mut response = ureq::get(url)
        .header("User-Agent", UA)
        .header("Accept", "application/json")
        .call()
        .map_err(|e| CatalogError::message(format!("PyPI request failed: {e}")))?;
    if !response.status().is_success() {
        return Err(CatalogError::message(format!(
            "PyPI HTTP {} for {url}",
            response.status()
        )));
    }
    response
        .body_mut()
        .read_json()
        .map_err(|e| CatalogError::message(e.to_string()))
}

#[derive(Debug, Deserialize)]
struct PypiJson {
    #[serde(default)]
    releases: std::collections::BTreeMap<String, Value>,
}
