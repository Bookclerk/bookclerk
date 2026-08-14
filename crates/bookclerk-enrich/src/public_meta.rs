//! Unauthenticated Audible catalog + Audnexus metadata (AudioBookshelf-style).
//!
//! ABS uses the public `api.audible{tld}` catalog for title/author search, then
//! enriches each ASIN via `https://api.audnex.us` — no Audible login required.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use crate::error::{EnrichError, Result};
use crate::match_score::is_valid_asin;

/// Timeout for unauthenticated Audible catalog and Audnexus HTTP (15 seconds).
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// Per-region flattened Audible Genres browse nodes (categories change rarely).
static GENRE_CATEGORY_CACHE: Mutex<Option<HashMap<String, Vec<GenreCategoryNode>>>> =
    Mutex::new(None);

#[derive(Debug, Clone)]
/// Flattened Audible Genres browse node used to resolve a genre string to a category id.
struct GenreCategoryNode {
    /// Catalog category id (stringified when the API sends a number).
    id: String,
    /// Display name of this browse node.
    name: String,
    /// Root → leaf display names joined with ` / `.
    path: String,
    /// Hop count from the Genres root (path length; deeper nodes score higher).
    depth: usize,
}

/// Map marketplace / region code to Audible API TLD (AudioBookshelf `regionMap`).
///
/// # Arguments
///
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// `&'static str` result.
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
///
/// # Arguments
///
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// String result for this operation.
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
///
/// # Returns
///
/// On success, the inner `Client` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub fn public_http_client() -> Result<Client> {
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("bookclerk/", env!("CARGO_PKG_VERSION")))
        .build()
        // Prefer `{:#}` so TLS/cert-store failures (common under the guest jail)
        // include the underlying cause, not just reqwest's "builder error".
        .map_err(|err| EnrichError::Sync(format!("{err:#}")))
}

/// One Audible catalog product (public search hit).
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogProduct {
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    pub asin: String,
    /// Display title as shown on the storefront or library card.
    pub title: Option<String>,
    /// Comma-separated author names when the storefront provides them.
    pub authors: Option<String>,
    /// Comma-separated narrator names when known.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a named series.
    pub series: Option<String>,
    /// Parent series ASIN when the catalog returns series metadata.
    pub series_asin: Option<String>,
    /// Series sequence / position when present (string form from Audible).
    pub series_sequence: Option<String>,
    /// Cover URL from `product_images` when the `media` response group is present.
    pub cover_url: Option<String>,
    /// Optional subtitle when the catalog distinguishes it from the title.
    pub subtitle: Option<String>,
    /// Publisher summary / blurb (`product_extended_attrs`).
    pub description: Option<String>,
    /// Publisher imprint as reported by the catalog.
    pub publisher: Option<String>,
    /// Audiobook runtime in whole minutes when known.
    pub length_minutes: Option<i64>,
    /// Publication date string from the catalog (ISO or storefront format).
    pub published_at: Option<String>,
    /// Genre / subject labels (`;`-separated).
    pub categories: Option<String>,
    /// BCP-47 / storefront language code (`en`, `de`, …) when known.
    pub language: Option<String>,
    /// List or deal price in integer cents of [`Self::currency`].
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for price fields (`USD`, `EUR`, …).
    pub currency: Option<String>,
    /// Human-readable price string from the storefront (e.g. `$14.95`).
    pub price_label: Option<String>,
    /// Overall community rating when the `rating` response group is present.
    pub rating_overall: Option<f64>,
    /// Count of star ratings (`rating.num_ratings`) when present.
    pub rating_count: Option<i64>,
}

/// Search the public Audible catalog by title/author; returns ASINs (relevance order).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `title` - Display title.
/// * `author` - String `author` for this call.
///
/// # Returns
///
/// On success, the inner `Vec<String>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
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
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `keywords` - String `keywords` for this call.
///
/// # Returns
///
/// On success, the inner `Vec<String>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
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

/// Lean response groups for expand / enrich (identity + blurb/runtime when present).
///
/// Avoid `price` / `category_ladders` / `rating` here — those inflate every Discover
/// expand call and routinely push the feed past the daemon’s 8s API timeout.
const CATALOG_RESPONSE_GROUPS: &str =
    "product_attrs,product_desc,contributors,series,product_extended_attrs,media";

/// Richer groups for user-facing catalog search (includes list price).
const CATALOG_RESPONSE_GROUPS_RICH: &str = "product_attrs,product_desc,contributors,series,\
product_extended_attrs,media,price,category_ladders";

/// Rich groups plus community rating (heavier; use for popularity/rating sorts).
const CATALOG_RESPONSE_GROUPS_RICH_RATING: &str = "product_attrs,product_desc,contributors,series,\
product_extended_attrs,media,price,category_ladders,rating";

/// Public Audible catalog search returning product metadata (not just ASINs).
///
/// Pass `title` and/or `author` and/or `keywords`. Empty title is allowed when
/// keywords or author alone are set.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `title` - Display title.
/// * `author` - String `author` for this call.
/// * `keywords` - String `keywords` for this call.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn search_catalog_products(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
    keywords: Option<&str>,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex(http, region, title, author, keywords, None, None, false).await
}

