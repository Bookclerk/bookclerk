//! External Libro.fm source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::v2::{
    decode_json, encode_json, ContentSource as ContentSourceRole, ContentSourceContext, HealthOk,
    PluginDescribe, PluginRoot, ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{
    serve, BrandDto, CatalogDetailParams, CatalogHitDto, ConfigOptionDto, ConfigOptionValueDto,
    ExpandCandidatesParams, FetchTitleParams, HandshakeResult, ListDealsParams, LoginParams,
    PluginError, PurchaseHintParams, ScanParams, SearchCatalogParams,
};
use bookclerk_source::{
    CatalogSearchOpts, CatalogSearchSort, ContentSource, ExpandSeed, PurchaseHintOpts,
};
use serde_json::Value;

fn describe_metadata() -> Result<String, PluginError> {
    encode_json(HandshakeResult {
        api_version: PRODUCT_API_VERSION,
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

fn catalog_opts(params: SearchCatalogParams) -> CatalogSearchOpts {
    CatalogSearchOpts {
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
    }
}

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
            metadata_json: describe_metadata()?,
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

#[async_trait(?Send)]
impl ContentSourceRole for LibroContentSource {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "libro source plugin ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<String, PluginError> {
        encode_json(vec!["libro plugin diagnose: ok"])
    }

    async fn login(&self, params_json: &str) -> Result<String, PluginError> {
        let params: LoginParams = decode_json(params_json)?;
        let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
        encode_json(
            bookclerk_plugin_source_libro::guest_login_rpc(&base, params)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn scan(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ScanParams = decode_json(params_json)?;
        let base = bookclerk_plugin_source_libro::resolve_base_url(&Value::Null);
        encode_json(
            bookclerk_plugin_source_libro::guest_scan_rpc(&base, &params)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn fetch_title(&self, params_json: &str) -> Result<String, PluginError> {
        let params: FetchTitleParams = decode_json(params_json)?;
        let base = bookclerk_plugin_source_libro::resolve_base_url(&params.source_config);
        let container = bookclerk_plugin_source_libro::resolve_container(&params.source_config);
        encode_json(
            bookclerk_plugin_source_libro::guest_fetch_title_rpc(&base, &params, container)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn search_catalog(&self, params_json: &str) -> Result<String, PluginError> {
        let params: SearchCatalogParams = decode_json(params_json)?;
        let source = bookclerk_plugin_source_libro::LibroSource::new();
        let hits = source
            .search_catalog(&catalog_opts(params))
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }

    async fn catalog_detail(&self, params_json: &str) -> Result<String, PluginError> {
        let params: CatalogDetailParams = decode_json(params_json)?;
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
        encode_json(hit.map(bookclerk_plugin_source_libro::catalog_hit_to_dto))
    }

    async fn expand_candidates(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ExpandCandidatesParams = decode_json(params_json)?;
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
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }

    async fn purchase_hint(&self, params_json: &str) -> Result<String, PluginError> {
        let params: PurchaseHintParams = decode_json(params_json)?;
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
        encode_json(hint.map(bookclerk_plugin_source_libro::purchase_hint_to_dto))
    }

    async fn list_deals(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ListDealsParams = decode_json(params_json)?;
        let limit = params.limit.unwrap_or(20);
        let source = bookclerk_plugin_source_libro::LibroSource::new();
        let hits = source
            .list_deals(limit)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_libro::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(LibroRoot).await?;
    Ok(())
}
