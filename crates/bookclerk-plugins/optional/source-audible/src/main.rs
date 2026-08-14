//! External Audible source plugin for Bookclerk.

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    BookclerkPlugin, BookclerkPluginGuest, BrandDto, CatalogHitDto, ConfigOptionDto,
    ConfigOptionValueDto, DiagnoseResult, ExpandCandidatesParams, FetchTitleParams,
    HandshakeParams, HandshakeResult, HealthResult, ListDealsParams, LoginCompleteParams,
    LoginResultDto, LoginStartParams, LoginStartResultDto, PluginError, PurchaseHintDto,
    PurchaseHintParams, ScanParams, ScanSummaryDto, SearchCatalogParams, SourceFetchDto,
    PLUGIN_API_VERSION,
};
use bookclerk_source::{
    CatalogSearchOpts, CatalogSearchSort, ContentSource, ExpandSeed, PurchaseHintOpts,
};

/// Audible source guest; OAuth login uses the host-owned callback tunnel.
struct AudiblePlugin;

#[async_trait]
impl BookclerkPlugin for AudiblePlugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
            id: "audible".into(),
            kind: "source".into(),
            display_name: Some("Audible".into()),
            capabilities: vec![
                "health".into(),
                "diagnose".into(),
                "loginStart".into(),
                "loginComplete".into(),
                "scan".into(),
                "fetchTitle".into(),
                "searchCatalog".into(),
                "expandCandidates".into(),
                "purchaseHint".into(),
                "listDeals".into(),
            ],
            portal_auth_mode: Some("oauth".into()),
            sort_key: Some(0),
            brand: Some(BrandDto {
                id: "audible".into(),
                name: "Audible".into(),
                bg: "#F8991D".into(),
                fg: "#111111".into(),
                accent: "#D97706".into(),
                icon_url: "https://www.google.com/s2/favicons?domain=audible.com&sz=128".into(),
            }),
            config_options: vec![ConfigOptionDto {
                key: "bitrate".into(),
                label: "Bitrate".into(),
                values: vec![
                    ConfigOptionValueDto {
                        id: "high".into(),
                        label: "High".into(),
                    },
                    ConfigOptionValueDto {
                        id: "normal".into(),
                        label: "Normal".into(),
                    },
                ],
            }],
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some("audible".into()),
            enabled: Some(true),
            detail: Some("audible source plugin ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["audible plugin diagnose: ok".into()],
        })
    }

    async fn login_start(
        &self,
        params: LoginStartParams,
    ) -> Result<LoginStartResultDto, PluginError> {
        let (session_id, url) = bookclerk_plugin_source_audible::guest_login_start(&params)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(LoginStartResultDto { session_id, url })
    }

    async fn login_complete(
        &self,
        params: LoginCompleteParams,
    ) -> Result<LoginResultDto, PluginError> {
        bookclerk_plugin_source_audible::guest_login_complete(&params.session_id)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn scan(&self, params: ScanParams) -> Result<ScanSummaryDto, PluginError> {
        bookclerk_plugin_source_audible::guest_scan(
            &params.credentials,
            &params.accounts,
            params.page_size,
            params.import_episodes,
            params.import_plus_titles,
        )
        .await
        .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn fetch_title(&self, params: FetchTitleParams) -> Result<SourceFetchDto, PluginError> {
        let work_dir = bookclerk_plugin_sdk::fetch_work_dir(&params)
            .map_err(|e| PluginError::internal(e.to_string()))?;
        let creds = params
            .credentials
            .ok_or_else(|| PluginError::invalid_params("fetchTitle requires host credentials"))?;
        bookclerk_plugin_source_audible::guest_fetch_title(
            &creds,
            &params.title_id,
            &work_dir,
            &params.source_config,
            &params.download,
        )
        .await
        .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn search_catalog(
        &self,
        params: SearchCatalogParams,
    ) -> Result<Vec<CatalogHitDto>, PluginError> {
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        let hits = source
            .search_catalog(&CatalogSearchOpts {
                query: params.query,
                region: params.region,
                limit: params.limit,
                page: params.page.max(1),
                sort: params
                    .sort
                    .as_deref()
                    .map(CatalogSearchSort::from_wire)
                    .unwrap_or_default(),
                field: params
                    .field
                    .as_deref()
                    .and_then(bookclerk_source::CatalogSearchField::from_wire),
                language: params.language,
            })
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(hits
            .into_iter()
            .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
            .collect())
    }

    async fn expand_candidates(
        &self,
        params: ExpandCandidatesParams,
    ) -> Result<Vec<CatalogHitDto>, PluginError> {
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        let hits = source
            .expand_candidates(
                &ExpandSeed {
                    source: params.source,
                    product_id: params.product_id,
                    title: params.title,
                    authors: params.authors,
                    narrators: params.narrators,
                    series: params.series,
                    series_asin: params.series_asin,
                    asin: params.asin,
                    isbn: params.isbn,
                    region: params.region,
                },
                params.limit,
            )
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(hits
            .into_iter()
            .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
            .collect())
    }

    async fn purchase_hint(
        &self,
        params: PurchaseHintParams,
    ) -> Result<Option<PurchaseHintDto>, PluginError> {
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        let hint = source
            .purchase_hint(&PurchaseHintOpts {
                product_id: params.product_id,
                title: params.title,
                authors: params.authors,
                asin: params.asin,
                isbn: params.isbn,
                region: params.region,
                with_price: params.with_price,
            })
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(hint.map(bookclerk_plugin_source_audible::purchase_hint_to_dto))
    }

    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHitDto>, PluginError> {
        let limit = params.limit.unwrap_or(20);
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        let hits = source
            .list_deals(limit)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(hits
            .into_iter()
            .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
            .collect())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(AudiblePlugin).await?;
    Ok(())
}
