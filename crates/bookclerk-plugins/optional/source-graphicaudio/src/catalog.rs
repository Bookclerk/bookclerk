//! Public Magento catalog browse for discovery (no login required).
//!
//! Uses series pages, product pages (related titles), and catalog search.
//! Ownership filtering happens in `bookclerk-discover`.

use crate::error::{GraphicAudioError, Result};
use crate::magento::{parse_html_fragment, DEFAULT_STORE_URL};

/// Desktop Chrome User-Agent used for unauthenticated Magento catalog pages.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 Bookclerk/GraphicAudio";

/// One Magento catalog product (grid / related / product page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagentoCatalogProduct {
    /// Product Identifier.
    pub product_id: String,
    /// Sku.
    pub sku: Option<String>,
    /// Title.
    pub title: String,
    /// URL.
    pub url: Option<String>,
    /// Series.
    pub series: Option<String>,
    /// Cover URL.
    pub cover_url: Option<String>,
}

impl MagentoCatalogProduct {
    /// Is series set.
    #[must_use]
    pub fn is_series_set(&self) -> bool {
        let sku = self.sku.as_deref().unwrap_or("");
        let title = self.title.to_ascii_lowercase();
        sku.to_ascii_uppercase().contains("-SET")
            || title.contains("series set")
            || title.ends_with(" set")
    }
}

/// Shared HTTP client for public Magento catalog pages.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub fn catalog_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::new())
}

/// Fetch HTML from an absolute Magento URL.
pub async fn fetch_catalog_html(http: &reqwest::Client, url: &str) -> Result<String> {
    let resp = http
        .get(url)
        .header("user-agent", BROWSER_UA)
        .send()
        .await?;
    let status = resp.status();
    let html = resp.text().await?;
    if !status.is_success() {
        return Err(GraphicAudioError::api(format!(
            "catalog fetch failed ({status}) for {url}"
        )));
    }
    Ok(html)
}

/// Magento product page by numeric product id.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn fetch_product_by_id(
    http: &reqwest::Client,
    store_base: &str,
    product_id: &str,
) -> Result<(String, Vec<MagentoCatalogProduct>, Option<String>)> {
    let base = store_base.trim_end_matches('/');
    let url = format!("{base}/catalog/product/view/id/{product_id}");
    let html = fetch_catalog_html(http, &url).await?;
    let related = parse_related_products(&html);
    let series_url = extract_series_page_url(&html, base);
    Ok((url, related, series_url))
}

/// Search Magento catalog; often redirects to a matching series page.
///
/// `page` > 1 tries Magento `?p=` once; when that yields nothing the source is
/// treated as exhausted by the host cursor (empty page).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn search_catalog(
    http: &reqwest::Client,
    store_base: &str,
    query: &str,
) -> Result<Vec<MagentoCatalogProduct>> {
    search_catalog_page(http, store_base, query, 1).await
}

/// Like [`search_catalog`] with an optional Magento page index.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn search_catalog_page(
    http: &reqwest::Client,
    store_base: &str,
    query: &str,
    page: u32,
) -> Result<Vec<MagentoCatalogProduct>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let page = page.max(1);
    let base = store_base.trim_end_matches('/');
    let url = if page <= 1 {
        format!(
            "{base}/catalogsearch/result/?q={}",
            urlencoding_minimal(query)
        )
    } else {
        format!(
            "{base}/catalogsearch/result/?q={}&p={}",
            urlencoding_minimal(query),
            page
        )
    };
    let html = fetch_catalog_html(http, &url).await?;
    let series_name = extract_series_name_from_page(&html);
    let mut products = parse_catalog_grid(&html);
    if products.is_empty() {
        if page > 1 {
            // Magento `p` unsupported or past the end — signal exhaustion.
            return Ok(Vec::new());
        }
        // Search may land on a product page — pull related + primary.
        if let Some(primary) = parse_primary_product(&html, &url) {
            products.push(primary);
        }
        products.extend(parse_related_products(&html));
    }
    if let Some(series) = series_name {
        for p in &mut products {
            if p.series.is_none() {
                p.series = Some(series.clone());
            }
        }
    }
    Ok(products)
}

/// Fetch titles listed on a series category page (includes series-set SKUs).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn fetch_series_page(
    http: &reqwest::Client,
    series_url: &str,
) -> Result<Vec<MagentoCatalogProduct>> {
    let html = fetch_catalog_html(http, series_url).await?;
    let series_name = extract_series_name_from_page(&html);
    let mut products = parse_catalog_grid(&html);
    if let Some(series) = series_name {
        for p in &mut products {
            p.series = Some(series.clone());
        }
    }
    Ok(products)
}

