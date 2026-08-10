//! Static HTTPS / local JSON registry.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::adapters::RegistryAdapter;
use crate::catalog::{CatalogHit, SearchQuery, CATALOG_DTO_SCHEMA_VERSION};
use crate::coordinate::{PackageCoordinate, RegistrySource};
use crate::error::{CatalogError, Result};
use crate::kind::RuntimeIdentity;
use crate::manifest::BookclerkPackageManifest;

/// Top-level static index document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticIndex {
    pub schema_version: u32,
    #[serde(default)]
    pub packages: Vec<StaticPackage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaticPackage {
    pub name: String,
    #[serde(default)]
    pub versions: std::collections::BTreeMap<String, BookclerkPackageManifest>,
}

/// Load a static index from a file path or `file://` URL.
pub fn load_static_index(path_or_url: &str) -> Result<StaticIndex> {
    let path = path_or_url.strip_prefix("file://").unwrap_or(path_or_url);
    if Path::new(path).exists() {
        let text = fs::read_to_string(path)?;
        return Ok(serde_json::from_str(&text)?);
    }
    if path_or_url.starts_with("https://")
        || path_or_url.starts_with("http://127.0.0.1")
        || path_or_url.starts_with("http://localhost")
    {
        let mut response = ureq::get(path_or_url)
            .header(
                "User-Agent",
                concat!("bookclerk/", env!("CARGO_PKG_VERSION"), " (plugin-catalog)"),
            )
            .call()
            .map_err(|e| CatalogError::message(format!("static index fetch failed: {e}")))?;
        if !response.status().is_success() {
            return Err(CatalogError::message(format!(
                "static index HTTP {}",
                response.status()
            )));
        }
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| CatalogError::message(e.to_string()))?;
        return Ok(serde_json::from_str(&text)?);
    }
    Err(CatalogError::message(format!(
        "cannot load static index from `{path_or_url}`"
    )))
}

/// Adapter over an in-memory or remote static index.
pub struct StaticAdapter {
    pub index_url: String,
    index: StaticIndex,
}

impl StaticAdapter {
    pub fn open(index_url: impl Into<String>) -> Result<Self> {
        let index_url = index_url.into();
        let index = load_static_index(&index_url)?;
        Ok(Self { index_url, index })
    }

    #[must_use]
    pub fn from_index(index_url: impl Into<String>, index: StaticIndex) -> Self {
        Self {
            index_url: index_url.into(),
            index,
        }
    }
}

impl RegistryAdapter for StaticAdapter {
    fn source_kind(&self) -> &'static str {
        "registry"
    }

    fn search(&self, q: &SearchQuery) -> Result<Vec<CatalogHit>> {
        let limit = q.limit.clamp(1, 100) as usize;
        let needle = q
            .text
            .as_deref()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let mut hits = Vec::new();
        for pkg in &self.index.packages {
            if !needle.is_empty() && !pkg.name.to_ascii_lowercase().contains(&needle) {
                continue;
            }
            if let Some(ver) = crate::version::max_version(pkg.versions.keys().map(String::as_str))
            {
                let manifest = pkg.versions.get(ver).expect("max_version key present");
                hits.push(CatalogHit {
                    schema_version: CATALOG_DTO_SCHEMA_VERSION,
                    coordinate: Some(PackageCoordinate {
                        source: RegistrySource::Static {
                            index_url: self.index_url.clone(),
                        },
                        name: pkg.name.clone(),
                        version: ver.to_string(),
                    }),
                    source_kind: "registry".into(),
                    package_name: pkg.name.clone(),
                    version: ver.to_string(),
                    description: manifest.description.clone(),
                    downloads: None,
                    repository: manifest.links.repository.clone(),
                    homepage: manifest.links.homepage.clone(),
                    documentation: manifest.links.documentation.clone(),
                    runtime: Some(RuntimeIdentity::new(manifest.kind, manifest.id.clone())),
                    manifest: Some(manifest.clone()),
                });
            }
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
    }

    fn fetch_manifest(&self, coord: &PackageCoordinate) -> Result<BookclerkPackageManifest> {
        let pkg = self
            .index
            .packages
            .iter()
            .find(|p| p.name == coord.name)
            .ok_or_else(|| {
                CatalogError::message(format!("package `{}` not in index", coord.name))
            })?;
        let mut manifest = pkg.versions.get(&coord.version).cloned().ok_or_else(|| {
            CatalogError::message(format!(
                "version `{}` of `{}` not in index",
                coord.version, coord.name
            ))
        })?;
        manifest.coordinate = Some(coord.clone());
        manifest.validate_for_install()?;
        Ok(manifest)
    }

    fn list_versions(&self, name: &str) -> Result<Vec<String>> {
        let pkg = self
            .index
            .packages
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| CatalogError::message(format!("package `{name}` not in index")))?;
        Ok(pkg.versions.keys().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kind::PluginKind;
    use crate::manifest::{ArtifactTarget, PROTOCOL_WORKERS_RPC};

    fn sample_manifest(id: &str) -> BookclerkPackageManifest {
        BookclerkPackageManifest {
            schema_version: 1,
            protocol: Some(PROTOCOL_WORKERS_RPC.into()),
            api_version: 1,
            api_version_max: None,
            min_bookclerk: None,
            kind: PluginKind::Integration,
            id: id.into(),
            display_name: Some("Echo".into()),
            description: Some("echo plugin".into()),
            coordinate: None,
            artifacts: vec![ArtifactTarget {
                target: "linux-x64-gnu".into(),
                url: "file:///tmp/echo.tar.gz".into(),
                archive_sha256: "aa".repeat(32),
                archive_root: ".".into(),
                executable: "echo".into(),
                executable_sha256: None,
            }],
            sandbox: Default::default(),
            links: Default::default(),
            yanked: false,
            released_at: None,
            publisher: None,
        }
    }

    #[test]
    fn static_search_and_fetch() {
        let mut versions = std::collections::BTreeMap::new();
        versions.insert("1.0.0".into(), sample_manifest("echo"));
        let index = StaticIndex {
            schema_version: 1,
            packages: vec![StaticPackage {
                name: "community/echo".into(),
                versions,
            }],
        };
        let adapter = StaticAdapter::from_index("file:///tmp/index.json", index);
        let hits = adapter
            .search(&SearchQuery {
                text: Some("echo".into()),
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits.len(), 1);
        let coord =
            PackageCoordinate::parse("registry:file:///tmp/index.json#community/echo@1.0.0")
                .unwrap();
        // fetch uses in-memory index
        let m = adapter.fetch_manifest(&coord).unwrap();
        assert_eq!(m.id, "echo");
    }

    #[test]
    fn static_search_picks_semver_max_not_lexical() {
        let mut versions = std::collections::BTreeMap::new();
        versions.insert("2.0.0".into(), sample_manifest("echo"));
        versions.insert("10.0.0".into(), sample_manifest("echo"));
        let index = StaticIndex {
            schema_version: 1,
            packages: vec![StaticPackage {
                name: "community/echo".into(),
                versions,
            }],
        };
        let adapter = StaticAdapter::from_index("file:///tmp/index.json", index);
        let hits = adapter
            .search(&SearchQuery {
                text: Some("echo".into()),
                limit: 10,
            })
            .unwrap();
        assert_eq!(hits[0].version, "10.0.0");
    }
}
