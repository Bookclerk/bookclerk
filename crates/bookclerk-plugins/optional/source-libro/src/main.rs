//! External Libro.fm source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::{manifest_capabilities, manifest_media};
use bookclerk_plugin_sdk::{
    purchase_hint_results_one_by_one, serve, Bindings, Brand, CatalogCapabilities,
    CatalogDetailParams, CatalogHit, ContentSource as ContentSourceRole, DatabaseBinding,
    Entrypoints, ExpandCandidatesParams, FetchTitleParams, HealthOk, Invocation, ListDealsParams,
    LoginParams, LoginResult, PlainFetch, PluginDescribe, PluginError, PluginMigration,
    PluginWorker, PortalAuthMode, PurchaseHint, PurchaseHintParams, PurchaseHintResult,
    PurchaseHintsParams, ScalarLimits, ScanPage, ScanParams, SearchCatalogParams,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_scan_delta::SCAN_STATE_BINDING;
use bookclerk_storefront::abi::expand_seed_from_params;
use bookclerk_storefront::{CatalogSearchOpts, PurchaseHintOpts};
use serde_json::Value;

/// External Libro.fm source guest; `describe` advertises scan/fetch/catalog capabilities.
struct LibroRoot;

#[async_trait(?Send)]
impl PluginWorker for LibroRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "libro".into(),
            display_name: Some("Libro.fm".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            media: manifest_media(include_str!("../plugin.toml"))?,
            portal_auth_mode: PortalAuthMode::Password,
            aliases: vec!["libro.fm".into(), "librofm".into()],
            sort_key: 1,
            brand: Some(Brand {
                id: "libro".into(),
                name: "Libro.fm".into(),
                bg: "#1F4E3D".into(),
                fg: "#F4F1EA".into(),
                accent: "#2F6B53".into(),
                icon_url: Some("https://www.google.com/s2/favicons?domain=libro.fm&sz=128".into()),
            }),
            // Libro.fm's catalog details answer any audiobook ISBN, so catalog
            // search uses it to fill lean rows from every store (its own
            // product ids are ISBNs too), and a price lookup by ISBN answers
            // exactly that edition.
            catalog: CatalogCapabilities {
                detail_by_isbn: true,
                price_by_isbn: true,
                price_by_asin: false,
                detail_by_product_id: true,
                quality_offers: false,
            },
            ..PluginDescribe::default()
        })
    }

    async fn open(
        &self,
        _invocation: Invocation,
        mut bindings: Bindings,
    ) -> Result<Entrypoints, PluginError> {
        let config = bindings.config.json_value().unwrap_or(Value::Null);
        let scan_state =
            DatabaseBinding::take_named_from_bindings(&mut bindings, SCAN_STATE_BINDING);
        Ok(Entrypoints {
            storefront: Some(Box::new(LibroContentSource { config, scan_state })),
            ..Entrypoints::default()
        })
    }

    async fn database_migrations(
        &self,
        binding: &str,
    ) -> Result<Vec<PluginMigration>, PluginError> {
        Ok(bookclerk_scan_delta::migrations_for(binding))
    }
}

struct LibroContentSource {
    /// The instance settings document from the `CONFIG` binding; its origin
    /// override reaches sign-in, scans and every catalog entrypoint.
    config: Value,
    /// The `SCAN_STATE` database, once the operator approved it: scan delta
    /// state.
    scan_state: Option<DatabaseBinding>,
}

impl LibroContentSource {
    /// Catalog client for this instance's configured origin.
    fn catalog(&self) -> bookclerk_plugin_source_libro::LibroSource {
        bookclerk_plugin_source_libro::LibroSource::from_config(&self.config)
    }
}

fn internal(err: impl std::fmt::Display) -> PluginError {
    PluginError::internal(err.to_string())
}

fn hits(hits: Vec<bookclerk_storefront::CatalogHit>) -> Vec<CatalogHit> {
    hits.into_iter().map(Into::into).collect()
}

#[async_trait(?Send)]
impl ContentSourceRole for LibroContentSource {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "libro source plugin ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<Vec<String>, PluginError> {
        Ok(vec!["libro plugin diagnose: ok".into()])
    }

    async fn login(&self, params: LoginParams) -> Result<LoginResult, PluginError> {
        let base = bookclerk_plugin_source_libro::resolve_base_url(&self.config);
        bookclerk_plugin_source_libro::guest_login_rpc(&base, params)
            .await
            .map_err(internal)
    }

    async fn scan(&self, params: ScanParams) -> Result<ScanPage, PluginError> {
        let base = bookclerk_plugin_source_libro::resolve_base_url(&self.config);
        bookclerk_plugin_source_libro::guest_scan_page(&base, &params, self.scan_state.as_ref())
            .await
    }

    async fn fetch_title(&self, params: FetchTitleParams) -> Result<PlainFetch, PluginError> {
        let source_config = params.source_config.json_value().unwrap_or(Value::Null);
        let base = bookclerk_plugin_source_libro::resolve_base_url(&source_config);
        let container = bookclerk_plugin_source_libro::resolve_container(&source_config);
        bookclerk_plugin_source_libro::guest_fetch_title_rpc(&base, &params, container)
            .await
            .map_err(internal)
    }

    async fn search_catalog(
        &self,
        params: SearchCatalogParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        let source = self.catalog();
        source
            .search_catalog(&CatalogSearchOpts::from(params))
            .await
            .map(hits)
            .map_err(internal)
    }

    async fn catalog_detail(
        &self,
        params: CatalogDetailParams,
    ) -> Result<Option<CatalogHit>, PluginError> {
        let key = params
            .isbn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(params.product_id.trim());
        let source = self.catalog();
        source
            .catalog_detail(key)
            .await
            .map(|hit| hit.map(Into::into))
            .map_err(internal)
    }

    async fn expand_candidates(
        &self,
        params: ExpandCandidatesParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        let (seed, limit) = expand_seed_from_params(params);
        let source = self.catalog();
        source
            .expand_candidates(&seed, limit)
            .await
            .map(hits)
            .map_err(internal)
    }

    async fn purchase_hint(
        &self,
        params: PurchaseHintParams,
    ) -> Result<Option<PurchaseHint>, PluginError> {
        let source = self.catalog();
        source
            .purchase_hint(&PurchaseHintOpts::from(params))
            .await
            .map(|hint| hint.map(Into::into))
            .map_err(PluginError::from)
    }

    /// An exact ISBN lookup answers that edition or nothing; a title search
    /// answers every for-sale listing it found as a priced record.
    async fn purchase_hint_result(
        &self,
        params: PurchaseHintParams,
    ) -> Result<PurchaseHintResult, PluginError> {
        let source = self.catalog();
        source
            .purchase_answer(&PurchaseHintOpts::from(params))
            .await
            .map(PurchaseHintResult::from)
            .map_err(PluginError::from)
    }

    /// Libro.fm has no bulk price endpoint: each query is answered as
    /// [`Self::purchase_hint_result`] does, a few at a time.
    async fn purchase_hints_results(
        &self,
        params: PurchaseHintsParams,
    ) -> Result<Vec<PurchaseHintResult>, PluginError> {
        purchase_hint_results_one_by_one(self, params).await
    }

    /// Libro.fm has no public deals feed, so the shelf is always empty.
    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHit>, PluginError> {
        let _ = params;
        Ok(Vec::new())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(LibroRoot).await?;
    Ok(())
}