/// Same as [`search_catalog_products`] but requests list price / category ladders.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `title` - Display title.
/// * `author` - String `author` for this call.
/// * `keywords` - String `keywords` for this call.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn search_catalog_products_rich(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
    keywords: Option<&str>,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex(http, region, title, author, keywords, None, None, true).await
}

/// Public catalog search with optional narrator, series ASIN, and/or category filters.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `title` - Display title.
/// * `author` - String `author` for this call.
/// * `keywords` - String `keywords` for this call.
/// * `narrator` - String `narrator` for this call.
/// * `series_asin` - String `series_asin` for this call.
/// * `rich` - Boolean flag `rich`.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn search_catalog_products_ex(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
    keywords: Option<&str>,
    narrator: Option<&str>,
    series_asin: Option<&str>,
    rich: bool,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex2(
        http,
        region,
        title,
        author,
        keywords,
        narrator,
        series_asin,
        None,
        rich,
        15,
    )
    .await
}

/// Like [`search_catalog_products_ex`] with optional `category_id` and result cap.
///
/// Note: Audible's public catalog ignores `language=` query params; prefer
/// local re-rank on [`CatalogProduct::language`] after fetch.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `title` - Display title.
/// * `author` - String `author` for this call.
/// * `keywords` - String `keywords` for this call.
/// * `narrator` - String `narrator` for this call.
/// * `series_asin` - String `series_asin` for this call.
/// * `category_id` - String `category_id` for this call.
/// * `rich` - Boolean flag `rich`.
/// * `num_results` - Numeric `num_results` value for this call.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn search_catalog_products_ex2(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
    keywords: Option<&str>,
    narrator: Option<&str>,
    series_asin: Option<&str>,
    category_id: Option<&str>,
    rich: bool,
    num_results: usize,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_paged(
        http,
        region,
        title,
        author,
        keywords,
        narrator,
        series_asin,
        category_id,
        1,
        "Relevance",
        rich,
        num_results,
        false,
    )
    .await
}

/// Storefront catalog search (`GET /1.0/catalog/search`) — the same index the
/// Storefront catalog search (`GET /1.0/catalog/search`) — the same index the
/// Audible website uses.
///
/// Prefer this over [`search_catalog_products_paged`] for Discover keyword browse:
/// `/catalog/products?keywords=` frequently omits titles that still resolve via
/// `/catalog/products/{asin}` (e.g. English *A Game of Thrones* `B002UZZ93G`).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `keywords` - String `keywords` for this call.
/// * `page` - Numeric `page` value for this call.
/// * `sort` - Sort field for the merged result set.
/// * `rich` - Boolean flag `rich`.
/// * `num_results` - Numeric `num_results` value for this call.
/// * `with_rating` - Boolean flag `with_rating`.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn search_catalog_storefront(
    http: &Client,
    region: &str,
    keywords: &str,
    page: u32,
    sort: &str,
    rich: bool,
    num_results: usize,
    with_rating: bool,
) -> Result<Vec<CatalogProduct>> {
    let keywords = keywords.trim();
    if keywords.is_empty() {
        return Ok(Vec::new());
    }
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/search",
        region_tld(&region)
    );
    let groups = if with_rating {
        CATALOG_RESPONSE_GROUPS_RICH_RATING
    } else if rich {
        CATALOG_RESPONSE_GROUPS_RICH
    } else {
        CATALOG_RESPONSE_GROUPS
    };
    let size = num_results.clamp(1, 50).to_string();
    let page = page.max(1).to_string();
    let sort = {
        let s = sort.trim();
        if s.is_empty() {
            "relevancerank"
        } else {
            s
        }
    };
    let req = http.get(&url).query(&[
        ("keywords", keywords),
        ("content_type", "Audiobook"),
        ("size", size.as_str()),
        ("page", page.as_str()),
        ("sort", sort),
        ("response_groups", groups),
    ]);
    catalog_products_from_response(req).await
}

