//! External Audible source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::v2::{
    decode_json, encode_json, ContentSource as ContentSourceRole, ContentSourceContext, HealthOk,
    PluginDescribe, PluginRoot, ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{
    serve, BrandDto, CatalogHitDto, ConfigOptionDto, ConfigOptionValueDto, ExpandCandidatesParams,
    FetchTitleParams, HandshakeResult, ListDealsParams, LoginCompleteParams, LoginStartParams,
    PluginError, PurchaseHintParams, ScanParams, SearchCatalogParams,
};
use bookclerk_source::{
    CatalogSearchOpts, CatalogSearchSort, ContentSource, ExpandSeed, PurchaseHintOpts,
};

fn describe_metadata() -> Result<String, PluginError> {
    encode_json(HandshakeResult {
        api_version: PRODUCT_API_VERSION,
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

/// Audible source guest; OAuth login uses the host-owned callback tunnel.
struct AudibleRoot;

#[async_trait(?Send)]
impl PluginRoot for AudibleRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "audible".into(),
            kind: "source".into(),
            display_name: Some("Audible".into()),
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
        Ok(Box::new(AudibleContentSource))
    }
}

struct AudibleContentSource;

#[async_trait(?Send)]
impl ContentSourceRole for AudibleContentSource {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "audible source plugin ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<String, PluginError> {
        encode_json(vec!["audible plugin diagnose: ok"])
    }

    async fn login_start(&self, params_json: &str) -> Result<String, PluginError> {
        let params: LoginStartParams = decode_json(params_json)?;
        let (session_id, url) = bookclerk_plugin_source_audible::guest_login_start(&params)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(bookclerk_plugin_sdk::LoginStartResultDto { session_id, url })
    }

    async fn login_complete(&self, params_json: &str) -> Result<String, PluginError> {
        let params: LoginCompleteParams = decode_json(params_json)?;
        encode_json(
            bookclerk_plugin_source_audible::guest_login_complete(&params.session_id)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn scan(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ScanParams = decode_json(params_json)?;
        encode_json(
            bookclerk_plugin_source_audible::guest_scan(
                &params.credentials,
                &params.accounts,
                params.page_size,
                params.import_episodes,
                params.import_plus_titles,
            )
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn fetch_title(&self, params_json: &str) -> Result<String, PluginError> {
        let params: FetchTitleParams = decode_json(params_json)?;
        let work_dir = bookclerk_plugin_sdk::fetch_work_dir(&params)
            .map_err(|e| PluginError::internal(e.to_string()))?;
        let creds = params
            .credentials
            .ok_or_else(|| PluginError::invalid_params("fetchTitle requires host credentials"))?;
        encode_json(
            bookclerk_plugin_source_audible::guest_fetch_title(
                &creds,
                &params.title_id,
                &work_dir,
                &params.source_config,
                &params.download,
            )
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn search_catalog(&self, params_json: &str) -> Result<String, PluginError> {
        let params: SearchCatalogParams = decode_json(params_json)?;
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        let hits = source
            .search_catalog(&catalog_opts(params))
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }

    async fn expand_candidates(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ExpandCandidatesParams = decode_json(params_json)?;
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
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }

    async fn purchase_hint(&self, params_json: &str) -> Result<String, PluginError> {
        let params: PurchaseHintParams = decode_json(params_json)?;
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
        encode_json(hint.map(bookclerk_plugin_source_audible::purchase_hint_to_dto))
    }

    async fn list_deals(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ListDealsParams = decode_json(params_json)?;
        let limit = params.limit.unwrap_or(20);
        let source = bookclerk_plugin_source_audible::AudibleSource::new();
        let hits = source
            .list_deals(limit)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_audible::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(AudibleRoot).await?;
    Ok(())
}
