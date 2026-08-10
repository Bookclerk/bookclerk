//! External Libro.fm source plugin for Bookclerk.

use async_trait::async_trait;
use bookclerk_plugin_sdk::{
    BookclerkPlugin, BookclerkPluginGuest, BrandDto, CatalogDetailParams, CatalogHitDto,
    ConfigOptionDto, ConfigOptionValueDto, DiagnoseResult, ExpandCandidatesParams,
    FetchTitleParams, HandshakeParams, HandshakeResult, HealthResult, ListDealsParams, LoginParams,
    LoginResultDto, PluginError, PurchaseHintDto, PurchaseHintParams, ScanParams, ScanSummaryDto,
    SearchCatalogParams, SourceFetchDto, PLUGIN_API_VERSION,
};
use bookclerk_source::{
    CatalogSearchOpts, CatalogSearchSort, ContentSource, ExpandSeed, PurchaseHintOpts,
};
use serde_json::Value;

struct LibroPlugin;

#[async_trait]
impl BookclerkPlugin for LibroPlugin {
    async fn handshake(&self, _params: HandshakeParams) -> Result<HandshakeResult, PluginError> {
        Ok(HandshakeResult {
            api_version: PLUGIN_API_VERSION,
            id: "libro".into(),
            kind: "source".into(),
            display_name: Some("Libro.fm".into()),
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
            portal_auth_mode: Some("password".into()),
            password_env_var: Some(bookclerk_plugin_source_libro::PASSWORD_ENV.into()),
            aliases: vec!["libro.fm".into(), "librofm".into()],
            sort_key: Some(1),
            brand: Some(BrandDto {
                id: "libro".into(),
                name: "Libro.fm".into(),
                bg: "#1F4E3D".into(),
                fg: "#F4F1EA".into(),
                accent: "#2F6B53".into(),
                icon_url: "https://www.google.com/s2/favicons?domain=libro.fm&sz=128".into(),
            }),
            config_options: vec![ConfigOptionDto {
                key: "container".into(),
                label: "Container".into(),
                values: vec![
                    ConfigOptionValueDto {
                        id: "m4b".into(),
                        label: "M4B".into(),
                    },
                    ConfigOptionValueDto {
                        id: "zip".into(),
                        label: "ZIP (MP3 parts)".into(),
                    },
                ],
            }],
            ..HandshakeResult::default()
        })
    }

    async fn health(&self) -> Result<HealthResult, PluginError> {
        Ok(HealthResult {
            ok: true,
            id: Some("libro".into()),
            enabled: Some(true),
            detail: Some("libro source plugin ready".into()),
        })
    }

    async fn diagnose(&self) -> Result<DiagnoseResult, PluginError> {
        Ok(DiagnoseResult {
            lines: vec!["libro plugin diagnose: ok".into()],
        })
    }

    async fn login(&self, params: LoginParams) -> Result<LoginResultDto, PluginError> {
        let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
        bookclerk_plugin_source_libro::guest_login_rpc(&base, params)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn scan(&self, params: ScanParams) -> Result<ScanSummaryDto, PluginError> {
        let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
        bookclerk_plugin_source_libro::guest_scan_rpc(&base, &params)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn fetch_title(&self, params: FetchTitleParams) -> Result<SourceFetchDto, PluginError> {
        let base = bookclerk_plugin_source_libro::resolve_base_url(&params.source_config);
        let container = bookclerk_plugin_source_libro::resolve_container(&params.source_config);
        bookclerk_plugin_source_libro::guest_fetch_title_rpc(&base, &params, container)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))
    }

    async fn search_catalog(
        &self,
        params: SearchCatalogParams,
    ) -> Result<Vec<CatalogHitDto>, PluginError> {
        let source = bookclerk_plugin_source_libro::LibroSource::new();
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
            .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
            .collect())
    }

    async fn catalog_detail(
        &self,
        params: CatalogDetailParams,
    ) -> Result<Option<CatalogHitDto>, PluginError> {
        let key = params
            .isbn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(params.product_id.trim());
        let source = bookclerk_plugin_source_libro::LibroSource::new();
        let hit = source
            .catalog_detail(key)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(hit.map(bookclerk_plugin_source_libro::catalog_hit_to_dto))
    }

    async fn expand_candidates(
        &self,
        params: ExpandCandidatesParams,
    ) -> Result<Vec<CatalogHitDto>, PluginError> {
        let source = bookclerk_plugin_source_libro::LibroSource::new();
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
            .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
            .collect())
    }

    async fn purchase_hint(
        &self,
        params: PurchaseHintParams,
    ) -> Result<Option<PurchaseHintDto>, PluginError> {
        let source = bookclerk_plugin_source_libro::LibroSource::new();
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
        Ok(hint.map(bookclerk_plugin_source_libro::purchase_hint_to_dto))
    }

    async fn list_deals(&self, params: ListDealsParams) -> Result<Vec<CatalogHitDto>, PluginError> {
        let limit = params.limit.unwrap_or(20);
        let source = bookclerk_plugin_source_libro::LibroSource::new();
        let hits = source
            .list_deals(limit)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        Ok(hits
            .into_iter()
            .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
            .collect())
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    BookclerkPluginGuest::serve(LibroPlugin).await?;
    Ok(())
}
