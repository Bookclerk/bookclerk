//! External Audible source plugin for Bookclerk.

use bookclerk_plugin_sdk::{
    methods, BrandDto, ConfigOptionDto, ConfigOptionValueDto, ExpandCandidatesParams,
    FetchTitleParams, HandshakeResult, HealthDto, LoginCompleteParams, LoginParams,
    LoginStartResultDto, PluginGuest, PurchaseHintParams, ScanParams, SearchCatalogParams,
    PLUGIN_API_VERSION,
};
use bookclerk_source::{
    CatalogSearchOpts, CatalogSearchSort, ContentSource, ExpandSeed, PurchaseHintOpts,
};
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    PluginGuest::serve(|method, params| async move {
        match method.as_str() {
            methods::HANDSHAKE => Ok(serde_json::to_value(HandshakeResult {
                api_version: PLUGIN_API_VERSION,
                id: "audible".into(),
                kind: "source".into(),
                display_name: Some("Audible".into()),
                capabilities: vec![
                    "health".into(),
                    "diagnose".into(),
                    "login.start".into(),
                    "login.complete".into(),
                    "scan".into(),
                    "fetch_title".into(),
                    "search_catalog".into(),
                    "expand_candidates".into(),
                    "purchase_hint".into(),
                    "list_deals".into(),
                ],
                portal_auth_mode: Some("oauth".into()),
                password_env_var: None,
                aliases: vec![],
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
                cli: None,
            })
            .unwrap()),
            methods::HEALTH => Ok(serde_json::to_value(HealthDto {
                id: "audible".into(),
                enabled: true,
                ok: true,
                detail: Some("audible source plugin ready".into()),
            })
            .unwrap()),
            methods::DIAGNOSE => Ok(json!(["audible plugin diagnose: ok"])),
            methods::LOGIN_START => {
                let p: LoginParams = serde_json::from_value(params)
                    .map_err(|e| format!("login.start params: {e}"))?;
                let (session_id, url) = bookclerk_plugin_source_audible::guest_login_start(&p)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(LoginStartResultDto { session_id, url }).unwrap())
            }
            methods::LOGIN_COMPLETE => {
                let p: LoginCompleteParams = serde_json::from_value(params)
                    .map_err(|e| format!("login.complete params: {e}"))?;
                let result = bookclerk_plugin_source_audible::guest_login_complete(&p.session_id)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(result).unwrap())
            }
            methods::SCAN => {
                let p: ScanParams =
                    serde_json::from_value(params).map_err(|e| format!("scan params: {e}"))?;
                let summary = bookclerk_plugin_source_audible::guest_scan(
                    &p.credentials,
                    &p.accounts,
                    p.page_size,
                    p.import_episodes,
                    p.import_plus_titles,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(summary).unwrap())
            }
            methods::FETCH_TITLE => {
                let p: FetchTitleParams = serde_json::from_value(params)
                    .map_err(|e| format!("fetch_title params: {e}"))?;
                let work_dir =
                    bookclerk_plugin_sdk::fetch_work_dir(&p).map_err(|e| e.to_string())?;
                let creds = p
                    .credentials
                    .ok_or_else(|| "fetch_title requires host credentials".to_string())?;
                let dto = bookclerk_plugin_source_audible::guest_fetch_title(
                    &creds,
                    &p.title_id,
                    &work_dir,
                    &p.source_config,
                    &p.download,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::SEARCH_CATALOG => {
                let p: SearchCatalogParams = serde_json::from_value(params)
                    .map_err(|e| format!("search_catalog params: {e}"))?;
                let source = bookclerk_plugin_source_audible::AudibleSource::new();
                let hits = source
                    .search_catalog(&CatalogSearchOpts {
                        query: p.query,
                        region: p.region,
                        limit: p.limit,
                        page: p.page.max(1),
                        sort: p
                            .sort
                            .as_deref()
                            .map(CatalogSearchSort::from_wire)
                            .unwrap_or_default(),
                        field: p
                            .field
                            .as_deref()
                            .and_then(bookclerk_source::CatalogSearchField::from_wire),
                        language: p.language,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                let dtos: Vec<_> = hits
                    .into_iter()
                    .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            methods::EXPAND_CANDIDATES => {
                let p: ExpandCandidatesParams = serde_json::from_value(params)
                    .map_err(|e| format!("expand_candidates params: {e}"))?;
                let source = bookclerk_plugin_source_audible::AudibleSource::new();
                let hits = source
                    .expand_candidates(
                        &ExpandSeed {
                            source: p.source,
                            product_id: p.product_id,
                            title: p.title,
                            authors: p.authors,
                            narrators: p.narrators,
                            series: p.series,
                            series_asin: p.series_asin,
                            asin: p.asin,
                            isbn: p.isbn,
                            region: p.region,
                        },
                        p.limit,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let dtos: Vec<_> = hits
                    .into_iter()
                    .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            methods::PURCHASE_HINT => {
                let p: PurchaseHintParams = serde_json::from_value(params)
                    .map_err(|e| format!("purchase_hint params: {e}"))?;
                let source = bookclerk_plugin_source_audible::AudibleSource::new();
                let hint = source
                    .purchase_hint(&PurchaseHintOpts {
                        product_id: p.product_id,
                        title: p.title,
                        authors: p.authors,
                        asin: p.asin,
                        isbn: p.isbn,
                        region: p.region,
                        with_price: p.with_price,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                let dto = hint.map(bookclerk_plugin_source_audible::purchase_hint_to_dto);
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::LIST_DEALS => {
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let source = bookclerk_plugin_source_audible::AudibleSource::new();
                let hits = source.list_deals(limit).await.map_err(|e| e.to_string())?;
                let dtos: Vec<_> = hits
                    .into_iter()
                    .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            other => Err(format!("unsupported method `{other}`")),
        }
    })
    .await?;
    Ok(())
}
