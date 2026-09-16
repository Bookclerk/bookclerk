//! External GraphicAudio source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    serve, Bindings, Brand, CatalogHit, ContentSource as ContentSourceRole, Entrypoints,
    ExpandCandidatesParams, FetchTitleParams, HealthOk, Invocation, ListDealsParams, LoginParams,
    LoginResult, PlainFetch, PluginDescribe, PluginError, PluginWorker, PortalAuthMode,
    PurchaseHint, PurchaseHintParams, ScalarLimits, ScanParams, ScanSummary, SearchCatalogParams,
    FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_source::abi::{expand_seed_from_params, DEFAULT_LIST_DEALS_LIMIT};
use bookclerk_source::{CatalogSearchOpts, ContentSource, PurchaseHintOpts};
use serde_json::Value;

/// GraphicAudio source guest; password login via `BOOKCLERK_GA_PASSWORD` or Accounts.
struct GraphicAudioRoot;

#[async_trait(?Send)]
impl PluginWorker for GraphicAudioRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "graphicaudio".into(),
            display_name: Some("GraphicAudio".into()),
            rpc_features: vec![FEATURE_SCALAR_LIMITS.into()],
            scalar_limits: ScalarLimits::default().into(),
            capabilities: manifest_capabilities(include_str!("../plugin.toml"))?,
            portal_auth_mode: PortalAuthMode::Password,
            password_env_var: Some(bookclerk_plugin_source_graphicaudio::GA_PASSWORD_ENV.into()),
            aliases: vec!["ga".into(), "graphic-audio".into()],
            sort_key: 2,
            brand: Some(Brand {
                id: "graphicaudio".into(),
                name: "GraphicAudio".into(),
                bg: "#111827".into(),
                fg: "#F9FAFB".into(),
                accent: "#DC2626".into(),
                icon_url: Some(
                    "https://www.google.com/s2/favicons?domain=graphicaudio.com&sz=128".into(),
                ),
            }),
            ..PluginDescribe::default()
        })
    }

    async fn open(
        &self,
        _invocation: Invocation,
        _bindings: Bindings,
    ) -> Result<Entrypoints, PluginError> {
        Ok(Entrypoints {
            storefront: Some(Box::new(GraphicAudioContentSource)),
            ..Entrypoints::default()
        })
    }
}

struct GraphicAudioContentSource;

fn internal(err: impl std::fmt::Display) -> PluginError {
    PluginError::internal(err.to_string())
}

fn hits(hits: Vec<bookclerk_source::CatalogHit>) -> Vec<CatalogHit> {
    hits.into_iter().map(Into::into).collect()
}

#[async_trait(?Send)]
impl ContentSourceRole for GraphicAudioContentSource {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "graphicaudio source plugin ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<Vec<String>, PluginError> {
        Ok(vec!["graphicaudio plugin diagnose: ok".into()])
    }

    async fn login(&self, params: LoginParams) -> Result<LoginResult, PluginError> {
        let cfg = Value::Null;
        let access_url = bookclerk_plugin_source_graphicaudio::resolve_access_base_url(&cfg);
        let store_url = bookclerk_plugin_source_graphicaudio::resolve_store_base_url(&cfg);
        let access = bookclerk_plugin_source_graphicaudio::resolve_access(&cfg);
        bookclerk_plugin_source_graphicaudio::guest_login_rpc(
            &access_url,
            &store_url,
            access,
            params,
        )
        .await
        .map_err(internal)
    }

    async fn scan(&self, params: ScanParams) -> Result<ScanSummary, PluginError> {
        let cfg = Value::Null;
        let access_url = bookclerk_plugin_source_graphicaudio::resolve_access_base_url(&cfg);
        let store_url = bookclerk_plugin_source_graphicaudio::resolve_store_base_url(&cfg);
        let access = bookclerk_plugin_source_graphicaudio::resolve_access(&cfg);
        bookclerk_plugin_source_graphicaudio::guest_scan_rpc(
            &access_url,
            &store_url,
            access,
            None,
            &params,
        )
        .await
        .map_err(internal)
    }

    async fn fetch_title(&self, params: FetchTitleParams) -> Result<PlainFetch, PluginError> {
        let cfg = params.source_config.json_value().unwrap_or(Value::Null);
        let access_url = bookclerk_plugin_source_graphicaudio::resolve_access_base_url(&cfg);
        let store_url = bookclerk_plugin_source_graphicaudio::resolve_store_base_url(&cfg);
        let access = bookclerk_plugin_source_graphicaudio::resolve_access(&cfg);
        let bitrate = bookclerk_plugin_source_graphicaudio::resolve_bitrate(&cfg);
        let container = bookclerk_plugin_source_graphicaudio::resolve_container(&cfg);
        bookclerk_plugin_source_graphicaudio::guest_fetch_title_rpc(
            &access_url,
            &store_url,
            &params,
            access,
            bitrate,
            container,
            None,
        )
        .await
        .map_err(internal)
    }

    async fn search_catalog(
        &self,
        params: SearchCatalogParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
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
        let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
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
        let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
        source
            .purchase_hint(&PurchaseHintOpts::from(params))
            .await
            .map(|hint| hint.map(Into::into))
            .map_err(internal)
    }

    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHit>, PluginError> {
        let limit = params.limit.unwrap_or(DEFAULT_LIST_DEALS_LIMIT);
        let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
        source
            .list_deals(usize::try_from(limit).unwrap_or(usize::MAX))
            .await
            .map(hits)
            .map_err(internal)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(GraphicAudioRoot).await?;
    Ok(())
}