/// Paged public catalog search with explicit `products_sort_by` and optional rating.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `title` - Display title.
/// * `author` - String `author` for this call.
/// * `keywords` - String `keywords` for this call.
/// * `narrator` - String `narrator` for this call.
/// * `series_asin` - String `series_asin` for this call.
/// * `category_id` - String `category_id` for this call.
/// * `page` - Numeric `page` value for this call.
/// * `products_sort_by` - String `products_sort_by` for this call.
/// * `rich` - Boolean flag `rich`.
/// * `num_results` - Numeric `num_results` value for this call.
/// * `with_rating` - Boolean flag `with_rating`.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn search_catalog_products_paged(
    http: &Client,
    region: &str,
    title: &str,
    author: Option<&str>,
    keywords: Option<&str>,
    narrator: Option<&str>,
    series_asin: Option<&str>,
    category_id: Option<&str>,
    page: u32,
    products_sort_by: &str,
    rich: bool,
    num_results: usize,
    with_rating: bool,
) -> Result<Vec<CatalogProduct>> {
    let title = title.trim();
    let author = author.map(str::trim).filter(|s| !s.is_empty());
    let keywords = keywords.map(str::trim).filter(|s| !s.is_empty());
    let narrator = narrator.map(str::trim).filter(|s| !s.is_empty());
    let series_asin = series_asin.map(str::trim).filter(|s| !s.is_empty());
    let category_id = category_id.map(str::trim).filter(|s| !s.is_empty());
    if title.is_empty()
        && author.is_none()
        && keywords.is_none()
        && narrator.is_none()
        && series_asin.is_none()
        && category_id.is_none()
    {
        return Ok(Vec::new());
    }
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products",
        region_tld(&region)
    );
    let groups = if with_rating {
        CATALOG_RESPONSE_GROUPS_RICH_RATING
    } else if rich {
        CATALOG_RESPONSE_GROUPS_RICH
    } else {
        CATALOG_RESPONSE_GROUPS
    };
    let num = num_results.clamp(1, 50).to_string();
    let page = page.max(1).to_string();
    let sort = {
        let s = products_sort_by.trim();
        if s.is_empty() {
            "Relevance"
        } else {
            s
        }
    };
    let mut req = http.get(&url).query(&[
        ("num_results", num.as_str()),
        ("page", page.as_str()),
        ("products_sort_by", sort),
        ("response_groups", groups),
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
    if let Some(category_id) = category_id {
        req = req.query(&[("category_id", category_id)]);
    }
    catalog_products_from_response(req).await
}

/// Keyword/title search, then keep products whose series name matches `series`.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `series` - Optional series name.
/// * `limit` - Maximum number of results to return.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn search_catalog_by_series_name(
    http: &Client,
    region: &str,
    series: &str,
    limit: usize,
) -> Result<Vec<CatalogProduct>> {
    let series = series.trim();
    if series.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 50);
    // `title=` + `keywords=` both surface series members well for named series.
    let products = search_catalog_products_ex2(
        http,
        region,
        series,
        None,
        Some(series),
        None,
        None,
        None,
        true,
        limit.max(15),
    )
    .await?;
    let matched: Vec<CatalogProduct> = products
        .into_iter()
        .filter(|p| {
            p.series
                .as_deref()
                .is_some_and(|s| s.eq_ignore_ascii_case(series))
        })
        .take(limit)
        .collect();
    Ok(matched)
}

/// Resolve an Audible Genres browse-node id for a category display name, then list products.
///
/// `page` is 1-based (Audible catalog convention), matching
/// [`search_catalog_products_paged`].
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `genre` - String `genre` for this call.
/// * `page` - Numeric `page` value for this call.
/// * `products_sort_by` - String `products_sort_by` for this call.
/// * `limit` - Maximum number of results to return.
/// * `with_rating` - Boolean flag `with_rating`.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
#[allow(clippy::too_many_arguments)]
pub async fn search_catalog_by_genre_name(
    http: &Client,
    region: &str,
    genre: &str,
    page: u32,
    products_sort_by: &str,
    limit: usize,
    with_rating: bool,
) -> Result<Vec<CatalogProduct>> {
    let genre = genre.trim();
    if genre.is_empty() {
        return Ok(Vec::new());
    }
    let limit = limit.clamp(1, 50);
    let page = page.max(1);
    let Some(category_id) = resolve_genre_category_id(http, region, genre).await? else {
        // Fallback: keyword search (still better than nothing for obscure labels).
        return search_catalog_products_paged(
            http,
            region,
            "",
            None,
            Some(genre),
            None,
            None,
            None,
            page,
            products_sort_by,
            true,
            limit,
            with_rating,
        )
        .await;
    };
    search_catalog_products_paged(
        http,
        region,
        "",
        None,
        None,
        None,
        None,
        Some(&category_id),
        page,
        products_sort_by,
        true,
        limit,
        with_rating,
    )
    .await
}

/// Map a genre display name to an Audible `category_id` under the Genres root.
///
/// Fetches `GET …/catalog/categories?categories_num_levels=3&root=Genres`, flattens
/// the tree, and picks the best case-insensitive name match (deeper nodes and
/// Science Fiction & Fantasy paths preferred; Children’s / Teen deprioritized).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `genre` - String `genre` for this call.
///
/// # Returns
///
/// On success, the inner `Option<String>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn resolve_genre_category_id(
    http: &Client,
    region: &str,
    genre: &str,
) -> Result<Option<String>> {
    let genre = genre.trim();
    if genre.is_empty() {
        return Ok(None);
    }
    let region = normalize_region(region);
    let nodes = genre_category_nodes(http, &region).await?;
    Ok(pick_best_genre_category(&nodes, genre).map(|n| n.id.clone()))
}

/// Returns cached flattened Genres nodes for `region`, fetching the catalog on a miss.
async fn genre_category_nodes(http: &Client, region: &str) -> Result<Vec<GenreCategoryNode>> {
    if let Ok(guard) = GENRE_CATEGORY_CACHE.lock() {
        if let Some(cache) = guard.as_ref() {
            if let Some(nodes) = cache.get(region) {
                return Ok(nodes.clone());
            }
        }
    }

    let url = format!(
        "https://api.audible{}/1.0/catalog/categories",
        region_tld(region)
    );
    let response = http
        .get(&url)
        .query(&[("categories_num_levels", "3"), ("root", "Genres")])
        .send()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?
        .error_for_status()
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    let body: Value = response
        .json()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    let mut nodes = Vec::new();
    if let Some(categories) = body.get("categories").and_then(Value::as_array) {
        flatten_genre_categories(categories, &[], &mut nodes);
    }

    if let Ok(mut guard) = GENRE_CATEGORY_CACHE.lock() {
        let cache = guard.get_or_insert_with(HashMap::new);
        cache.insert(region.to_string(), nodes.clone());
    }
    Ok(nodes)
}

