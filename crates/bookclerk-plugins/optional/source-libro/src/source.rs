//! [`LibroSource`]: catalog lookups against the public Libro.fm storefront.
//!
//! Account login, library scans and title fetches run in the guest RPC
//! handlers ([`crate::guest`]); the host stores the credentials they return.

use bookclerk_storefront::{
    CatalogHit, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, SourcePurchaseHint,
};

use crate::client::DEFAULT_BASE_URL;
use crate::container::LibroContainer;

/// Describe / config id for this store (`libro`).
pub const ID: &str = "libro";

/// Libro.fm catalog client used by the guest's discovery entrypoints.
///
/// Construct with [`LibroSource::new`]; use [`LibroSource::with_base_url`]
/// for wiremock / staging.
#[derive(Debug, Clone)]
pub struct LibroSource {
    /// Libro.fm API origin (production default, or a wiremock / staging URL).
    base_url: String,
    /// Preferred download container (`container` plugin setting).
    pub container: LibroContainer,
}

impl Default for LibroSource {
    fn default() -> Self {
        Self::new()
    }
}

impl LibroSource {
    /// Production Libro.fm API origin.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            container: LibroContainer::M4b,
        }
    }

    /// Override API base (wiremock / staging).
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            container: LibroContainer::M4b,
        }
    }

    /// Sets the preferred download container (`m4b` / track zip).
    #[must_use]
    pub fn with_container(mut self, container: LibroContainer) -> Self {
        self.container = container;
        self
    }

    /// Searches the public catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the storefront request or its parse fails.
    pub async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
        crate::catalog::search_catalog(&self.base_url, opts).await
    }

    /// Loads one catalog title by product id (ISBN); `None` when it is unknown.
    ///
    /// # Errors
    ///
    /// Returns an error when the storefront request or its parse fails.
    pub async fn catalog_detail(
        &self,
        product_id: &str,
    ) -> bookclerk_storefront::Result<Option<CatalogHit>> {
        crate::catalog::catalog_detail(&self.base_url, product_id).await
    }

    /// Finds up to `limit` titles related to `seed` (same author, series, …).
    ///
    /// # Errors
    ///
    /// Returns an error when the storefront request or its parse fails.
    pub async fn expand_candidates(
        &self,
        seed: &ExpandSeed,
        limit: usize,
    ) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
        crate::catalog::expand_candidates(&self.base_url, seed, limit).await
    }

    /// Resolves the storefront price and link for a title; `None` when not sold.
    ///
    /// # Errors
    ///
    /// Returns an error when the storefront request or its parse fails.
    pub async fn purchase_hint(
        &self,
        opts: &PurchaseHintOpts,
    ) -> bookclerk_storefront::Result<Option<SourcePurchaseHint>> {
        crate::catalog::purchase_hint(&self.base_url, opts).await
    }
}
