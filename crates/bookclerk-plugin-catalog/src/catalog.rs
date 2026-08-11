//! Registry-neutral catalog search hits.

use serde::{Deserialize, Serialize};

use crate::coordinate::{PackageCoordinate, RegistrySource};
use crate::kind::{PluginKind, RuntimeIdentity};
use crate::manifest::BookclerkPackageManifest;

/// Schema version for CLI JSON catalog output.
pub const CATALOG_DTO_SCHEMA_VERSION: u32 = 1;

/// One discovery hit from any registry adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogHit {
    /// DTO schema version for CLI/UI JSON compatibility.
    pub schema_version: u32,
    /// Source-qualified coordinate when version is known; otherwise name-only hint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinate: Option<PackageCoordinate>,
    /// Registry adapter kind (`cargo`, `npm`, `pypi`, `static`, …).
    pub source_kind: String,
    /// Package name in the source registry.
    pub package_name: String,
    /// Resolved or candidate package version string.
    pub version: String,
    /// Short package description from the registry, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Download count from the registry index, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<u64>,
    /// Source repository URL from package metadata, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Project homepage URL from package metadata, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    /// Documentation URL from package metadata, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation: Option<String>,
    /// Bookclerk runtime identity (kind + plugin id) when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeIdentity>,
    /// Present after metadata hydration / static index lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manifest: Option<BookclerkPackageManifest>,
}

impl CatalogHit {
    /// Build a hit from a crates.io-shaped search result.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn from_cargo_search(
        crate_name: impl Into<String>,
        version: impl Into<String>,
        kind: Option<PluginKind>,
        id: Option<String>,
        description: Option<String>,
        downloads: u64,
        repository: Option<String>,
        homepage: Option<String>,
        documentation: Option<String>,
    ) -> Self {
        let package_name = crate_name.into();
        let version = version.into();
        let runtime = match (kind, id) {
            (Some(kind), Some(id)) => Some(RuntimeIdentity::new(kind, id)),
            _ => None,
        };
        let coordinate = Some(PackageCoordinate {
            source: RegistrySource::Cargo {
                registry_url: "https://crates.io".into(),
            },
            name: package_name.clone(),
            version: version.clone(),
        });
        Self {
            schema_version: CATALOG_DTO_SCHEMA_VERSION,
            coordinate,
            source_kind: "cargo".into(),
            package_name,
            version,
            description,
            downloads: Some(downloads),
            repository,
            homepage,
            documentation,
            runtime,
            manifest: None,
        }
    }
}

/// Search query passed to adapters.
#[derive(Debug, Clone, Default)]
pub struct SearchQuery {
    /// Free-text search string passed to registry adapters.
    pub text: Option<String>,
    /// Maximum hits to return (clamped by the caller).
    pub limit: u32,
}
