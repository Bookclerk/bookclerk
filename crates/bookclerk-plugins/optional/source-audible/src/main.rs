//! External Audible source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    serve, Bindings, Brand, CatalogHit, ConfigOption, ConfigOptionValue,
    ContentSource as ContentSourceRole, Entrypoints, ExpandCandidatesParams, FetchTitleParams,
    HealthOk, Invocation, ListDealsParams, LoginCompleteParams, LoginParams, LoginResult,
    LoginStartResult, PlainFetch, PluginDescribe, PluginError, PluginWorker, PortalAuthMode,
    PurchaseHint, PurchaseHintParams, ScalarLimits, ScanParams, ScanSummary, SearchCatalogParams,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_source::abi::{
    account_credentials_json, credentials_from_bytes, expand_seed_from_params,
    DEFAULT_LIST_DEALS_LIMIT,
};
use bookclerk_source::{CatalogSearchOpts, ContentSource, PurchaseHintOpts};
use serde_json::Value;

/// Audible source guest; OAuth login uses the host-owned callback tunnel.
struct AudibleRoot;

#[async_trait(?Send)]
impl PluginWorker for AudibleRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "audible".into(),
            display_name: Some("Audible".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            portal_auth_mode: PortalAuthMode::Oauth,
            sort_key: 0,
            brand: Some(Brand {
                id: "audible".into(),
                name: "Audible".into(),
                bg: "#F8991D".into(),
                fg: "#111111".into(),
                accent: "#D97706".into(),
                icon_url: Some(
                    "https://www.google.com/s2/favicons?domain=audible.com&sz=128".into(),
                ),
            }),
            config_options: vec![ConfigOption {
                key: "bitrate".into(),
                label: "Bitrate".into(),
                values: vec![
                    ConfigOptionValue {
                        id: "high".into(),
                        label: "High".into(),
                    },
                    ConfigOptionValue {
                        id: "normal".into(),
                        label: "Normal".into(),
                    },
                ],
            }],
            ..PluginDescribe::default()
        })
    }

    async fn open(
        &self,
        _invocation: Invocation,
        _bindings: Bindings,
    ) -> Result<Entrypoints, PluginError> {
        Ok(Entrypoints {
            storefront: Some(Box::new(AudibleContentSource)),
            ..Entrypoints::default()
        })
    }
}

struct AudibleContentSource;

fn internal(err: impl std::fmt::Display) -> PluginError {
    PluginError::internal(err.to_string())
}

fn hits(hits: Vec<bookclerk_source::CatalogHit>) -> Vec<CatalogHit> {
    hits.into_iter().map(Into::into).collect()
}

#[async_trait(?Send)]
impl ContentSourceRole for AudibleContentSource {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "audible source plugin ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<Vec<String>, PluginError> {
        Ok(vec!["audible plugin diagnose: ok".into()])
    }

    async fn login_start(&self, params: LoginParams) -> Result<LoginStartResult, PluginError> {
        let (session_id, url) = bookclerk_plugin_source_audible::guest_login_start(&params)
            .await
            .map_err(internal)?;
        Ok(LoginStartResult { session_id, url })
    }

    async fn login_complete(
        &self,
        params: LoginCompleteParams,
    ) -> Result<LoginResult, PluginError> {
        bookclerk_plugin_source_audible::guest_login_complete(&params.session_id)
            .await
            .map_err(internal)
    }

    async fn scan(&self, params: ScanParams) -> Result<ScanSummary, PluginError> {
        let credentials = account_credentials_json(&params.credentials)
            .map_err(|e| PluginError::invalid_params(e.to_string()))?;
        bookclerk_plugin_source_audible::guest_scan(
            &credentials,
            &params.accounts,
            params.page_size,
            params.import_episodes,
            params.import_plus_titles,
        )
        .await
        .map_err(internal)
    }

    async fn fetch_title(&self, params: FetchTitleParams) -> Result<PlainFetch, PluginError> {
        let work_dir = bookclerk_plugin_sdk::fetch_work_dir(&params).map_err(internal)?;
        let creds = params
            .credentials
            .as_deref()
            .ok_or_else(|| PluginError::invalid_params("fetchTitle requires host credentials"))?;
        let creds = credentials_from_bytes(creds)
            .map_err(|e| PluginError::invalid_params(e.to_string()))?;
        let source_config = params.source_config.json_value().unwrap_or(Value::Null);
        bookclerk_plugin_source_audible::guest_fetch_title(
            &creds,
            &params.title_id,
            &work_dir,
            &source_config,
            &params.fetch,
        )
        .await
        .map_err(internal)
    }

    async fn search_catalog(
        &self,
        params: SearchCatalogParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
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
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
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
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        source
            .purchase_hint(&PurchaseHintOpts::from(params))
            .await
            .map(|hint| hint.map(Into::into))
            .map_err(internal)
    }

    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHit>, PluginError> {
        let limit = params.limit.unwrap_or(DEFAULT_LIST_DEALS_LIMIT);
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        source
            .list_deals(usize::try_from(limit).unwrap_or(usize::MAX))
            .await
            .map(hits)
            .map_err(internal)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(AudibleRoot).await?;
    Ok(())
}