/// Expand a owned Magento product id into related + series siblings.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn expand_from_product_id(
    http: &reqwest::Client,
    store_base: Option<&str>,
    product_id: &str,
) -> Result<Vec<MagentoCatalogProduct>> {
    let base = store_base.unwrap_or(DEFAULT_STORE_URL);
    let (_url, related, series_url) = fetch_product_by_id(http, base, product_id).await?;
    let mut by_id = std::collections::HashMap::new();
    for p in related {
        by_id.entry(p.product_id.clone()).or_insert(p);
    }
    if let Some(series_url) = series_url {
        for p in fetch_series_page(http, &series_url).await? {
            by_id.entry(p.product_id.clone()).or_insert(p);
        }
    }
    Ok(by_id.into_values().collect())
}

/// Expand via Magento catalog search (series name or title).
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn expand_from_search(
    http: &reqwest::Client,
    store_base: Option<&str>,
    query: &str,
) -> Result<Vec<MagentoCatalogProduct>> {
    let base = store_base.unwrap_or(DEFAULT_STORE_URL);
    search_catalog(http, base, query).await
}

/// Parse Magento product grid / related list items.
#[must_use]
pub fn parse_catalog_grid(html: &str) -> Vec<MagentoCatalogProduct> {
    let document = parse_html_fragment(html);
    let Ok(item_sel) = scraper::Selector::parse("li.item.product.product-item, li.product-item")
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for item in document.select(&item_sel) {
        let sku = item
            .value()
            .attr("data-sku")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let product_id = extract_product_id_from_element(item).or_else(|| {
            // Fall back to numeric id embedded in image container class.
            item.html().find("product-image-container-").and_then(|_| {
                let s = item.html();
                let marker = "product-image-container-";
                let idx = s.find(marker)?;
                let rest = &s[idx + marker.len()..];
                let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if id.is_empty() {
                    None
                } else {
                    Some(id)
                }
            })
        });
        let Some(product_id) = product_id else {
            continue;
        };
        if !seen.insert(product_id.clone()) {
            continue;
        }
        let title = extract_item_title(item).unwrap_or_else(|| {
            sku.clone()
                .unwrap_or_else(|| format!("GraphicAudio {product_id}"))
        });
        let url = extract_item_url(item);
        let cover_url = extract_item_cover_url(item);
        out.push(MagentoCatalogProduct {
            product_id,
            sku,
            title,
            url,
            series: None,
            cover_url,
        });
    }
    out
}

/// Parse the Recommended / related products block on a product page.
#[must_use]
pub fn parse_related_products(html: &str) -> Vec<MagentoCatalogProduct> {
    let document = parse_html_fragment(html);
    let Ok(block_sel) =
        scraper::Selector::parse(".products-related, #catalog\\.product\\.related, .block.related")
    else {
        return parse_catalog_grid(html);
    };
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for block in document.select(&block_sel) {
        for p in parse_catalog_grid(&block.html()) {
            if seen.insert(p.product_id.clone()) {
                out.push(p);
            }
        }
    }
    if out.is_empty() {
        // Fallback: whole-page grid parse (series pages).
        return parse_catalog_grid(html);
    }
    out
}

/// Extracts the product-page id, SKU, and title; missing id returns `None`.
fn parse_primary_product(html: &str, page_url: &str) -> Option<MagentoCatalogProduct> {
    let document = parse_html_fragment(html);
    let sku = document
        .select(&scraper::Selector::parse("[data-product-sku]").ok()?)
        .filter_map(|el| el.value().attr("data-product-sku"))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string);
    let product_id = document
        .select(&scraper::Selector::parse("[data-product-id]").ok()?)
        .filter_map(|el| el.value().attr("data-product-id"))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            // pageCache handles often include product id.
            let marker = "catalog_product_view_id_";
            let idx = html.find(marker)?;
            let rest = &html[idx + marker.len()..];
            let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if id.is_empty() {
                None
            } else {
                Some(id)
            }
        })?;
    let title = document
        .select(&scraper::Selector::parse("h1.page-title span, h1.page-title, .page-title").ok()?)
        .next()
        .map(|el| decode(el.text().collect::<String>().trim()))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            html.find("\"name\":\"")
                .and_then(|idx| {
                    let rest = &html[idx + 8..];
                    let end = rest.find('"')?;
                    Some(decode(&rest[..end]))
                })
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| format!("GraphicAudio {product_id}"));
    Some(MagentoCatalogProduct {
        product_id,
        sku,
        title,
        url: Some(page_url.to_string()),
        series: None,
        cover_url: None,
    })
}

/// First deep `/our-productions/series/` link, skipping the A–E index pages.
fn extract_series_page_url(html: &str, store_base: &str) -> Option<String> {
    let document = parse_html_fragment(html);
    let Ok(sel) = scraper::Selector::parse(r#"a[href*="/our-productions/series/"]"#) else {
        return None;
    };
    for a in document.select(&sel) {
        let Some(href) = a
            .value()
            .attr("href")
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        // Prefer deep series pages over the A-E index.
        if href.matches('/').count() < 5 && !href.ends_with(".html") {
            continue;
        }
        if href.ends_with("/series.html") || href.contains("/series/a-e.html") {
            continue;
        }
        if href.starts_with("http") {
            return Some(href.to_string());
        }
        return Some(format!(
            "{}/{}",
            store_base.trim_end_matches('/'),
            href.trim_start_matches('/')
        ));
    }
    None
}

