//! External Chirp source plugin for Bookclerk.

#![allow(clippy::missing_docs_in_private_items)]

use async_trait::async_trait;
use bookclerk_plugin_sdk::v2::{
    decode_json, encode_json, ContentSource as ContentSourceRole, ContentSourceContext, HealthOk,
    PluginDescribe, PluginRoot, ScalarLimits, FEATURE_SCALAR_LIMITS, PRODUCT_API_VERSION,
};
use bookclerk_plugin_sdk::{
    serve, BrandDto, CatalogHitDto, ExpandCandidatesParams, FetchTitleParams, HandshakeResult,
    ListDealsParams, LoginParams, PluginError, PurchaseHintParams, ScanParams, SearchCatalogParams,
};
use bookclerk_source::{
    CatalogSearchOpts, CatalogSearchSort, ContentSource, ExpandSeed, PurchaseHintOpts,
};
use serde_json::Value;

fn describe_metadata() -> Result<String, PluginError> {
    encode_json(HandshakeResult {
        api_version: PRODUCT_API_VERSION,
        id: "chirp".into(),
        kind: "source".into(),
        display_name: Some("Chirp".into()),
        capabilities: vec![
            "health".into(),
            "diagnose".into(),
            "login".into(),
            "scan".into(),
            "fetchTitle".into(),
            "searchCatalog".into(),
            "expandCandidates".into(),
            "purchaseHint".into(),
            "listDeals".into(),
        ],
        portal_auth_mode: Some("password".into()),
        password_env_var: Some(bookclerk_plugin_source_chirp::PASSWORD_ENV.into()),
        sort_key: Some(3),
        brand: Some(BrandDto {
            id: "chirp".into(),
            name: "Chirp".into(),
            bg: "#E85D04".into(),
            fg: "#FFFFFF".into(),
            accent: "#F48C06".into(),
            icon_url: "https://www.google.com/s2/favicons?domain=chirpbooks.com&sz=128".into(),
        }),
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

/// External Chirp storefront guest (`kind = source`, password portal auth).
struct ChirpRoot;

#[async_trait(?Send)]
impl PluginRoot for ChirpRoot {
    async fn describe(&self) -> Result<PluginDescribe, PluginError> {
        Ok(PluginDescribe {
            api_version: PRODUCT_API_VERSION,
            id: "chirp".into(),
            kind: "source".into(),
            display_name: Some("Chirp".into()),
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
        Ok(Box::new(ChirpContentSource))
    }
}

struct ChirpContentSource;

#[async_trait(?Send)]
impl ContentSourceRole for ChirpContentSource {
    async fn health(&self) -> Result<HealthOk, PluginError> {
        Ok(HealthOk {
            ok: true,
            detail: "chirp source plugin ready".into(),
        })
    }

    async fn diagnose(&self) -> Result<String, PluginError> {
        encode_json(vec!["chirp plugin diagnose: ok"])
    }

    async fn login(&self, params_json: &str) -> Result<String, PluginError> {
        let params: LoginParams = decode_json(params_json)?;
        let gql = bookclerk_plugin_source_chirp::resolve_graphql_url(&Value::Null);
        encode_json(
            bookclerk_plugin_source_chirp::guest_login_rpc(&gql, params)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn scan(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ScanParams = decode_json(params_json)?;
        let gql = bookclerk_plugin_source_chirp::resolve_graphql_url(&Value::Null);
        encode_json(
            bookclerk_plugin_source_chirp::guest_scan_rpc(&gql, &params)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn fetch_title(&self, params_json: &str) -> Result<String, PluginError> {
        let params: FetchTitleParams = decode_json(params_json)?;
        let gql = bookclerk_plugin_source_chirp::resolve_graphql_url(&params.source_config);
        encode_json(
            bookclerk_plugin_source_chirp::guest_fetch_title_rpc(&gql, &params)
                .await
                .map_err(|e| PluginError::internal(e.to_string()))?,
        )
    }

    async fn search_catalog(&self, params_json: &str) -> Result<String, PluginError> {
        let params: SearchCatalogParams = decode_json(params_json)?;
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
        let hits = source
            .search_catalog(&catalog_opts(params))
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_chirp::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }

    async fn expand_candidates(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ExpandCandidatesParams = decode_json(params_json)?;
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
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
                .map(bookclerk_plugin_source_chirp::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }

    async fn purchase_hint(&self, params_json: &str) -> Result<String, PluginError> {
        let params: PurchaseHintParams = decode_json(params_json)?;
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
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
        encode_json(hint.map(bookclerk_plugin_source_chirp::purchase_hint_to_dto))
    }

    async fn list_deals(&self, params_json: &str) -> Result<String, PluginError> {
        let params: ListDealsParams = decode_json(params_json)?;
        let limit = params.limit.unwrap_or(20);
        let source = bookclerk_plugin_source_chirp::ChirpSource::new();
        let hits = source
            .list_deals(limit)
            .await
            .map_err(|e| PluginError::internal(e.to_string()))?;
        encode_json(
            hits.into_iter()
                .map(bookclerk_plugin_source_chirp::catalog_hit_to_dto)
                .collect::<Vec<CatalogHitDto>>(),
        )
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    serve(ChirpRoot).await?;
    Ok(())
}
