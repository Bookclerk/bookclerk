//! External Audible source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::manifest_capabilities;
use bookclerk_plugin_sdk::{
    serve, Bindings, Brand, CatalogCapabilities, CatalogHit, ContentSource as ContentSourceRole,
    Entrypoints, ExpandCandidatesParams, FetchTitleParams, HealthOk, Invocation, ListDealsParams,
    LoginCompleteParams, LoginParams, LoginResult, LoginStartResult, PlainFetch, PluginDescribe,
    PluginError, PluginWorker, PortalAuthMode, PurchaseHint, PurchaseHintParams,
    PurchaseHintsParams, RevokeAccountParams, ScalarLimits, ScanPage, ScanParams,
    SearchCatalogParams, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_source_audible::catalog;
use bookclerk_storefront::abi::{credentials_from_bytes, expand_seed_from_params};
use bookclerk_storefront::{CatalogSearchOpts, PurchaseHintOpts};
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
            // Audible's catalog details take an ASIN, not an ISBN; its search
            // hits carry `rank_hint` instead.
            catalog: CatalogCapabilities {
                detail_by_isbn: false,
            },
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

fn hits(hits: Vec<bookclerk_storefront::CatalogHit>) -> Vec<CatalogHit> {
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

    async fn scan(&self, params: ScanParams) -> Result<ScanPage, PluginError> {
        let cursor = bookclerk_plugin_source_audible::AudibleScanCursor::parse(&params.cursor)?;
        bookclerk_plugin_source_audible::guest_scan_page(&params, cursor)
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

    async fn revoke_account(&self, params: RevokeAccountParams) -> Result<(), PluginError> {
        let Some(creds) = params.credentials.as_deref() else {
            return Ok(());
        };
        let creds = credentials_from_bytes(creds)
            .map_err(|e| PluginError::invalid_params(e.to_string()))?;
        let device = bookclerk_plugin_source_audible::guest_revoke_account(&creds)
            .await
            .map_err(internal)?;
        tracing::info!(
            account = %params.account_id,
            device = device.as_deref().unwrap_or("unknown"),
            "deregistered Audible device"
        );
        Ok(())
    }

    async fn search_catalog(
        &self,
        params: SearchCatalogParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        catalog::search_catalog(&CatalogSearchOpts::from(params))
            .await
            .map(hits)
            .map_err(internal)
    }

    async fn expand_candidates(
        &self,
        params: ExpandCandidatesParams,
    ) -> Result<Vec<CatalogHit>, PluginError> {
        let (seed, limit) = expand_seed_from_params(params);
        catalog::expand_candidates(&seed, limit)
            .await
            .map(hits)
            .map_err(internal)
    }

    async fn purchase_hint(
        &self,
        params: PurchaseHintParams,
    ) -> Result<Option<PurchaseHint>, PluginError> {
        catalog::purchase_hint(&PurchaseHintOpts::from(params))
            .await
            .map(|hint| hint.map(Into::into))
            .map_err(PluginError::from)
    }

    /// One catalog request prices up to fifty titles, so a page of cards
    /// is one call.
    async fn purchase_hints(
        &self,
        params: PurchaseHintsParams,
    ) -> Result<Vec<Option<PurchaseHint>>, PluginError> {
        let opts: Vec<PurchaseHintOpts> = params
            .queries
            .into_iter()
            .map(PurchaseHintOpts::from)
            .collect();
        catalog::purchase_hints(&opts)
            .await
            .map(|hints| hints.into_iter().map(|h| h.map(Into::into)).collect())
            .map_err(PluginError::from)
    }

    /// Audible has no deals feed; answer an empty list rather than
    /// "unsupported" so Discover treats it like any store without deals.
    async fn list_deals(&self, _params: ListDealsParams) -> Result<Vec<CatalogHit>, PluginError> {
        Ok(Vec::new())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(AudibleRoot).await?;
    Ok(())
}