/// Walks nested `categories` JSON into path-joined nodes, skipping entries without id or name.
fn flatten_genre_categories(categories: &[Value], path: &[&str], out: &mut Vec<GenreCategoryNode>) {
    for cat in categories {
        let Some(id) = cat
            .get("id")
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let Some(name) = cat
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let mut next_path: Vec<&str> = path.to_vec();
        next_path.push(name);
        out.push(GenreCategoryNode {
            id,
            name: name.to_string(),
            path: next_path.join(" / "),
            depth: next_path.len(),
        });
        if let Some(children) = cat.get("children").and_then(Value::as_array) {
            flatten_genre_categories(children, &next_path, out);
        }
    }
}

/// Picks the highest-scoring node whose name matches `genre` case-insensitively.
fn pick_best_genre_category<'a>(
    nodes: &'a [GenreCategoryNode],
    genre: &str,
) -> Option<&'a GenreCategoryNode> {
    let mut best: Option<(&GenreCategoryNode, i64)> = None;
    for node in nodes {
        if !node.name.eq_ignore_ascii_case(genre) {
            continue;
        }
        let score = genre_category_score(node);
        if best
            .as_ref()
            .is_none_or(|(_, best_score)| score > *best_score)
        {
            best = Some((node, score));
        }
    }
    best.map(|(n, _)| n)
}

/// Scores a node by depth, with a Science Fiction & Fantasy bonus and a children/YA penalty.
fn genre_category_score(node: &GenreCategoryNode) -> i64 {
    let path_lower = node.path.to_ascii_lowercase();
    // Depth is primary; SF&F bonus beats one extra depth level so "Fantasy"
    // resolves to Science Fiction & Fantasy / Fantasy over deeper Lit Fiction forks.
    let mut score = (node.depth as i64) * 100;
    if path_lower.contains("science fiction & fantasy") {
        score += 150;
    }
    if path_lower.contains("children")
        || path_lower.contains("teen")
        || path_lower.contains("young adult")
    {
        score -= 200;
    }
    score
}

/// List products in an Audible series by parent series ASIN (public catalog).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `series_asin` - String `series_asin` for this call.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn search_catalog_by_series_asin(
    http: &Client,
    region: &str,
    series_asin: &str,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex(http, region, "", None, None, None, Some(series_asin), false).await
}

/// Narrator-focused public catalog search.
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `narrator` - String `narrator` for this call.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogProduct>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn search_catalog_by_narrator(
    http: &Client,
    region: &str,
    narrator: &str,
) -> Result<Vec<CatalogProduct>> {
    search_catalog_products_ex(http, region, "", None, None, Some(narrator), None, false).await
}

/// Sends the catalog request and maps `products` into titles, dropping podcasts and unparseable rows.
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
    // Podcasts are out of Bookclerk v1 (search + Discover expand). Filter at the
    // shared parse choke so every catalog path drops them.
    Ok(products
        .iter()
        .filter(|p| !is_audible_podcast_product(p))
        .filter_map(parse_catalog_product)
        .collect())
}