/// Series page `<h1 class="page-title">` text after HTML-entity decode.
fn extract_series_name_from_page(html: &str) -> Option<String> {
    let document = parse_html_fragment(html);
    let Ok(sel) = scraper::Selector::parse("h1.page-title span, h1.page-title") else {
        return None;
    };
    document
        .select(&sel)
        .next()
        .map(|el| decode(el.text().collect::<String>().trim()))
        .filter(|s| !s.is_empty())
}

/// Magento `data-product-id` on the element or a descendant.
fn extract_product_id_from_element(el: scraper::ElementRef<'_>) -> Option<String> {
    if let Some(id) = el
        .value()
        .attr("data-product-id")
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return Some(id.to_string());
    }
    let Ok(sel) = scraper::Selector::parse("[data-product-id]") else {
        return None;
    };
    el.select(&sel)
        .filter_map(|child| child.value().attr("data-product-id"))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

/// Product-card title from name CSS, or a base64 `data-product-name` fallback.
fn extract_item_title(el: scraper::ElementRef<'_>) -> Option<String> {
    let Ok(sel) = scraper::Selector::parse(".product-item-name, .product.name a, h2.product-name")
    else {
        return None;
    };
    el.select(&sel)
        .next()
        .map(|n| decode(n.text().collect::<String>().trim()))
        .filter(|s| !s.is_empty())
        .or_else(|| {
            // Magento sometimes base64-encodes the name in data-product-name.
            let b64 = el
                .select(&scraper::Selector::parse("[data-product-name]").ok()?)
                .filter_map(|child| child.value().attr("data-product-name"))
                .map(str::trim)
                .find(|s| !s.is_empty())?;
            let bytes = decode_base64(b64)?;
            let s = String::from_utf8(bytes).ok()?;
            let s = decode(s.trim());
            if s.is_empty() {
                None
            } else {
                Some(s)
            }
        })
}

/// Decodes standard base64, skipping non-alphabet bytes; padding ends the stream.
fn decode_base64(input: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut buf = 0u32;
    let mut bits = 0i32;
    for &b in input.as_bytes() {
        if b == b'=' {
            break;
        }
        let Some(v) = val(b) else {
            continue;
        };
        buf = (buf << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// Product-card href from Magento product-link or photo anchors.
fn extract_item_url(el: scraper::ElementRef<'_>) -> Option<String> {
    let Ok(sel) = scraper::Selector::parse("a.product, a.product-item-link, a.product-item-photo")
    else {
        return None;
    };
    el.select(&sel)
        .filter_map(|a| a.value().attr("href"))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

/// Product-card cover URL, skipping `data:` URIs and Magento placeholders.
fn extract_item_cover_url(el: scraper::ElementRef<'_>) -> Option<String> {
    let Ok(sel) = scraper::Selector::parse(
        "img.product-image-photo, .product-item-photo img, .product-image-wrapper img, img",
    ) else {
        return None;
    };
    el.select(&sel)
        .filter_map(|img| {
            img.value()
                .attr("data-src")
                .or_else(|| img.value().attr("src"))
        })
        .map(str::trim)
        .find(|s| {
            !s.is_empty()
                && !s.starts_with("data:")
                && !s.contains("placeholder")
                && !s.contains("blank.gif")
        })
        .map(str::to_string)
}

/// Decodes HTML entities in scraped Magento text.
fn decode(s: &str) -> String {
    html_escape::decode_html_entities(s).into_owned()
}

/// Percent-encodes a query value, mapping space to `+` (Magento catalog search).
fn urlencoding_minimal(s: &str) -> String {
    let mut out = String::new();
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_series_grid_items() {
        let html = r#"
        <ol class="products list items product-items">
          <li class="item product product-item" data-sku="REDRISING0101">
            <a href="https://www.graphicaudio.net/red-rising-1.html" class="product">
              <h2 class="product name product-item-name">Red Rising Saga 1: Red Rising 1 of 2</h2>
            </a>
            <div class="price" data-product-id="7323"></div>
          </li>
          <li class="item product product-item" data-sku="REDRISING-SET">
            <a href="https://www.graphicaudio.net/set.html" class="product">
              <h2 class="product name product-item-name">Red Rising Saga (Series Set)</h2>
            </a>
            <div class="price" data-product-id="7587"></div>
          </li>
        </ol>
        "#;
        let products = parse_catalog_grid(html);
        assert_eq!(products.len(), 2);
        assert_eq!(products[0].product_id, "7323");
        assert_eq!(products[0].title, "Red Rising Saga 1: Red Rising 1 of 2");
        assert!(products[1].is_series_set());
    }
}
