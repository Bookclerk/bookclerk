//! Unauthenticated Audible catalog + Audnexus metadata (AudioBookshelf-style).
//!
//! ABS uses the public `api.audible{tld}` catalog for title/author search, then
//! enriches each ASIN via `https://api.audnex.us` — no Audible login required.

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::error::{EnrichError, Result};
use crate::match_score::is_valid_asin;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Map marketplace / region code to Audible API TLD (AudioBookshelf `regionMap`).
#[must_use]
pub fn region_tld(region: &str) -> &'static str {
    match region.trim().to_ascii_lowercase().as_str() {
        "ca" => ".ca",
        "uk" | "gb" => ".co.uk",
        "au" => ".com.au",
        "fr" => ".fr",
        "de" => ".de",
        "jp" => ".co.jp",
        "it" => ".it",
        "in" => ".in",
        "es" => ".es",
        _ => ".com",
    }
}

/// Normalize a marketplace string to an Audnexus `region` query value.
#[must_use]
pub fn normalize_region(region: &str) -> String {
    match region.trim().to_ascii_lowercase().as_str() {
        "gb" => "uk".into(),
        "" => "us".into(),
        "us" | "ca" | "uk" | "au" | "fr" | "de" | "jp" | "it" | "in" | "es" => {
            region.trim().to_ascii_lowercase()
        }
        _ => "us".into(),
    }
}

/// Shared HTTP client for public metadata calls.
pub fn public_http_client() -> Result<Client> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("bookclerk/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| EnrichError::Sync(err.to_string()))
}

/// One Audible catalog product (public search hit).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogProduct {
    pub asin: String,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    /// Parent series ASIN when the catalog returns series metadata.
    pub series_asin: Option<String>,
    /// Series sequence / position when present (string form from Audible).
    pub series_sequence: Option<String>,
}

/// Search the public Audible catalog by title/author; returns ASINs (relevance order).
pub async fn search_catalog_asins(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
) -> Result<Vec<String>> {
    Ok(search_catalog_products(http, region, title, author, None)
        .await?
        .into_iter()
        .map(|p| p.asin)
        .collect())
}

/// Keyword catalog search (e.g. ISBN) — useful when title search misses a hit.
pub async fn search_catalog_keywords(
    http: &Client,
    region: &str,
    keywords: &str,
) -> Result<Vec<String>> {
    Ok(
        search_catalog_products(http, region, "", None, Some(keywords))
            .await?
            .into_iter()
            .map(|p| p.asin)
            .collect(),
    )
}

/// Response groups that include product titles (`product_desc`) for public search.
const CATALOG_RESPONSE_GROUPS: &str =
    "product_attrs,product_desc,contributors,series,product_extended_attrs,media";

/// Public Audible catalog search returning product metadata (not just ASINs).
///
/// Pass `title` and/or `author` and/or `keywords`. Empty title is allowed when
/// keywords or author alone are set.
pub async fn search_catalog_products(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
    keywords: Option<&str>,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex(http, region, title, author, keywords, None, None).await
}

/// Public catalog search with optional narrator and/or series ASIN filters.
pub async fn search_catalog_products_ex(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
    keywords: Option<&str>,
    narrator: Option<&str>,
    series_asin: Option<&str>,
) -> Result<Vec<CatalogProduct>> {
    let title = title.trim();
    let author = author.map(str::trim).filter(|s| !s.is_empty());
    let keywords = keywords.map(str::trim).filter(|s| !s.is_empty());
    let narrator = narrator.map(str::trim).filter(|s| !s.is_empty());
    let series_asin = series_asin.map(str::trim).filter(|s| !s.is_empty());
    if title.is_empty()
        && author.is_none()
        && keywords.is_none()
        && narrator.is_none()
        && series_asin.is_none()
    {
        return Ok(Vec::new());
    }
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products",
        region_tld(&region)
    );
    let mut req = http.get(&url).query(&[
        ("num_results", "15"),
        ("products_sort_by", "Relevance"),
        ("response_groups", CATALOG_RESPONSE_GROUPS),
    ]);
    if !title.is_empty() {
        req = req.query(&[("title", title)]);
    }
    if let Some(author) = author {
        req = req.query(&[("author", author)]);
    }
    if let Some(keywords) = keywords {
        req = req.query(&[("keywords", keywords)]);
    }
    if let Some(narrator) = narrator {
        req = req.query(&[("narrator", narrator)]);
    }
    if let Some(series_asin) = series_asin {
        req = req.query(&[("series_asin", series_asin)]);
    }
    catalog_products_from_response(req).await
}

/// List products in an Audible series by parent series ASIN (public catalog).
pub async fn search_catalog_by_series_asin(
    http: &Client,
    region: &str,
    series_asin: &str,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex(http, region, "", None, None, None, Some(series_asin)).await
}

/// Narrator-focused public catalog search.
pub async fn search_catalog_by_narrator(
    http: &Client,
    region: &str,
    narrator: &str,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex(http, region, "", None, None, Some(narrator), None).await
}

async fn catalog_products_from_response(
    req: reqwest::RequestBuilder,
) -> Result<Vec<CatalogProduct>> {
    let response = req
        .send()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?
        .error_for_status()
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    let body: Value = response
        .json()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    let Some(products) = body.get("products").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    Ok(products.iter().filter_map(parse_catalog_product).collect())
}