/// True for Audible podcast shows / episodes (`content_type` / delivery type).
///
/// # Arguments
///
/// * `p` - `p` input for this call.
///
/// # Returns
///
/// `true` when the predicate holds.
#[must_use]
pub fn is_audible_podcast_product(p: &Value) -> bool {
    let content_type = p
        .get("content_type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    let delivery = p
        .get("content_delivery_type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(
        content_type.as_str(),
        "podcast" | "podcastepisode" | "episode"
    ) || matches!(
        delivery.as_str(),
        "podcastparent" | "podcastepisode" | "episode"
    ) || bookclerk_library::is_podcast_parent(&content_type)
        || bookclerk_library::is_episode(&content_type)
}

/// Maps one catalog JSON object onto [`CatalogProduct`]; missing ASIN yields `None`.
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
    let subtitle = p
        .get("subtitle")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let description = p
        .get("publisher_summary")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            p.get("merchandising_summary")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
        .map(str::to_string);
    let publisher = p
        .get("publisher_name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let length_minutes = p
        .get("runtime_length_min")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_f64().map(|n| n.round() as i64))
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .filter(|n| *n > 0);
    let published_at = p
        .get("release_date")
        .or_else(|| p.get("issue_date"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let language = p
        .get("language")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let categories = categories_from_catalog_product(p);
    let (price_cents, currency, price_label) = price_from_catalog_product(p);
    let rating = parse_catalog_rating(p);
    let rating_overall = rating.as_ref().and_then(|r| r.overall);
    let rating_count = rating.as_ref().and_then(|r| r.num_ratings);
    Some(CatalogProduct {
        asin: asin.to_string(),
        title,
        authors,
        narrators,
        series,
        series_asin,
        series_sequence,
        cover_url: product_cover_url(p),
        subtitle,
        description,
        publisher,
        length_minutes,
        published_at,
        categories,
        language,
        price_cents,
        currency,
        price_label,
        rating_overall,
        rating_count,
    })
}

/// Joins unique category-ladder (or thesaurus keyword) names with `; `, or `None` if empty.
fn categories_from_catalog_product(p: &Value) -> Option<String> {
    let mut names = Vec::new();
    if let Some(ladders) = p.get("category_ladders").and_then(Value::as_array) {
        for ladder in ladders {
            let Some(ladder_arr) = ladder
                .get("ladder")
                .and_then(Value::as_array)
                .or_else(|| ladder.as_array())
            else {
                continue;
            };
            for rung in ladder_arr {
                if let Some(name) = rung.get("name").and_then(Value::as_str) {
                    let name = name.trim();
                    if !name.is_empty()
                        && !names.iter().any(|n: &String| n.eq_ignore_ascii_case(name))
                    {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    if names.is_empty() {
        if let Some(keywords) = p
            .get("thesaurus_subject_keywords")
            .and_then(Value::as_array)
        {
            for kw in keywords {
                if let Some(name) = kw.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    if !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                        names.push(name.to_string());
                    }
                }
            }
        }
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join("; "))
    }
}

/// Lowest/list price as cents, currency code, and a display label (`FREE` when zero).
fn price_from_catalog_product(p: &Value) -> (Option<i64>, Option<String>, Option<String>) {
    let Some(price) = p.get("price") else {
        return (None, None, None);
    };
    let Some(lowest) = price
        .get("lowest_price")
        .or_else(|| price.get("list_price"))
    else {
        return (None, None, None);
    };
    let Some(amount) = lowest.get("base").and_then(Value::as_f64) else {
        return (None, None, None);
    };
    let currency = lowest
        .get("currency_code")
        .and_then(Value::as_str)
        .unwrap_or("USD")
        .to_string();
    let cents = (amount * 100.0).round() as i64;
    let cents = cents.max(0);
    let label = if cents == 0 {
        String::from("FREE")
    } else {
        let major = cents / 100;
        let minor = (cents % 100).unsigned_abs();
        match currency.to_ascii_uppercase().as_str() {
            "USD" | "" => format!("${major}.{minor:02}"),
            "GBP" => format!("£{major}.{minor:02}"),
            "EUR" => format!("€{major}.{minor:02}"),
            other => format!("{major}.{minor:02} {other}"),
        }
    };
    (Some(cents), Some(currency), Some(label))
}

/// Prefer a mid-size Audible `product_images` entry (keys are pixel sizes as strings).
fn product_cover_url(p: &Value) -> Option<String> {
    let images = p.get("product_images")?.as_object()?;
    const TARGET: i64 = 500;
    let mut best: Option<(i64, &str)> = None;
    for (key, value) in images {
        let Some(url) = value.as_str().map(str::trim).filter(|s| !s.is_empty()) else {
            continue;
        };
        let size = i64::from(key.parse::<u32>().unwrap_or(0));
        let score = if size == 0 {
            i64::MIN / 4
        } else {
            -(size - TARGET).abs()
        };
        if best
            .as_ref()
            .is_none_or(|(best_score, _)| score > *best_score)
        {
            best = Some((score, url));
        }
    }
    best.map(|(_, url)| url.to_string()).or_else(|| {
        images
            .values()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .find(|s| !s.is_empty())
            .map(str::to_string)
    })
}

/// Joins non-empty `name` fields from an authors/narrators array with `, `.
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

/// Community ratings from a single Audible catalog product (`response_groups=rating`).
///
/// Kept off bulk Discover catalog calls (see `CATALOG_RESPONSE_GROUPS`); use from
/// title-detail paths only.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogRating {
    /// Aggregate star rating across listeners.
    pub overall: Option<f64>,
    /// Narration / performance sub-score.
    pub performance: Option<f64>,
    /// Story / content sub-score.
    pub story: Option<f64>,
    /// Total number of ratings contributing to the averages.
    pub num_ratings: Option<i64>,
    /// Total number of written reviews when distinct from ratings.
    pub num_reviews: Option<i64>,
}

/// One customer review from Audible's public catalog reviews endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogReview {
    /// Stable identifier for this item.
    pub id: Option<String>,
    /// Display title as shown on the storefront or library card.
    pub title: Option<String>,
    /// Full review text from the storefront.
    pub body: String,
    /// Display name of the review author on the storefront.
    pub author_name: Option<String>,
    /// Overall star rating for this review when present.
    pub overall_rating: Option<i64>,
    /// Narration sub-rating for this review when present.
    pub performance_rating: Option<i64>,
    /// Story sub-rating for this review when present.
    pub story_rating: Option<i64>,
    /// When the review was submitted (storefront timestamp string).
    pub submitted_at: Option<String>,
}

/// Fetch Audible community ratings for one ASIN (public catalog; no account).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// On success, the inner `Option<CatalogRating>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn fetch_audible_catalog_rating(
    http: &Client,
    asin: &str,
    region: &str,
) -> Result<Option<CatalogRating>> {
    let asin = asin.trim().to_ascii_uppercase();
    if !is_valid_asin(&asin) {
        return Ok(None);
    }
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products/{}",
        region_tld(&region),
        asin
    );
    let response = http
        .get(&url)
        .query(&[("response_groups", "rating")])
        .send()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        tracing::debug!(
            asin = %asin,
            status = %response.status(),
            "audible catalog rating fetch non-success"
        );
        return Ok(None);
    }
    let body: Value = response
        .json()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    Ok(parse_catalog_rating(body.get("product").unwrap_or(&body)))
}

