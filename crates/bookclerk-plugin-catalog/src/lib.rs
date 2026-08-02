//! Registry-neutral plugin catalog, manifests, and secure installer.
//!
//! See `docs/plugin-registry.md`.

mod adapters;
mod catalog;
mod coordinate;
mod error;
mod extract;
mod install;
mod kind;
mod manifest;
mod receipt;
mod target;
mod trust;
mod version;

pub use adapters::{
    load_static_index, CargoAdapter, NpmAdapter, PypiAdapter, RegistryAdapter, StaticAdapter,
    StaticIndex, StaticPackage,
};
pub use catalog::{CatalogHit, SearchQuery, CATALOG_DTO_SCHEMA_VERSION};
pub use coordinate::{PackageCoordinate, RegistrySource};
pub use error::{CatalogError, Result};
pub use extract::{
    extract_archive, safe_join, sha256_bytes, sha256_file, MAX_ARCHIVE_BYTES, MAX_EXTRACTED_BYTES,
};
pub use install::{
    InstallOptions, InstallOutcome, Installer, DOWNLOAD_TIMEOUT, MAX_DOWNLOAD_BYTES,
};
pub use kind::{PluginKind, RuntimeIdentity};
pub use manifest::{
    parse_sha256_hex, validate_sha256_hex, ArtifactTarget, BookclerkPackageManifest, PackageLinks,
    PublisherIdentity, SandboxRequest, MANIFEST_SCHEMA_VERSION, PROTOCOL_JSONRPC_STDIO_V1,
};
pub use receipt::{InstallReceipt, RECEIPT_FILE};
pub use target::{
    host_bookclerk_target, normalize_target, rust_triple, select_target, ArchiveFormat, TARGETS,
};
pub use trust::TrustPolicy;
pub use version::{max_version, newest_newer_than, Version};

/// Federated search across configured adapters (static first, then cargo/npm).
pub fn federated_search(
    adapters: &[&dyn RegistryAdapter],
    query: &SearchQuery,
) -> Result<Vec<CatalogHit>> {
    let mut all = Vec::new();
    let mut errors = Vec::new();
    for adapter in adapters {
        match adapter.search(query) {
            Ok(mut hits) => all.append(&mut hits),
            Err(err) => errors.push(format!("{}: {err}", adapter.source_kind())),
        }
    }
    if all.is_empty() && !errors.is_empty() {
        return Err(CatalogError::message(format!(
            "all registry adapters failed: {}",
            errors.join("; ")
        )));
    }
    // Cap total.
    let limit = query.limit.clamp(1, 100) as usize;
    all.truncate(limit);
    Ok(all)
}

fn adapter_for_coordinate(
    coord: &PackageCoordinate,
    static_indexes: &[(String, StaticIndex)],
) -> Result<Box<dyn RegistryAdapter>> {
    Ok(match &coord.source {
        RegistrySource::Static { index_url } => {
            if let Some((_, index)) = static_indexes.iter().find(|(u, _)| u == index_url) {
                Box::new(StaticAdapter::from_index(index_url.clone(), index.clone()))
            } else {
                Box::new(StaticAdapter::open(index_url.clone())?)
            }
        }
        RegistrySource::Cargo { registry_url } => Box::new(CargoAdapter {
            registry_url: registry_url.clone(),
        }),
        RegistrySource::Npm { registry_url } => Box::new(NpmAdapter {
            registry_url: registry_url.clone(),
        }),
        RegistrySource::Pypi { simple_url } => Box::new(PypiAdapter {
            base_url: simple_url.clone(),
        }),
        RegistrySource::LocalArchive => {
            return Err(CatalogError::message(
                "local: coordinates require install --archive with an explicit manifest",
            ));
        }
    })
}

/// Resolve a coordinate to a manifest using the matching adapter.
pub fn fetch_manifest_for_coordinate(
    coord: &PackageCoordinate,
    static_indexes: &[(String, StaticIndex)],
) -> Result<BookclerkPackageManifest> {
    adapter_for_coordinate(coord, static_indexes)?.fetch_manifest(coord)
}

/// List published versions for a coordinate's package name.
pub fn list_versions_for_coordinate(
    coord: &PackageCoordinate,
    static_indexes: &[(String, StaticIndex)],
) -> Result<Vec<String>> {
    adapter_for_coordinate(coord, static_indexes)?.list_versions(&coord.name)
}

/// Resolve the newest version newer than `coord.version`, if any.
pub fn resolve_newer_version(
    coord: &PackageCoordinate,
    static_indexes: &[(String, StaticIndex)],
) -> Result<Option<String>> {
    let versions = list_versions_for_coordinate(coord, static_indexes)?;
    Ok(newest_newer_than(&coord.version, versions.iter().map(String::as_str)).map(str::to_string))
}
