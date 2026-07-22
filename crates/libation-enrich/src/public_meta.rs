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
        .user_agent(concat!("libation-rs/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| EnrichError::Sync(err.to_string()))
}

/// Search the public Audible catalog by title/author; returns ASINs (relevance order).
pub async fn search_catalog_asins(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
) -> Result<Vec<String>> {
    let title = title.trim();
    if title.is_empty() {
        return Ok(Vec::new());
    }
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products",
        region_tld(&region)
    );
    let mut req = http.get(&url).query(&[
        ("num_results", "10"),
        ("products_sort_by", "Relevance"),
        ("title", title),
    ]);
    if let Some(author) = author.map(str::trim).filter(|s| !s.is_empty()) {
        req = req.query(&[("author", author)]);
    }
    catalog_asins_from_response(req).await
}

/// Keyword catalog search (e.g. ISBN) — useful when title search misses a hit.
pub async fn search_catalog_keywords(
    http: &Client,
    region: &str,
    keywords: &str,
) -> Result<Vec<String>> {
    let keywords = keywords.trim();
    if keywords.is_empty() {
        return Ok(Vec::new());
    }
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products",
        region_tld(&region)
    );
    let req = http.get(&url).query(&[
        ("num_results", "10"),
        ("products_sort_by", "Relevance"),
        ("keywords", keywords),
    ]);
    catalog_asins_from_response(req).await
}

async fn catalog_asins_from_response(req: reqwest::RequestBuilder) -> Result<Vec<String>> {
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
    Ok(products
        .iter()
        .filter_map(|p| p.get("asin").and_then(Value::as_str))
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect())
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