fn parse_catalog_product(p: &Value) -> Option<CatalogProduct> {
    let asin = p
        .get("asin")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())?;
    // Public catalog sometimes omits `title` unless `product_desc` is requested;
    // fall back to publication_name / subtitle so callers still get a label.
    let title = p
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            p.get("publication_name")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string);
    let authors = join_named_people(p.get("authors"));
    let narrators = join_named_people(p.get("narrators"));
    let series_obj = p
        .get("series")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first());
    let series = series_obj
        .and_then(|s| s.get("title").or_else(|| s.get("name")))
        .and_then(Value::as_str)
        .map(str::to_string);
    let series_asin = series_obj
        .and_then(|s| s.get("asin"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let series_sequence = series_obj
        .and_then(|s| s.get("sequence").or_else(|| s.get("sort")))
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        });
    Some(CatalogProduct {
        asin: asin.to_string(),
        title,
        authors,
        narrators,
        series,
        series_asin,
        series_sequence,
    })
}

fn join_named_people(value: Option<&Value>) -> Option<String> {
    let arr = value?.as_array()?;
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

/// Fetch book metadata from Audnexus (no Audible account).
pub async fn fetch_audnexus_book(http: &Client, asin: &str, region: &str) -> Result<Option<Value>> {
    let asin = asin.trim().to_ascii_uppercase();
    if !is_valid_asin(&asin) {
        return Ok(None);
    }
    let region = normalize_region(region);
    let url = format!("https://api.audnex.us/books/{asin}");
    let response = http
        .get(&url)
        .query(&[("region", region.as_str())])
        .send()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    let body: Value = response
        .json()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    if body.get("asin").and_then(Value::as_str).is_none() {
        return Ok(None);
    }
    Ok(Some(body))
}

/// Fetch chapter metadata from Audnexus and normalize to Audible `chapter_info` shape.
///
/// Audnexus uses camelCase (`startOffsetMs`, `brandIntroDurationMs`). Callers that
/// already accept both casings can use the raw body; this helper also mirrors
/// snake_case keys for code that only reads Audible API field names.
pub async fn fetch_audnexus_chapters(
    http: &Client,
    asin: &str,
    region: &str,
) -> Result<Option<Value>> {
    let asin = asin.trim().to_ascii_uppercase();
    if !is_valid_asin(&asin) {
        return Ok(None);
    }
    let region = normalize_region(region);
    let url = format!("https://api.audnex.us/books/{asin}/chapters");
    let response = http
        .get(&url)
        .query(&[("region", region.as_str())])
        .send()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let response = response
        .error_for_status()
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    let mut body: Value = response
        .json()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    normalize_chapter_info_casings(&mut body);
    Ok(Some(body))
}

/// Convenience: public chapter fetch with a fresh HTTP client.
pub async fn fetch_public_chapter_info(asin: &str, region: &str) -> Result<Option<Value>> {
    let http = public_http_client()?;
    fetch_audnexus_chapters(&http, asin, region).await
}

fn normalize_chapter_info_casings(info: &mut Value) {
    let Some(obj) = info.as_object_mut() else {
        return;
    };
    mirror_u64(obj, "brandIntroDurationMs", "brand_intro_duration_ms");
    mirror_u64(obj, "brandOutroDurationMs", "brand_outro_duration_ms");
    mirror_u64(obj, "runtimeLengthMs", "runtime_length_ms");
    if let Some(chapters) = obj.get_mut("chapters").and_then(Value::as_array_mut) {
        for chapter in chapters.iter_mut() {
            normalize_chapter_node(chapter);
        }
    }
}

fn normalize_chapter_node(node: &mut Value) {
    let Some(obj) = node.as_object_mut() else {
        return;
    };
    mirror_u64(obj, "startOffsetMs", "start_offset_ms");
    mirror_u64(obj, "lengthMs", "length_ms");
    if let Some(nested) = obj.get_mut("chapters").and_then(Value::as_array_mut) {
        for child in nested.iter_mut() {
            normalize_chapter_node(child);
        }
    }
}

fn mirror_u64(obj: &mut serde_json::Map<String, Value>, camel: &str, snake: &str) {
    if obj.contains_key(snake) {
        return;
    }
    if let Some(v) = obj.get(camel).cloned() {
        obj.insert(snake.to_string(), v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_tld_map() {
        assert_eq!(region_tld("us"), ".com");
        assert_eq!(region_tld("uk"), ".co.uk");
        assert_eq!(region_tld("jp"), ".co.jp");
    }

    #[test]
    fn normalize_chapters_mirrors_snake_case() {
        let mut info = serde_json::json!({
            "brandIntroDurationMs": 1904,
            "brandOutroDurationMs": 4969,
            "runtimeLengthMs": 1000,
            "chapters": [{"title": "Opening", "startOffsetMs": 0, "lengthMs": 10}]
        });
        normalize_chapter_info_casings(&mut info);
        assert_eq!(info["brand_intro_duration_ms"], 1904);
        assert_eq!(info["chapters"][0]["start_offset_ms"], 0);
    }
}