/// Audible catalog reviews sort (`MostHelpful` or `MostRecent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogReviewsSort {
    /// Sort reviews by helpfulness (storefront default).
    MostHelpful,
    /// Sort reviews by newest submission first.
    MostRecent,
}

impl CatalogReviewsSort {
    /// Returns the wire / FourCC string form of this value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MostHelpful => "MostHelpful",
            Self::MostRecent => "MostRecent",
        }
    }

    /// Parses this value from its wire string form.
    ///
    /// # Arguments
    ///
    /// * `raw` - String `raw` for this call.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    pub fn parse(raw: &str) -> Self {
        match raw.trim() {
            s if s.eq_ignore_ascii_case("MostRecent") || s.eq_ignore_ascii_case("recent") => {
                Self::MostRecent
            }
            _ => Self::MostHelpful,
        }
    }
}

/// One page of helpful Audible customer reviews (public; no account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogReviewsPage {
    /// Review rows on this page.
    pub reviews: Vec<CatalogReview>,
    /// 1-based page index for this reviews / list request.
    pub page: u32,
    /// Maximum number of items returned in this page.
    pub page_size: usize,
    /// When true, another page is available by incrementing [`Self::page`].
    pub has_more: bool,
    /// Remote sort key for reviews (`most_helpful`, `most_recent`, …).
    pub sort_by: CatalogReviewsSort,
}

/// Fetch a short list of helpful Audible customer reviews (public; no account).
///
/// Convenience wrapper around [`fetch_audible_catalog_reviews_page`] (page 1).
/// `limit` is clamped to `1..=50` (Audible's reviews endpoint max).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `limit` - Maximum number of results to return.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogReview>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn fetch_audible_catalog_reviews(
    http: &Client,
    asin: &str,
    region: &str,
    limit: usize,
) -> Result<Vec<CatalogReview>> {
    Ok(fetch_audible_catalog_reviews_page(
        http,
        asin,
        region,
        1,
        limit,
        CatalogReviewsSort::MostHelpful,
    )
    .await?
    .reviews)
}

/// Paginated Audible catalog reviews (`page` is 1-based).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `page` - Numeric `page` value for this call.
/// * `page_size` - Maximum items to return in this page.
/// * `sort_by` - `sort_by` input for this call.
///
/// # Returns
///
/// On success, the inner `CatalogReviewsPage` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn fetch_audible_catalog_reviews_page(
    http: &Client,
    asin: &str,
    region: &str,
    page: u32,
    page_size: usize,
    sort_by: CatalogReviewsSort,
) -> Result<CatalogReviewsPage> {
    let asin = asin.trim().to_ascii_uppercase();
    let page = page.max(1);
    let page_size = page_size.clamp(1, 50);
    if !is_valid_asin(&asin) {
        return Ok(CatalogReviewsPage {
            reviews: Vec::new(),
            page,
            page_size,
            has_more: false,
            sort_by,
        });
    }
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products/{}/reviews",
        region_tld(&region),
        asin
    );
    // Ask for one extra row when room remains under Audible's max so we can
    // detect another page without relying on an undocumented total count.
    let fetch_size = (page_size.saturating_add(1)).min(50);
    let num = fetch_size.to_string();
    let page_s = page.to_string();
    let sort = sort_by.as_str();
    let response = http
        .get(&url)
        .query(&[
            ("num_results", num.as_str()),
            ("page", page_s.as_str()),
            ("sort_by", sort),
        ])
        .send()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(CatalogReviewsPage {
            reviews: Vec::new(),
            page,
            page_size,
            has_more: false,
            sort_by,
        });
    }
    if !response.status().is_success() {
        tracing::debug!(
            asin = %asin,
            status = %response.status(),
            "audible catalog reviews fetch non-success"
        );
        return Ok(CatalogReviewsPage {
            reviews: Vec::new(),
            page,
            page_size,
            has_more: false,
            sort_by,
        });
    }
    let body: Value = response
        .json()
        .await
        .map_err(|err| EnrichError::Sync(err.to_string()))?;
    let mut reviews = parse_catalog_reviews(&body, fetch_size);
    let has_more = if fetch_size > page_size {
        reviews.len() > page_size
    } else {
        // Could not peek past page_size (already at Audible max).
        reviews.len() >= page_size
    };
    if reviews.len() > page_size {
        reviews.truncate(page_size);
    }
    Ok(CatalogReviewsPage {
        reviews,
        page,
        page_size,
        has_more,
        sort_by,
    })
}

/// Reads overall/performance/story averages and counts from a product `rating` object.
fn parse_catalog_rating(product: &Value) -> Option<CatalogRating> {
    let rating = product.get("rating")?;
    let overall = dist_average(rating.get("overall_distribution"));
    let performance = dist_average(rating.get("performance_distribution"));
    let story = dist_average(rating.get("story_distribution"));
    let num_ratings = rating
        .get("overall_distribution")
        .and_then(|d| d.get("num_ratings"))
        .and_then(json_i64);
    let num_reviews = rating.get("num_reviews").and_then(json_i64);
    if overall.is_none()
        && performance.is_none()
        && story.is_none()
        && num_ratings.is_none()
        && num_reviews.is_none()
    {
        return None;
    }
    Some(CatalogRating {
        overall,
        performance,
        story,
        num_ratings,
        num_reviews,
    })
}

