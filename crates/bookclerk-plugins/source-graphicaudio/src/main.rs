//! External GraphicAudio source plugin for Bookclerk.

use bookclerk_plugin_sdk::{
    methods, BrandDto, ExpandCandidatesParams, FetchTitleParams, HandshakeResult, HealthDto,
    LoginParams, PluginGuest, PurchaseHintParams, ScanParams, SearchCatalogParams,
    PLUGIN_API_VERSION,
};
use bookclerk_source::{
    CatalogSearchOpts, CatalogSearchSort, ContentSource, ExpandSeed, PurchaseHintOpts,
};
use serde_json::{json, Value};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    PluginGuest::serve(|method, params| async move {
        match method.as_str() {
            methods::HANDSHAKE => Ok(serde_json::to_value(HandshakeResult {
                api_version: PLUGIN_API_VERSION,
                id: "graphicaudio".into(),
                kind: "source".into(),
                display_name: Some("GraphicAudio".into()),
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
                password_env_var: Some(
                    bookclerk_plugin_source_graphicaudio::GA_PASSWORD_ENV.into(),
                ),
                aliases: vec!["ga".into(), "graphic-audio".into()],
                sort_key: Some(2),
                brand: Some(BrandDto {
                    id: "graphicaudio".into(),
                    name: "GraphicAudio".into(),
                    bg: "#111827".into(),
                    fg: "#F9FAFB".into(),
                    accent: "#DC2626".into(),
                    icon_url: "https://www.google.com/s2/favicons?domain=graphicaudio.com&sz=128"
                        .into(),
                }),
                config_options: vec![],
                cli: None,
            })
            .unwrap()),
            methods::HEALTH => Ok(serde_json::to_value(HealthDto {
                id: "graphicaudio".into(),
                enabled: true,
                ok: true,
                detail: Some("graphicaudio source plugin ready".into()),
            })
            .unwrap()),
            methods::DIAGNOSE => Ok(json!(["graphicaudio plugin diagnose: ok"])),
            methods::LOGIN => {
                let p: LoginParams =
                    serde_json::from_value(params).map_err(|e| format!("login params: {e}"))?;
                let cfg = Value::Null;
                let access_url =
                    bookclerk_plugin_source_graphicaudio::resolve_access_base_url(&cfg);
                let store_url = bookclerk_plugin_source_graphicaudio::resolve_store_base_url(&cfg);
                let access = bookclerk_plugin_source_graphicaudio::resolve_access(&cfg);
                let dto = bookclerk_plugin_source_graphicaudio::guest_login_rpc(
                    &access_url,
                    &store_url,
                    access,
                    p,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::SCAN => {
                let p: ScanParams =
                    serde_json::from_value(params).map_err(|e| format!("scan params: {e}"))?;
                let cfg = Value::Null;
                let access_url =
                    bookclerk_plugin_source_graphicaudio::resolve_access_base_url(&cfg);
                let store_url = bookclerk_plugin_source_graphicaudio::resolve_store_base_url(&cfg);
                let access = bookclerk_plugin_source_graphicaudio::resolve_access(&cfg);
                let dto = bookclerk_plugin_source_graphicaudio::guest_scan_rpc(
                    &access_url,
                    &store_url,
                    access,
                    None,
                    &p,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::FETCH_TITLE => {
                let p: FetchTitleParams = serde_json::from_value(params)
                    .map_err(|e| format!("fetch_title params: {e}"))?;
                let access_url =
                    bookclerk_plugin_source_graphicaudio::resolve_access_base_url(&p.source_config);
                let store_url =
                    bookclerk_plugin_source_graphicaudio::resolve_store_base_url(&p.source_config);
                let access = bookclerk_plugin_source_graphicaudio::resolve_access(&p.source_config);
                let bitrate =
                    bookclerk_plugin_source_graphicaudio::resolve_bitrate(&p.source_config);
                let container =
                    bookclerk_plugin_source_graphicaudio::resolve_container(&p.source_config);
                let dto = bookclerk_plugin_source_graphicaudio::guest_fetch_title_rpc(
                    &access_url,
                    &store_url,
                    &p,
                    access,
                    bitrate,
                    container,
                    None,
                )
                .await
                .map_err(|e| e.to_string())?;
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::SEARCH_CATALOG => {
                let p: SearchCatalogParams = serde_json::from_value(params)
                    .map_err(|e| format!("search_catalog params: {e}"))?;
                let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
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
                    .map(bookclerk_plugin_source_graphicaudio::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            methods::EXPAND_CANDIDATES => {
                let p: ExpandCandidatesParams = serde_json::from_value(params)
                    .map_err(|e| format!("expand_candidates params: {e}"))?;
                let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
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
                    .map(bookclerk_plugin_source_graphicaudio::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            methods::PURCHASE_HINT => {
                let p: PurchaseHintParams = serde_json::from_value(params)
                    .map_err(|e| format!("purchase_hint params: {e}"))?;
                let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
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
                let dto = hint.map(bookclerk_plugin_source_graphicaudio::purchase_hint_to_dto);
                Ok(serde_json::to_value(dto).unwrap())
            }
            methods::LIST_DEALS => {
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let source = bookclerk_plugin_source_graphicaudio::GraphicAudioSource::new();
                let hits = source.list_deals(limit).await.map_err(|e| e.to_string())?;
                let dtos: Vec<_> = hits
                    .into_iter()
                    .map(bookclerk_plugin_source_graphicaudio::catalog_hit_to_dto)
                    .collect();
                Ok(serde_json::to_value(dtos).unwrap())
            }
            other => Err(format!("unsupported method `{other}`")),
        }
    })
    .await?;
    Ok(())
}
