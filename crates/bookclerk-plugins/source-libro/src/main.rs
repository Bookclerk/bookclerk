//! External Libro.fm source plugin for Bookclerk.

use bookclerk_plugin_sdk::{
    methods, BrandDto, ConfigOptionDto, ConfigOptionValueDto, ExpandCandidatesParams,
    FetchTitleParams, HandshakeResult, HealthDto, LoginParams, PluginGuest, PurchaseHintParams,
    ScanParams, SearchCatalogParams, PLUGIN_API_VERSION,
};
use bookclerk_source::{CatalogSearchOpts, ContentSource, ExpandSeed, PurchaseHintOpts};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    PluginGuest::serve(|method, params| async move {
        match method.as_str() {
            methods::HANDSHAKE => Ok(serde_json::to_value(HandshakeResult {
                api_version: PLUGIN_API_VERSION,
                id: "libro".into(),
                kind: "source".into(),
                display_name: Some("Libro.fm".into()),
                capabilities: vec![
                    "health".into(),
                    "diagnose".into(),
                    "login".into(),
                    "scan".into(),
                    "fetch_title".into(),
                    "search_catalog".into(),
                    "expand_candidates".into(),
                    "purchase_hint".into(),
                    "list_deals".into(),
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
                cli: None,
            })
            .unwrap()),
            methods::HEALTH => Ok(serde_json::to_value(HealthDto {
                id: "libro".into(),
                enabled: true,
                ok: true,
                detail: Some("libro source plugin ready".into()),
            })
            .unwrap()),
            methods::DIAGNOSE => Ok(json!(["libro plugin diagnose: ok"])),
            methods::LOGIN => {
                let p: LoginParams =
                    serde_json::from_value(params).map_err(|e| format!("login params: {e}"))?;
                let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
                let dto = bookclerk_plugin_source_libro::guest_login_rpc(&base, p)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::SCAN => {
                let p: ScanParams =
                    serde_json::from_value(params).map_err(|e| format!("scan params: {e}"))?;
                let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
                let dto = bookclerk_plugin_source_libro::guest_scan_rpc(&base, &p)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::FETCH_TITLE => {
                let p: FetchTitleParams = serde_json::from_value(params)
                    .map_err(|e| format!("fetch_title params: {e}"))?;
                let base = bookclerk_plugin_source_libro::resolve_base_url(&p.source_config);
                let container = bookclerk_plugin_source_libro::resolve_container(&p.source_config);
                let dto =
                    bookclerk_plugin_source_libro::guest_fetch_title_rpc(&base, &p, container)
                        .await
                        .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::SEARCH_CATALOG => {
                let p: SearchCatalogParams = serde_json::from_value(params)
                    .map_err(|e| format!("search_catalog params: {e}"))?;
                let source = bookclerk_plugin_source_libro::LibroSource::new();
                let hits = source
                    .search_catalog(&CatalogSearchOpts {
                        query: p.query,
                        region: p.region,
                        limit: p.limit,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                let dtos: Vec<_> = hits
                    .into_iter()
                    .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            methods::EXPAND_CANDIDATES => {
                let p: ExpandCandidatesParams = serde_json::from_value(params)
                    .map_err(|e| format!("expand_candidates params: {e}"))?;
                let source = bookclerk_plugin_source_libro::LibroSource::new();
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
                    .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            methods::PURCHASE_HINT => {
                let p: PurchaseHintParams = serde_json::from_value(params)
                    .map_err(|e| format!("purchase_hint params: {e}"))?;
                let source = bookclerk_plugin_source_libro::LibroSource::new();
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
                let dto = hint.map(bookclerk_plugin_source_libro::purchase_hint_to_dto);
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::LIST_DEALS => {
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let source = bookclerk_plugin_source_libro::LibroSource::new();
                let hits = source.list_deals(limit).await.map_err(|e| e.to_string())?;
                let dtos: Vec<_> = hits
                    .into_iter()
                    .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            other => Err(format!("unsupported method `{other}`")),
        }
    })
    .await?;
    Ok(())
}