/// Parses up to `limit` customer reviews from a catalog reviews body.
fn parse_catalog_reviews(body: &Value, limit: usize) -> Vec<CatalogReview> {
    let Some(arr) = body.get("customer_reviews").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(parse_one_catalog_review)
        .take(limit)
        .collect()
}

/// Normalize review text for clients: decode HTML entities and preserve Audible
/// guided questionnaire JSON (`[{ type, question, id, answer }, …]`) so the UI
/// can render Q&A sections. Unrelated JSON arrays are left unchanged.
///
/// # Arguments
///
/// * `body` - HTTP request body bytes.
///
/// # Returns
///
/// String result for this operation.
pub fn normalize_review_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.starts_with('[') {
        if let Ok(Value::Array(mut items)) = serde_json::from_str::<Value>(trimmed) {
            if looks_like_guided_review_array(&items) {
                for item in &mut items {
                    let Some(obj) = item.as_object_mut() else {
                        continue;
                    };
                    for key in ["type", "question", "answer"] {
                        if let Some(Value::String(s)) = obj.get(key).cloned() {
                            obj.insert(key.to_string(), Value::String(decode_html_entities(&s)));
                        }
                    }
                }
                return serde_json::to_string(&items)
                    .unwrap_or_else(|_| decode_html_entities(trimmed));
            }
        }
    }
    decode_html_entities(trimmed)
}

/// True when at least half the array items look like Audible guided Q&A objects.
fn looks_like_guided_review_array(items: &[Value]) -> bool {
    if items.is_empty() {
        return false;
    }
    let guided_hits = items
        .iter()
        .filter(|item| {
            let Some(obj) = item.as_object() else {
                return false;
            };
            obj.contains_key("answer") && (obj.contains_key("question") || obj.contains_key("type"))
        })
        .count();
    guided_hits > 0 && guided_hits * 2 >= items.len()
}

pub use bookclerk_library::decode_html_entities;

/// Maps one review JSON object, normalizing the body and skipping empty text.
fn parse_one_catalog_review(v: &Value) -> Option<CatalogReview> {
    let raw_body = v
        .get("body")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .replace('\r', "");
    let body = normalize_review_body(&raw_body);
    if body.trim().is_empty() {
        return None;
    }
    let ratings = v.get("ratings");
    Some(CatalogReview {
        id: v
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        title: v
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(decode_html_entities),
        body,
        author_name: v
            .get("author_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(decode_html_entities),
        overall_rating: ratings
            .and_then(|r| r.get("overall_rating"))
            .and_then(json_i64),
        performance_rating: ratings
            .and_then(|r| r.get("performance_rating"))
            .and_then(json_i64),
        story_rating: ratings
            .and_then(|r| r.get("story_rating"))
            .and_then(json_i64),
        submitted_at: v
            .get("submission_date")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    })
}

/// Average from a rating distribution (`average_rating` or parseable `display_average_rating`).
fn dist_average(dist: Option<&Value>) -> Option<f64> {
    let dist = dist?;
    dist.get("average_rating")
        .and_then(Value::as_f64)
        .or_else(|| {
            dist.get("display_average_rating")
                .and_then(Value::as_str)
                .and_then(|s| s.parse().ok())
        })
}

/// Coerces a JSON number to `i64`, truncating floats.
fn json_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().map(|n| n as i64))
        .or_else(|| v.as_f64().map(|n| n as i64))
}

/// Fetch book metadata from Audnexus (no Audible account).
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// On success, the inner `Option<Value>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
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
///
/// # Arguments
///
/// * `http` - `http` input for this call.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// On success, the inner `Option<Value>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
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
///
/// # Arguments
///
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// On success, the inner `Option<Value>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn fetch_public_chapter_info(asin: &str, region: &str) -> Result<Option<Value>> {
    let http = public_http_client()?;
    fetch_audnexus_chapters(&http, asin, region).await
}

/// Mirrors camelCase chapter-info duration fields onto snake_case keys used by the host.
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

/// Mirrors camelCase offsets/lengths on a chapter node and walks nested children.
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

