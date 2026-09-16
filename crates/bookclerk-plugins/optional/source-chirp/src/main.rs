//! External Chirp source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    serve, Brand, CatalogHit, ContentSource as ContentSourceRole, ContentSourceContext,
    ExpandCandidatesParams, FetchTitleParams, HealthOk, ListDealsParams, LoginParams, LoginResult,
    PlainFetch, PluginDescribe, PluginError, PluginRoot, PortalAuthMode, PurchaseHint,
    PurchaseHintParams, ScalarLimits, ScanParams, ScanSummary, SearchCatalogParams,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_source::abi::{expand_seed_from_params, DEFAULT_LIST_DEALS_LIMIT};
use bookclerk_source::{CatalogSearchOpts, ContentSource, PurchaseHintOpts};
use serde_json::Value;

/// External Chirp storefront guest (`kind = source`, password portal auth).
struct ChirpRoot;

#[async_trait(?Send)]
impl PluginRoot for ChirpRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "chirp".into(),
            kind: "source".into(),
            display_name: Some("Chirp".into()),
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
                "expandCandidates".into(),
                "purchaseHint".into(),
                "listDeals".into(),
            ],
            portal_auth_mode: PortalAuthMode::Password,
            password_env_var: Some(bookclerk_plugin_source_chirp::PASSWORD_ENV.into()),
            sort_key: 3,
            brand: Some(Brand {
                id: "chirp".into(),
                name: "Chirp".into(),
                bg: "#E85D04".into(),
                fg: "#FFFFFF".into(),
                accent: "#F48C06".into(),
                icon_url: Some(
                    "https://www.google.com/s2/favicons?domain=chirpbooks.com&sz=128".into(),
                ),
            }),
            ..PluginDescribe::default()
        })
    }

    async fn content_source(
        &self,
        _context: ContentSourceContext,
    ) -> Result<Box<dyn ContentSourceRole>, PluginError> {
        Ok(Box::new(ChirpContentSource))
    }
}

struct ChirpContentSource;

fn internal(err: impl std::fmt::Display) -> PluginError {
    PluginError::internal(err.to_string())
}

fn hits(hits: Vec<bookclerk_source::CatalogHit>) -> Vec<CatalogHit> {
    hits.into_iter().map(Into::into).collect()
}

#[async_trait(?Send)]
impl ContentSourceRole for ChirpContentSource {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "chirp source plugin ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<Vec<String>, PluginError> {
        Ok(vec!["chirp plugin diagnose: ok".into()])
    }

    async fn login(&self, params: LoginParams) -> Result<LoginResult, PluginError> {
        let gql = bookclerk_plugin_source_chirp::resolve_graphql_url(&Value::Null);
        bookclerk_plugin_source_chirp::guest_login_rpc(&gql, params)
            .await
            .map_err(internal)
    }

    async fn scan(&self, params: ScanParams) -> Result<ScanSummary, PluginError> {
        let gql = bookclerk_plugin_source_chirp::resolve_graphql_url(&Value::Null);
        bookclerk_plugin_source_chirp::guest_scan_rpc(&gql, &params)
            .await
            .map_err(internal)
    }

    async fn fetch_title(&self, params: FetchTitleParams) -> Result<PlainFetch, PluginError> {
        let source_config = params.source_config.json_value().unwrap_or(Value::Null);
        let gql = bookclerk_plugin_source_chirp::resolve_graphql_url(&source_config);
        bookclerk_plugin_source_chirp::guest_fetch_title_rpc(&gql, &params)
            .await
            .map_err(internal)
    }

    async fn search_catalog(
        &self,
        params: SearchCatalogParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
        source
            .search_catalog(&CatalogSearchOpts::from(params))
            .await
            .map(hits)
            .map_err(internal)
    }

    async fn expand_candidates(
        &self,
        params: ExpandCandidatesParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        let (seed, limit) = expand_seed_from_params(params);
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
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
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
        source
            .purchase_hint(&PurchaseHintOpts::from(params))
            .await
            .map(|hint| hint.map(Into::into))
            .map_err(internal)
    }

    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHit>, PluginError> {
        let limit = params.limit.unwrap_or(DEFAULT_LIST_DEALS_LIMIT);
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
        source
            .list_deals(usize::try_from(limit).unwrap_or(usize::MAX))
            .await
            .map(hits)
            .map_err(internal)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(ChirpRoot).await?;
    Ok(())
}
