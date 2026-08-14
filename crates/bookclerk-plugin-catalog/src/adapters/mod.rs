//! Federated registry adapters.

mod cargo;
mod npm;
mod pypi;
mod static_reg;

pub use cargo::CargoAdapter;
pub use npm::NpmAdapter;
pub use pypi::PypiAdapter;
pub use static_reg::{load_static_index, StaticAdapter, StaticIndex, StaticPackage};

use crate::catalog::{CatalogHit, SearchQuery};
use crate::coordinate::PackageCoordinate;
use crate::error::Result;
use crate::manifest::BookclerkPackageManifest;

/// Registry adapter trait (search / hydrate / versions).
pub trait RegistryAdapter: Send + Sync {
    /// Returns the adapter source kind string (`cargo`, `npm`, `pypi`, `static`).
    fn source_kind(&self) -> &'static str;
    /// Searches this registry for packages matching `query`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    fn search(&self, q: &SearchQuery) -> Result<Vec<CatalogHit>>;
    /// Fetches and parses the Bookclerk package manifest for `coordinate`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    fn fetch_manifest(&self, coord: &PackageCoordinate) -> Result<BookclerkPackageManifest>;
    /// Lists known versions for `name` in this registry.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    fn list_versions(&self, name: &str) -> Result<Vec<String>>;
}