/// Copies `camel` onto `snake` when the snake_case key is absent.
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
    fn podcast_products_are_detected() {
        let podcast = serde_json::json!({
            "asin": "B09T5BPLLD",
            "title": "History of Westeros",
            "content_type": "Podcast",
            "content_delivery_type": "PodcastParent",
            "is_listenable": false
        });
        let book = serde_json::json!({
            "asin": "B00M22DPWO",
            "title": "The World of Ice & Fire",
            "content_type": "Product",
            "content_delivery_type": "MultiPartBook",
            "is_listenable": true
        });
        let lecture = serde_json::json!({
            "asin": "B00DIATCXA",
            "title": "The Foundations of Western Civilization",
            "content_type": "Lecture",
            "content_delivery_type": "MultiPartBook",
            "is_listenable": true
        });
        assert!(is_audible_podcast_product(&podcast));
        assert!(!is_audible_podcast_product(&book));
        assert!(!is_audible_podcast_product(&lecture));
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

    #[test]
    fn pick_genre_prefers_deeper_sff_over_childrens() {
        let nodes = vec![
            GenreCategoryNode {
                id: "child".into(),
                name: "Fantasy".into(),
                path: "Children's Audiobooks / Science Fiction & Fantasy / Fantasy".into(),
                depth: 3,
            },
            GenreCategoryNode {
                id: "sff".into(),
                name: "Fantasy".into(),
                path: "Science Fiction & Fantasy / Fantasy".into(),
                depth: 2,
            },
            GenreCategoryNode {
                id: "lit".into(),
                name: "Fantasy".into(),
                path: "Literature & Fiction / Action & Adventure / Fantasy".into(),
                depth: 3,
            },
        ];
        let best = pick_best_genre_category(&nodes, "fantasy").unwrap();
        assert_eq!(best.id, "sff");
    }

    #[test]
    fn pick_genre_space_opera_unique() {
        let nodes = vec![GenreCategoryNode {
            id: "18580645011".into(),
            name: "Space Opera".into(),
            path: "Science Fiction & Fantasy / Science Fiction / Space Opera".into(),
            depth: 3,
        }];
        let best = pick_best_genre_category(&nodes, "Space Opera").unwrap();
        assert_eq!(best.id, "18580645011");
    }

    #[test]
    fn parse_audible_catalog_rating() {
        let product = serde_json::json!({
            "rating": {
                "num_reviews": 147,
                "overall_distribution": {
                    "average_rating": 4.789,
                    "num_ratings": 1719
                },
                "performance_distribution": {
                    "average_rating": 4.898
                },
                "story_distribution": {
                    "display_average_rating": "4.8"
                }
            }
        });
        let r = parse_catalog_rating(&product).unwrap();
        assert!((r.overall.unwrap() - 4.789).abs() < 0.001);
        assert!((r.performance.unwrap() - 4.898).abs() < 0.001);
        assert!((r.story.unwrap() - 4.8).abs() < 0.001);
        assert_eq!(r.num_ratings, Some(1719));
        assert_eq!(r.num_reviews, Some(147));
    }

    #[test]
    fn parse_audible_catalog_reviews() {
        let body = serde_json::json!({
            "customer_reviews": [
                {
                    "id": "abc",
                    "title": "Enjoyable if not great.",
                    "body": "Very-good series.\r<br />More text.",
                    "author_name": "Gr",
                    "ratings": {
                        "overall_rating": 4,
                        "performance_rating": 3,
                        "story_rating": 4
                    },
                    "submission_date": "2024-05-26T15:39:18Z"
                },
                {
                    "body": "   ",
                    "author_name": "empty"
                }
            ]
        });
        let reviews = parse_catalog_reviews(&body, 5);
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].id.as_deref(), Some("abc"));
        assert_eq!(reviews[0].title.as_deref(), Some("Enjoyable if not great."));
        assert!(reviews[0].body.contains("Very-good series."));
        assert!(!reviews[0].body.contains('\r'));
        assert_eq!(reviews[0].overall_rating, Some(4));
        assert_eq!(reviews[0].author_name.as_deref(), Some("Gr"));
    }

    #[test]
    fn normalize_guided_review_json_body_preserves_schema() {
        let raw = r#"[{"type":"Overall","question":"Where does it rank?","id":47,"answer":"As a fan of the game, I found the story interesting."},{"type":"Story","question":"What did you like?","id":48,"answer":"Fun worldbuilding."},{"type":"Performance","question":"Narration?","id":49,"answer":""},{"type":"Genre","question":"Genre?","id":50,"answer":"Fantasy"}]"#;
        let out = normalize_review_body(raw);
        assert!(out.starts_with('['));
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 4);
        assert_eq!(
            arr[0]["answer"],
            "As a fan of the game, I found the story interesting."
        );
        assert_eq!(arr[1]["type"], "Story");
        assert_eq!(arr[2]["answer"], "");
    }

    #[test]
    fn normalize_review_body_decodes_html_entities() {
        let raw = "I&rsquo;ve put this off because it&rsquo;s a novel.";
        assert_eq!(
            normalize_review_body(raw),
            "I’ve put this off because it’s a novel."
        );
    }

    #[test]
    fn normalize_review_body_leaves_plain_text() {
        let plain = "A normal prose review.\nSecond paragraph.";
        assert_eq!(normalize_review_body(plain), plain);
    }

    #[test]
    fn normalize_review_body_leaves_unrelated_json() {
        let raw = r#"[{"foo":1},{"bar":2}]"#;
        assert_eq!(normalize_review_body(raw), raw);
    }

    #[test]
    fn parse_catalog_review_guided_json() {
        let body = serde_json::json!({
            "customer_reviews": [{
                "title": "Too short",
                "body": "[{\"type\":\"Overall\",\"question\":\"Q\",\"id\":1,\"answer\":\"Worth a credit.\"}]",
                "author_name": "Roger",
                "ratings": { "overall_rating": 3 }
            }]
        });
        let reviews = parse_catalog_reviews(&body, 3);
        assert_eq!(reviews.len(), 1);
        assert!(reviews[0].body.starts_with('['));
        assert!(reviews[0].body.contains("Worth a credit."));
    }
}
