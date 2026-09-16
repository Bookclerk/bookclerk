//! External Libro.fm source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    serve, Brand, CatalogDetailParams, CatalogHit, ConfigOption, ConfigOptionValue,
    ContentSource as ContentSourceRole, ContentSourceContext, ExpandCandidatesParams,
    FetchTitleParams, HealthOk, ListDealsParams, LoginParams, LoginResult, PlainFetch,
    PluginDescribe, PluginError, PluginRoot, PortalAuthMode, PurchaseHint, PurchaseHintParams,
    ScalarLimits, ScanParams, ScanSummary, SearchCatalogParams, FEATURE_SCALAR_LIMITS,
    PRODUCT_API_VERSION,
};
use bookclerk_source::abi::{expand_seed_from_params, DEFAULT_LIST_DEALS_LIMIT};
use bookclerk_source::{CatalogSearchOpts, ContentSource, PurchaseHintOpts};
use serde_json::Value;

/// External Libro.fm source guest; `describe` advertises scan/fetch/catalog capabilities.
struct LibroRoot;

#[async_trait(?Send)]
impl PluginRoot for LibroRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "libro".into(),
            kind: "source".into(),
            display_name: Some("Libro.fm".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            supported_roles: vec!["contentSource".into()],
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "login".into(),
                "scan".into(),
                "fetchTitle".into(),
                "searchCatalog".into(),
                "catalogDetail".into(),
                "expandCandidates".into(),
                "purchaseHint".into(),
                "listDeals".into(),
            ],
            portal_auth_mode: PortalAuthMode::Password,
            password_env_var: Some(bookclerk_plugin_source_libro::PASSWORD_ENV.into()),
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
            config_options: vec![ConfigOption {
                key: "container".into(),
                label: "Container".into(),
                values: vec![
                    ConfigOptionValue {
                        id: "m4b".into(),
                        label: "M4B".into(),
                    },
                    ConfigOptionValue {
                        id: "zip".into(),
                        label: "ZIP (MP3 parts)".into(),
                    },
                ],
            }],
            ..PluginDescribe::default()
        })
    }

    async fn content_source(
        &self,
        _context: ContentSourceContext,
    ) -> Result<Box<dyn ContentSourceRole>, PluginError> {
        Ok(Box::new(LibroContentSource))
    }
}

struct LibroContentSource;

fn internal(err: impl std::fmt::Display) -> PluginError {
    PluginError::internal(err.to_string())
}

fn hits(hits: Vec<bookclerk_source::CatalogHit>) -> Vec<CatalogHit> {
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
        let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
        bookclerk_plugin_source_libro::guest_login_rpc(&base, params)
            .await
            .map_err(internal)
    }

    async fn scan(&self, params: ScanParams) -> Result<ScanSummary, PluginError> {
        let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
        bookclerk_plugin_source_libro::guest_scan_rpc(&base, &params)
            .await
            .map_err(internal)
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
        let source = bookclerk_plugin_source_libro::LibroSource::new();
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
        let source = bookclerk_plugin_source_libro::LibroSource::new();
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
        let source = bookclerk_plugin_source_libro::LibroSource::new();
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
        let source = bookclerk_plugin_source_libro::LibroSource::new();
        source
            .purchase_hint(&PurchaseHintOpts::from(params))
            .await
            .map(|hint| hint.map(Into::into))
            .map_err(internal)
    }

    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHit>, PluginError> {
        let limit = params.limit.unwrap_or(DEFAULT_LIST_DEALS_LIMIT);
        let source = bookclerk_plugin_source_libro::LibroSource::new();
        source
            .list_deals(usize::try_from(limit).unwrap_or(usize::MAX))
            .await
            .map(hits)
            .map_err(internal)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(LibroRoot).await?;
    Ok(())
}
