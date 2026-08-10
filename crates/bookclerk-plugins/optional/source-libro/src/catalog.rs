//! Public Libro.fm explore catalog helpers for Discover.
//!
//! Mirrors former `bookclerk-discover` explore HTTP (no account required).
//! This crate intentionally does **not** depend on `bookclerk-enrich`.

use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, SourcePurchaseHint,
};
use serde::Deserialize;
use serde_json::Value;

/// Browser-like UA: Libro's CDN / WAF returns 403 for bare `bookclerk/…`
/// agents on product HTML while accepting a Mozilla Chrome string.
const PUBLIC_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 ",
    "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36 bookclerk/",
    env!("CARGO_PKG_VERSION")
);

fn public_http_client() -> reqwest::Client {
    reqwest::Client::new()
}

fn public_get(http: &reqwest::Client, url: impl Into<String>) -> reqwest::RequestBuilder {
    http.get(url.into()).header("user-agent", PUBLIC_USER_AGENT)
}

/// Explore search → [`CatalogHit`]s keyed by ISBN.
///
/// Prefers the explore JSON endpoint; when that fails (Libro often returns
/// 500/403), falls back to scraping the public HTML search page.
///
/// Maps every bibliographic field the search payload already includes.
///
/// Lean HTML search hits stay sparse on purpose — Discover enriches the
/// **final page** via [`catalog_detail`] so we do not N+1 product fetches
/// inside each store search (that blew the daemon’s 12s search budget).
pub async fn search_catalog(opts: &CatalogSearchOpts) -> bookclerk_source::Result<Vec<CatalogHit>> {
    let q = opts.query.trim();
    if q.is_empty() || opts.limit == 0 {
        return Ok(Vec::new());
    }
    let http = public_http_client();
    let hits = libro_search_hits(&http, q, opts.limit, opts.page.max(1)).await?;
    Ok(hits
        .into_iter()
        .map(|mut h| {
            h.origin = String::from("search");
            h.decode_html_entities()
        })
        .collect())
}

/// Public catalog detail for one ISBN/slug (title dialogs / page enrich).
///
/// Prefers product HTML (JSON-LD + `.audiobook-genres`) — reliable under WAF —
/// and falls back to `explore/audiobook_details` when HTML misses. Explore is
/// tried second because it often 403s and wasted a round-trip before HTML.
pub async fn catalog_detail(product_id: &str) -> bookclerk_source::Result<Option<CatalogHit>> {
    let key = product_id.trim();
    let Some(key) = isbn_or_slug(key) else {
        return Ok(None);
    };
    let http = public_http_client();
    if let Some(html_hit) = libro_product_html_hit(&http, key).await? {
        let mut hit = html_hit;
        // Explore can still fill series / genres when HTML is thin.
        if hit_needs_html_extras(&hit) || !non_empty_opt(&hit.series) {
            if let Ok(Some(explore)) = libro_explore_audiobook(&http, key).await {
                hit = merge_catalog_hits(hit, explore);
            }
        }
        return Ok(Some(hit.decode_html_entities()));
    }
    Ok(libro_explore_audiobook(&http, key)
        .await?
        .map(CatalogHit::decode_html_entities))
}

fn hit_needs_html_extras(h: &CatalogHit) -> bool {
    !non_empty_opt(&h.categories) || h.is_abridged.is_none()
}

fn non_empty_opt(s: &Option<String>) -> bool {
    s.as_deref().map(str::trim).is_some_and(|v| !v.is_empty())
}

fn merge_catalog_hits(mut base: CatalogHit, fill: CatalogHit) -> CatalogHit {
    if base.title.trim().is_empty() {
        base.title = fill.title;
    }
    fill_opt(&mut base.authors, fill.authors);
    fill_opt(&mut base.narrators, fill.narrators);
    fill_opt(&mut base.series, fill.series);
    fill_opt(&mut base.series_index, fill.series_index);
    fill_opt(&mut base.asin, fill.asin);
    fill_opt(&mut base.isbn, fill.isbn);
    fill_opt(&mut base.url, fill.url);
    fill_opt(&mut base.cover_url, fill.cover_url);
    fill_opt(&mut base.subtitle, fill.subtitle);
    fill_opt(&mut base.description, fill.description);
    fill_opt(&mut base.publisher, fill.publisher);
    fill_opt(&mut base.published_at, fill.published_at);
    fill_opt(&mut base.categories, fill.categories);
    fill_opt(&mut base.language, fill.language);
    fill_opt(&mut base.currency, fill.currency);
    fill_opt(&mut base.price_label, fill.price_label);
    if base.length_minutes.is_none() {
        base.length_minutes = fill.length_minutes;
    }
    if base.price_cents.is_none() {
        base.price_cents = fill.price_cents;
    }
    if base.rating_overall.is_none() {
        base.rating_overall = fill.rating_overall;
    }
    if base.rating_count.is_none() {
        base.rating_count = fill.rating_count;
    }
    if base.is_abridged.is_none() {
        base.is_abridged = fill.is_abridged;
    }
    if base.product_id.trim().is_empty() {
        base.product_id = fill.product_id;
    }
    base
}

fn fill_opt(slot: &mut Option<String>, fill: Option<String>) {
    if !non_empty_opt(slot) {
        *slot = fill.filter(|s| !s.trim().is_empty());
    }
}

/// Expand via `explore/audiobook_details/{isbn}` → `related_audiobooks`.
pub async fn expand_candidates(
    seed: &ExpandSeed,
    limit: usize,
) -> bookclerk_source::Result<Vec<CatalogHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let isbn = seed
        .isbn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let pid = seed.product_id.trim();
            if seed.source.eq_ignore_ascii_case("libro") && !pid.is_empty() {
                Some(pid)
            } else {
                None
            }
        });
    let Some(isbn) = isbn else {
        // Fallback: explore search by title/author for non-Libro seeds.
        if seed.source.eq_ignore_ascii_case("libro") {
            return Ok(Vec::new());
        }
        let title = seed.title.trim();
        if title.is_empty() {
            return Ok(Vec::new());
        }
        let q = match primary_author(seed.authors.as_deref()) {
            Some(a) => format!("{title} {a}"),
            None => title.to_string(),
        };
        let hits = search_catalog(&CatalogSearchOpts {
            query: q,
            region: String::new(),
            limit,
            page: 1,
            sort: Default::default(),
            field: None,
            language: None,
        })
        .await?;
        return Ok(hits
            .into_iter()
            .map(|mut h| {
                h.origin = format!("libro catalog search (“{}”)", seed.title);
                h
            })
            .collect());
    };

    let http = public_http_client();
    match libro_related(&http, isbn).await {
        Ok(mut hits) => {
            for h in &mut hits {
                h.origin = format!("libro related to “{}”", seed.title);
            }
            hits.truncate(limit);
            Ok(hits)
        }
        Err(err) => {
            tracing::debug!(isbn, error = %err, "libro related lookup failed");
            Ok(Vec::new())
        }
    }
}

/// Resolve a Libro.fm purchase URL (ISBN or catalog search), optionally priced.
pub async fn purchase_hint(
    opts: &PurchaseHintOpts,
) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
    let title = opts
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let author = opts
        .authors
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    // Only treat product_id as an ISBN/slug when it looks like one — never an
    // Audible ASIN leaked from a cross-store purchase-hint call.
    // ISBN alone is not membership: Audible exclusives often ship a bibliographic
    // ISBN that 404s on libro.fm — verify the product page before advertising.
    let mut hint = if let Some(isbn) = opts
        .isbn
        .as_deref()
        .map(str::trim)
        .filter(|s| isbn_or_slug(s).is_some())
        .or_else(|| {
            opts.product_id
                .as_deref()
                .map(str::trim)
                .filter(|s| isbn_or_slug(s).is_some())
        }) {
        let key = isbn_or_slug(isbn).unwrap_or(isbn);
        if libro_product_page_ok(key).await {
            Some(SourcePurchaseHint {
                product_id: key.to_string(),
                title: title.map(str::to_string),
                url: Some(format!("https://libro.fm/audiobooks/{key}")),
                ..Default::default()
            })
        } else {
            tracing::debug!(isbn = %key, "libro ISBN/slug not found; trying title search");
            None
        }
    } else {
        None
    };

    if hint.is_none() {
        if let Some(t) = title {
            let http = public_http_client();
            hint = match libro_title_search(&http, t, author).await {
                Ok(h) => h,
                Err(err) => {
                    tracing::debug!(error = %err, "libro title search failed");
                    None
                }
            };
        }
    }

    if opts.with_price {
        if let Some(ref mut h) = hint {
            if let Some(priced) = fetch_libro_price(&h.product_id).await {
                apply_dual_price(h, &priced);
            }
        }
    }

    Ok(hint)
}

/// True when `GET /audiobooks/{id}` looks like a real product (not 404).
async fn libro_product_page_ok(isbn_or_slug: &str) -> bool {
    let http = public_http_client();
    let url = format!("https://libro.fm/audiobooks/{isbn_or_slug}");
    let resp = match public_get(&http, &url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !resp.status().is_success() {
        return false;
    }
    let html = match resp.text().await {
        Ok(t) => t,
        Err(_) => return false,
    };
    // Some storefronts return 200 with a not-found body.
    let lower = html.to_ascii_lowercase();
    if lower.contains("page not found")
        || lower.contains("we couldn't find")
        || lower.contains("we couldn&#39;t find")
        || lower.contains("audiobook not found")
    {
        return false;
    }
    true
}

#[derive(Debug, Deserialize)]
struct LibroExploreSearch {
    #[serde(default)]
    audiobook_collection: Option<LibroCollection>,
}

#[derive(Debug, Deserialize)]
struct LibroCollection {
    #[serde(default)]
    audiobooks: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct LibroBook {
    isbn: Option<String>,
    title: Option<String>,
    #[serde(default)]
    authors: Option<String>,
    slug: Option<String>,
    #[serde(default)]
    series: Option<LibroSeriesField>,
    #[serde(default)]
    series_num: Option<Value>,
    #[serde(default, alias = "coverUrl", alias = "image_url", alias = "cover")]
    cover_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LibroSeriesField {
    Name(String),
    Object { name: Option<String> },
}

impl LibroSeriesField {
    fn name(self) -> Option<String> {
        match self {
            Self::Name(s) => Some(s).filter(|v| !v.trim().is_empty()),
            Self::Object { name } => name.filter(|v| !v.trim().is_empty()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct LibroDetailsResponse {
    #[serde(default)]
    data: Option<LibroDetailsData>,
}

#[derive(Debug, Deserialize)]
struct LibroDetailsData {
    #[serde(default)]
    audiobook: Option<Value>,
    #[serde(default)]
    related_audiobooks: Vec<Value>,
}

async fn libro_related(
    http: &reqwest::Client,
    isbn: &str,
) -> bookclerk_source::Result<Vec<CatalogHit>> {
    let url = format!("https://libro.fm/explore/audiobook_details/{isbn}");
    let resp = public_get(http, &url)
        .send()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let body: LibroDetailsResponse = resp
        .json()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(format!("libro related parse: {e}")))?;
    let related = body.data.map(|d| d.related_audiobooks).unwrap_or_default();
    Ok(related
        .iter()
        .filter_map(parse_libro_book)
        .map(|mut h| {
            h.origin = String::from("libro related");
            h
        })
        .collect())
}

async fn libro_explore_audiobook(
    http: &reqwest::Client,
    isbn_or_slug: &str,
) -> bookclerk_source::Result<Option<CatalogHit>> {
    let isbn_key = isbn_or_slug
        .split('-')
        .next()
        .filter(|s| is_isbn_digits(s))
        .unwrap_or(isbn_or_slug);
    let url = format!("https://libro.fm/explore/audiobook_details/{isbn_key}");
    let resp = public_get(http, &url)
        .send()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    if !resp.status().is_success() {
        tracing::debug!(
            isbn = isbn_key,
            status = %resp.status(),
            "libro explore audiobook_details non-success"
        );
        return Ok(None);
    }
    let body: LibroDetailsResponse = resp
        .json()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(format!("libro details parse: {e}")))?;
    let Some(audiobook) = body.data.and_then(|d| d.audiobook) else {
        return Ok(None);
    };
    Ok(parse_libro_book(&audiobook).map(|mut h| {
        h.origin = String::from("libro details");
        if h.url.is_none() {
            h.url = Some(format!("https://libro.fm/audiobooks/{isbn_or_slug}"));
        }
        h
    }))
}

async fn libro_product_html_hit(
    http: &reqwest::Client,
    isbn_or_slug: &str,
) -> bookclerk_source::Result<Option<CatalogHit>> {
    let url = format!("https://libro.fm/audiobooks/{isbn_or_slug}");
    let resp = public_get(http, &url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    if !resp.status().is_success() {
        tracing::debug!(
            isbn_or_slug,
            status = %resp.status(),
            "libro product HTML detail non-success"
        );
        return Ok(None);
    }
    let html = resp
        .text()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    Ok(parse_libro_product_html(&html, &url))
}

/// JSON-LD + `.audiobook-genres` (+ Unabridged/Abridged badge) from product HTML.
fn parse_libro_product_html(html: &str, page_url: &str) -> Option<CatalogHit> {
    let mut hit = parse_libro_json_ld_audiobook(html)?;
    hit.origin = String::from("libro product html");
    if hit.url.is_none() {
        hit.url = Some(page_url.to_string());
    }
    if let Some(genres) = parse_libro_html_genres(html) {
        hit.categories = Some(genres);
    }
    if hit.is_abridged.is_none() {
        hit.is_abridged = parse_libro_html_abridged(html);
    }
    Some(hit)
}

fn parse_libro_book(v: &Value) -> Option<CatalogHit> {
    let isbn = v
        .get("isbn")
        .and_then(|x| {
            x.as_str()
                .map(str::to_string)
                .or_else(|| x.as_i64().map(|n| n.to_string()))
                .or_else(|| x.as_u64().map(|n| n.to_string()))
        })
        .filter(|s| !s.is_empty())?;
    let title = v
        .get("title")
        .or_else(|| v.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?
        .to_string();
    let authors =
        parse_person_names(v.get("authors")).or_else(|| parse_person_names(v.get("author")));
    let narrators = parse_person_names(v.get("narrators"))
        .or_else(|| parse_person_names(v.get("readBy")))
        .or_else(|| {
            v.get("audiobook_info")
                .and_then(|info| parse_person_names(info.get("narrators")))
        });
    let series = v.get("series").and_then(|s| {
        s.as_str()
            .map(str::to_string)
            .or_else(|| s.get("name").and_then(Value::as_str).map(str::to_string))
    });
    let series_index = series_num_to_index(v.get("series_num")).or_else(|| {
        // HTML-derived related payloads sometimes use `series_number`.
        series_num_to_index(v.get("series_number"))
    });
    let cover_url = v
        .get("cover_url")
        .or_else(|| v.get("coverUrl"))
        .or_else(|| v.get("image_url"))
        .or_else(|| v.get("cover"))
        .or_else(|| v.get("image"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let subtitle = v
        .get("subtitle")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let description = v
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let publisher = parse_named_string(v.get("publisher"));
    let published_at = v
        .get("publication_date")
        .or_else(|| v.get("datePublished"))
        .or_else(|| v.get("published_at"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let categories = parse_genres(v.get("genres")).or_else(|| parse_genres(v.get("genre")));
    let language = v
        .get("language")
        .or_else(|| v.get("inLanguage"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let length_minutes = length_minutes_from_value(v);
    let slug = v
        .get("slug")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let product_id = slug.unwrap_or(isbn.as_str()).to_string();
    let url = v
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| Some(format!("https://libro.fm/audiobooks/{product_id}")));
    let (price_cents, currency, price_label) = price_fields_from_value(v);
    let is_abridged = parse_abridged_flag(v.get("abridged"));
    Some(CatalogHit {
        product_id,
        title,
        authors,
        narrators,
        series,
        series_index,
        isbn: Some(isbn),
        cover_url,
        url,
        origin: String::from("libro"),
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
        is_abridged,
        ..Default::default()
    })
}

fn parse_abridged_flag(v: Option<&Value>) -> Option<bool> {
    match v? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "abridged" => Some(true),
            "false" | "0" | "no" | "unabridged" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Genres from `<div class="audiobook-genres"><a>…</a>…</div>`.
fn parse_libro_html_genres(html: &str) -> Option<String> {
    let marker = "audiobook-genres";
    let start = html.find(marker)?;
    let after = &html[start + marker.len()..];
    let gt = after.find('>')?;
    let body = &after[gt + 1..];
    let end = body.find("</div>")?;
    let block = &body[..end];
    let mut names = Vec::new();
    let mut rest = block;
    while let Some(a_start) = rest.find("<a") {
        let after_a = &rest[a_start..];
        let Some(open_end) = after_a.find('>') else {
            break;
        };
        let inner = &after_a[open_end + 1..];
        let Some(close) = inner.find("</a>") else {
            break;
        };
        let raw = inner[..close].trim();
        let decoded = bookclerk_library::decode_html_entities(raw);
        let name = decoded.trim();
        if !name.is_empty() {
            names.push(name.to_string());
        }
        rest = &inner[close + 4..];
    }
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn parse_libro_html_abridged(html: &str) -> Option<bool> {
    let lower = html.to_ascii_lowercase();
    if lower.contains("<span>unabridged</span>")
        || lower.contains(">unabridged<")
        || lower.contains("\"abridged\": \"false\"")
        || lower.contains("\"abridged\":\"false\"")
    {
        return Some(false);
    }
    if lower.contains("<span>abridged</span>")
        || lower.contains(">abridged<")
        || lower.contains("\"abridged\": \"true\"")
        || lower.contains("\"abridged\":\"true\"")
    {
        return Some(true);
    }
    None
}

/// schema.org `Audiobook` JSON-LD embedded in the public product HTML page.
fn parse_libro_json_ld_audiobook(html: &str) -> Option<CatalogHit> {
    let marker = "application/ld+json";
    let mut search_from = 0;
    while let Some(rel) = html[search_from..].find(marker) {
        let abs = search_from + rel;
        let after = &html[abs + marker.len()..];
        let Some(gt) = after.find('>') else {
            break;
        };
        let body = &after[gt + 1..];
        let Some(end) = body.find("</script>") else {
            break;
        };
        let json_text = body[..end].trim();
        search_from = abs + marker.len() + gt + 1 + end;
        if json_text.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(json_text) else {
            continue;
        };
        for candidate in json_ld_audiobook_candidates(&v) {
            if let Some(hit) = parse_libro_book(candidate) {
                return Some(hit);
            }
            // JSON-LD uses `name` instead of `title`; parse_libro_book already
            // accepts `name`. Also map `readBy` → already handled.
        }
    }
    None
}

fn json_ld_audiobook_candidates(v: &Value) -> Vec<&Value> {
    match v {
        Value::Array(arr) => arr
            .iter()
            .filter(|x| json_ld_type_is_audiobook(x))
            .collect(),
        Value::Object(_) if json_ld_type_is_audiobook(v) => vec![v],
        Value::Object(map) => map
            .get("@graph")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter(|x| json_ld_type_is_audiobook(x))
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn json_ld_type_is_audiobook(v: &Value) -> bool {
    match v.get("@type") {
        Some(Value::String(s)) => {
            let t = s.rsplit('/').next().unwrap_or(s);
            t.eq_ignore_ascii_case("Audiobook") || t.eq_ignore_ascii_case("Book")
        }
        Some(Value::Array(arr)) => arr.iter().any(|x| {
            x.as_str().is_some_and(|s| {
                let t = s.rsplit('/').next().unwrap_or(s);
                t.eq_ignore_ascii_case("Audiobook") || t.eq_ignore_ascii_case("Book")
            })
        }),
        _ => false,
    }
}

fn parse_person_names(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    if let Some(arr) = v.as_array() {
        let names: Vec<&str> = arr
            .iter()
            .filter_map(|x| {
                x.as_str()
                    .or_else(|| x.get("name").and_then(Value::as_str))
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
            })
            .collect();
        if names.is_empty() {
            return None;
        }
        return Some(names.join(", "));
    }
    v.get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_named_string(v: Option<&Value>) -> Option<String> {
    let v = v?;
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            v.get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

fn parse_genres(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(s) = v.as_str() {
        let t = s.trim();
        return (!t.is_empty()).then(|| t.to_string());
    }
    let arr = v.as_array()?;
    let names: Vec<&str> = arr
        .iter()
        .filter_map(|x| {
            x.as_str()
                .or_else(|| x.get("name").and_then(Value::as_str))
                .map(str::trim)
                .filter(|s| !s.is_empty())
        })
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join(", "))
    }
}

fn length_minutes_from_value(v: &Value) -> Option<i64> {
    if let Some(mins) = v
        .get("length_minutes")
        .and_then(Value::as_i64)
        .filter(|&n| n > 0)
    {
        return Some(mins);
    }
    let info = v.get("audiobook_info");
    if let Some(secs) = info
        .and_then(|i| i.get("duration"))
        .and_then(|d| d.as_u64().or_else(|| d.as_i64().map(|n| n as u64)))
        .filter(|&n| n > 0)
    {
        return Some((secs / 60) as i64);
    }
    if let Some(secs) = v
        .get("duration")
        .and_then(|d| d.as_u64().or_else(|| d.as_i64().map(|n| n as u64)))
        .filter(|&n| n > 0)
    {
        // Library API uses seconds; ISO-8601 strings are handled below.
        if secs > 1000 {
            return Some((secs / 60) as i64);
        }
    }
    v.get("duration")
        .and_then(Value::as_str)
        .and_then(parse_iso8601_duration_minutes)
}

/// Parse schema.org durations like `PT11H19M21S` → whole minutes.
fn parse_iso8601_duration_minutes(raw: &str) -> Option<i64> {
    let s = raw.trim();
    let s = s.strip_prefix("PT").or_else(|| s.strip_prefix("pt"))?;
    let mut hours = 0i64;
    let mut minutes = 0i64;
    let mut seconds = 0i64;
    let mut num = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        match c {
            'H' | 'h' => hours = n,
            'M' | 'm' => minutes = n,
            'S' | 's' => seconds = n,
            _ => return None,
        }
    }
    if !num.is_empty() {
        return None;
    }
    let total_secs = hours * 3600 + minutes * 60 + seconds;
    (total_secs > 0).then_some(total_secs / 60)
}

fn price_fields_from_value(v: &Value) -> (Option<i64>, Option<String>, Option<String>) {
    let offers = v.get("offers");
    let low = offers
        .and_then(|o| o.get("lowPrice"))
        .and_then(|p| {
            p.as_f64()
                .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
        })
        .or_else(|| {
            v.get("price").and_then(|p| {
                p.as_f64()
                    .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
            })
        });
    let currency = offers
        .and_then(|o| o.get("priceCurrency"))
        .and_then(Value::as_str)
        .or_else(|| v.get("currency").and_then(Value::as_str))
        .map(str::to_string);
    let cents = low.map(|n| (n * 100.0).round() as i64);
    let label = cents.map(|c| format_money_label(c, currency.as_deref().unwrap_or("USD")));
    (cents, currency, label)
}

async fn libro_title_search(
    http: &reqwest::Client,
    title: &str,
    author: Option<&str>,
) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
    let q = match author {
        Some(a) if !a.is_empty() => format!("{title} {a}"),
        _ => title.to_string(),
    };
    let Some(hit) = libro_search_hits(http, &q, 1, 1).await?.into_iter().next() else {
        return Ok(None);
    };
    let Some(isbn) = hit.isbn.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    // Prefer the ISBN-slug path for product_id so price HTML fetch matches the
    // storefront URL (bare ISBN also works once the UA is accepted).
    let product_id = if hit.product_id.contains('-') {
        hit.product_id
    } else {
        isbn.clone()
    };
    let url = Some(format!("https://libro.fm/audiobooks/{product_id}"));
    Ok(Some(SourcePurchaseHint {
        product_id,
        title: Some(hit.title).filter(|s| !s.is_empty()),
        url,
        ..Default::default()
    }))
}

/// Search Libro.fm: explore JSON first, then public HTML search.
async fn libro_search_hits(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
    page: u32,
) -> bookclerk_source::Result<Vec<CatalogHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let page = page.max(1);
    match libro_explore_json_search(http, query, limit, page).await {
        Ok(hits) if !hits.is_empty() => return Ok(hits),
        Ok(_) => tracing::debug!("libro explore JSON search empty; trying HTML"),
        Err(err) => tracing::debug!(error = %err, "libro explore JSON search failed; trying HTML"),
    }
    // HTML fallback has no reliable paging — only use it for page 1.
    if page > 1 {
        return Ok(Vec::new());
    }
    libro_html_search(http, query, limit).await
}

async fn libro_explore_json_search(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
    page: u32,
) -> bookclerk_source::Result<Vec<CatalogHit>> {
    let url = format!(
        "https://libro.fm/explore/search?page={}&q={}",
        page.max(1),
        urlencode_minimal(query)
    );
    let resp = public_get(http, &url)
        .send()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(bookclerk_source::SourceError::api(format!(
            "libro explore search HTTP {}",
            resp.status()
        )));
    }
    let body: LibroExploreSearch = resp
        .json()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(format!("libro explore parse: {e}")))?;
    let hits = body
        .audiobook_collection
        .map(|c| c.audiobooks)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|v| parse_libro_book(&v))
        .take(limit)
        .collect();
    Ok(hits)
}

async fn libro_html_search(
    http: &reqwest::Client,
    query: &str,
    limit: usize,
) -> bookclerk_source::Result<Vec<CatalogHit>> {
    let url = format!("https://libro.fm/search?q={}", urlencode_minimal(query));
    let resp = public_get(http, &url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), "libro HTML search non-success");
        return Ok(Vec::new());
    }
    let html = resp
        .text()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    Ok(parse_libro_search_html(&html, limit)
        .into_iter()
        .filter_map(|book| {
            let isbn = book.isbn.filter(|s| !s.is_empty())?;
            let product_id = book
                .slug
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| isbn.clone());
            Some(CatalogHit {
                product_id,
                title: book
                    .title
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| isbn.clone()),
                authors: book.authors.filter(|s| !s.is_empty()),
                series: book.series.and_then(LibroSeriesField::name),
                series_index: series_num_to_index(book.series_num.as_ref()),
                isbn: Some(isbn),
                cover_url: book.cover_url.filter(|s| !s.trim().is_empty()),
                origin: String::from("search"),
                ..Default::default()
            })
        })
        .collect())
}

/// ISBN-10/13, or an ISBN-leading storefront slug (`9781…-ashes-of-man`).
fn isbn_or_slug(raw: &str) -> Option<&str> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some((head, rest)) = s.split_once('-') {
        if is_isbn_digits(head) && !rest.is_empty() {
            return Some(s);
        }
    }
    let compact: String = s.chars().filter(|c| *c != '-').collect();
    if is_isbn_digits(&compact) {
        Some(s)
    } else {
        None
    }
}

fn is_isbn_digits(s: &str) -> bool {
    let b = s.as_bytes();
    match b.len() {
        13 => b.iter().all(|c| c.is_ascii_digit()),
        10 => {
            b[..9].iter().all(|c| c.is_ascii_digit())
                && (b[9].is_ascii_digit() || b[9].eq_ignore_ascii_case(&b'X'))
        }
        _ => false,
    }
}

/// Parse `/audiobooks/{isbn}-{slug}` results from the public HTML search page.
fn parse_libro_search_html(html: &str, limit: usize) -> Vec<LibroBook> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let marker = "/audiobooks/";
    let mut rest = html;
    while out.len() < limit {
        let Some(idx) = rest.find(marker) else {
            break;
        };
        let after = &rest[idx + marker.len()..];
        let end = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
            .unwrap_or(after.len());
        let slug = &after[..end];
        rest = &after[end..];
        let Some(isbn) = slug.split('-').next().filter(|s| is_isbn_digits(s)) else {
            continue;
        };
        if !seen.insert(isbn.to_string()) {
            continue;
        }
        let (title, authors) = title_authors_from_search_html(html, slug, isbn);
        out.push(LibroBook {
            isbn: Some(isbn.to_string()),
            title,
            authors,
            slug: Some(slug.to_string()),
            series: None,
            series_num: None,
            cover_url: Some(format!("https://covers.libro.fm/{isbn}_400.jpg")),
        });
    }
    out
}

fn title_authors_from_search_html(
    html: &str,
    slug: &str,
    isbn: &str,
) -> (Option<String>, Option<String>) {
    // Prefer alt / aria-label near this result: "View audiobook of TITLE by AUTHOR"
    // or "View audiobook TITLE By AUTHOR".
    let needles = [
        format!("/audiobooks/{slug}"),
        format!("covers.libro.fm/{isbn}"),
    ];
    for needle in needles {
        if let Some(idx) = html.find(&needle) {
            let start = idx.saturating_sub(400);
            let window = &html[start..html.len().min(idx + needle.len() + 200)];
            if let Some(pair) = parse_view_audiobook_label(window) {
                return pair;
            }
        }
    }
    (title_from_libro_slug(slug), None)
}

fn parse_view_audiobook_label(window: &str) -> Option<(Option<String>, Option<String>)> {
    for (prefix, by) in [
        ("View audiobook of ", " by "),
        ("View audiobook ", " By "),
        ("View audiobook ", " by "),
    ] {
        let lower = window.to_ascii_lowercase();
        let p = prefix.to_ascii_lowercase();
        let Some(start) = lower.find(&p) else {
            continue;
        };
        let after = &window[start + prefix.len()..];
        let end = after.find('"').or_else(|| after.find('<'))?;
        let label = after[..end].trim();
        if label.is_empty() {
            continue;
        }
        if let Some((title, author)) = label.split_once(by) {
            let title = title.trim();
            let author = author.trim();
            if !title.is_empty() {
                return Some((
                    Some(title.to_string()),
                    Some(author.to_string()).filter(|s| !s.is_empty()),
                ));
            }
        } else if !label.is_empty() {
            return Some((Some(label.to_string()), None));
        }
    }
    None
}

fn title_from_libro_slug(slug: &str) -> Option<String> {
    let rest = slug.split_once('-').map(|(_, r)| r).unwrap_or(slug);
    let title = rest.replace('-', " ");
    let title = title.trim();
    if title.is_empty() {
        None
    } else {
        // Cheap title-case for slug fallbacks.
        Some(
            title
                .split_whitespace()
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" "),
        )
    }
}

struct DualPriced {
    currency: String,
    list_cents: Option<i64>,
    list_label: Option<String>,
    member_cents: Option<i64>,
    member_label: Option<String>,
}

fn apply_dual_price(hint: &mut SourcePurchaseHint, priced: &DualPriced) {
    hint.currency = Some(priced.currency.clone());
    hint.list_price_cents = priced.list_cents;
    hint.list_price_label = priced.list_label.clone();
    hint.member_price_cents = priced.member_cents;
    hint.member_price_label = priced.member_label.clone();
    hint.price_cents = priced.member_cents.or(priced.list_cents);
    hint.price_label = priced
        .member_label
        .clone()
        .or_else(|| priced.list_label.clone());
}

async fn fetch_libro_price(isbn_or_slug: &str) -> Option<DualPriced> {
    let http = public_http_client();
    // Product HTML is more reliable than explore JSON (often 403/500) and
    // exposes both member and non-member prices in the price sidebar / JSON-LD.
    let url = format!("https://libro.fm/audiobooks/{isbn_or_slug}");
    let resp = public_get(&http, &url)
        .header("Accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .ok()?;
    if resp.status().is_success() {
        let html = resp.text().await.ok()?;
        if let Some(priced) = parse_libro_html_prices(&html) {
            return Some(priced);
        }
        tracing::debug!(isbn_or_slug, "libro product HTML had no parseable prices");
    } else {
        tracing::debug!(
            isbn_or_slug,
            status = %resp.status(),
            "libro product HTML price fetch non-success"
        );
    }

    // Fallback: explore details JSON (DFS first price as list/primary).
    // Strip slug suffix — explore keys by ISBN digits.
    let isbn_key = isbn_or_slug
        .split('-')
        .next()
        .filter(|s| is_isbn_digits(s))
        .unwrap_or(isbn_or_slug);
    let url = format!("https://libro.fm/explore/audiobook_details/{isbn_key}");
    let resp = public_get(&http, &url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let single = find_price_in_json(&body)?;
    Some(DualPriced {
        currency: single.currency,
        list_cents: Some(single.cents),
        list_label: Some(single.label),
        member_cents: None,
        member_label: None,
    })
}

struct SinglePriced {
    cents: i64,
    currency: String,
    label: String,
}

fn parse_libro_html_prices(html: &str) -> Option<DualPriced> {
    let lower = html.to_ascii_lowercase();
    let mut list_cents = money_after_marker(html, &lower, "class=\"price\"");
    let retail_cents = money_after_marker(html, &lower, "retail price:");
    let member_from_cta = member_price_from_cta(html, &lower);
    let low_ld = json_string_number(html, "lowPrice");
    let high_ld = json_string_number(html, "highPrice");

    let member_cents = member_from_cta.or(match (low_ld, high_ld, list_cents) {
        (Some(low), Some(high), _) if low < high => Some(low),
        (Some(low), _, Some(list)) if low < list => Some(low),
        _ => None,
    });

    if let Some(retail) = retail_cents {
        list_cents = Some(retail);
    } else if let Some(high) = high_ld {
        if list_cents.is_none() || member_cents.is_some_and(|m| list_cents == Some(m)) {
            list_cents = Some(high);
        }
    }

    let currency = String::from("USD");
    let list_label = list_cents.map(|c| format_money_label(c, &currency));
    let member_label = member_cents.map(|c| format_money_label(c, &currency));
    if list_cents.is_none() && member_cents.is_none() {
        return None;
    }
    let (member_cents, member_label) = match (member_cents, list_cents) {
        (Some(m), Some(l)) if m == l => (None, None),
        other => (other.0, member_label),
    };
    Some(DualPriced {
        currency,
        list_cents,
        list_label,
        member_cents,
        member_label,
    })
}

/// Find the first `$…` after `marker` (marker matched case-insensitively).
fn money_after_marker(html: &str, lower: &str, marker: &str) -> Option<i64> {
    let idx = lower.find(marker)?;
    let slice = &html[idx..html.len().min(idx + 160)];
    if let Some(dollar) = slice.find('$') {
        return parse_money_label_to_cents(&slice[dollar..slice.len().min(dollar + 16)]);
    }
    None
}

/// `Get for $14.99 with membership` CTA on Libro product pages.
fn member_price_from_cta(html: &str, lower: &str) -> Option<i64> {
    let idx = lower.find("with membership")?;
    let start = idx.saturating_sub(48);
    let window = &html[start..idx];
    let dollar = window.rfind('$')?;
    parse_money_label_to_cents(&window[dollar..])
}

fn json_string_number(html: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{key}\"");
    let idx = html.find(&needle)?;
    let after = &html[idx + needle.len()..];
    let colon = after.find(':')?;
    let mut rest = after[colon + 1..].trim_start();
    if let Some(stripped) = rest.strip_prefix('"') {
        rest = stripped;
        let end = rest.find('"')?;
        return parse_money_label_to_cents(&rest[..end]);
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    parse_money_label_to_cents(&rest[..end])
}

fn find_price_in_json(value: &Value) -> Option<SinglePriced> {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = k.to_ascii_lowercase();
                if matches!(
                    key.as_str(),
                    "price" | "sale_price" | "current_price" | "list_price" | "amount"
                ) {
                    if let Some(p) = price_from_json_node(v) {
                        return Some(p);
                    }
                }
            }
            for v in map.values() {
                if let Some(p) = find_price_in_json(v) {
                    return Some(p);
                }
            }
            None
        }
        Value::Array(items) => {
            for v in items {
                if let Some(p) = find_price_in_json(v) {
                    return Some(p);
                }
            }
            None
        }
        _ => None,
    }
}

fn price_from_json_node(v: &Value) -> Option<SinglePriced> {
    if let Some(s) = v.as_str() {
        if let Some(cents) = parse_money_label_to_cents(s) {
            return Some(SinglePriced {
                cents,
                currency: String::from("USD"),
                label: s.trim().to_string(),
            });
        }
    }
    if let Some(n) = v.as_f64() {
        let cents = if n >= 1000.0 {
            n.round() as i64
        } else {
            (n * 100.0).round() as i64
        };
        return Some(SinglePriced {
            cents: cents.max(0),
            currency: String::from("USD"),
            label: format_money_label(cents, "USD"),
        });
    }
    if let Some(obj) = v.as_object() {
        if let Some(amount) = obj
            .get("amount")
            .or_else(|| obj.get("value"))
            .and_then(Value::as_f64)
        {
            let currency = obj
                .get("currency")
                .or_else(|| obj.get("currency_code"))
                .and_then(Value::as_str)
                .unwrap_or("USD");
            let cents = (amount * 100.0).round() as i64;
            return Some(SinglePriced {
                cents: cents.max(0),
                currency: currency.to_string(),
                label: format_money_label(cents, currency),
            });
        }
    }
    None
}

fn series_num_to_index(v: Option<&Value>) -> Option<String> {
    let v = v?;
    if let Some(n) = v.as_i64() {
        return Some(n.to_string());
    }
    if let Some(n) = v.as_u64() {
        return Some(n.to_string());
    }
    if let Some(n) = v.as_f64() {
        if n.fract() == 0.0 {
            return Some((n as i64).to_string());
        }
        return Some(n.to_string());
    }
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_money_label_to_cents(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("free") || s.eq_ignore_ascii_case("free!") {
        return Some(0);
    }
    let mut num = String::new();
    let mut seen_dot = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else if c == '.' && !seen_dot {
            num.push('.');
            seen_dot = true;
        } else if c == ',' {
            continue;
        } else if !num.is_empty() {
            break;
        }
    }
    if num.is_empty() {
        return None;
    }
    let amount: f64 = num.parse().ok()?;
    Some((amount * 100.0).round() as i64)
}

fn format_money_label(cents: i64, currency: &str) -> String {
    if cents <= 0 {
        return String::from("FREE");
    }
    let major = cents / 100;
    let minor = (cents % 100).unsigned_abs();
    match currency.to_ascii_uppercase().as_str() {
        "USD" | "" => format!("${major}.{minor:02}"),
        "GBP" => format!("£{major}.{minor:02}"),
        "EUR" => format!("€{major}.{minor:02}"),
        other => format!("{major}.{minor:02} {other}"),
    }
}

fn primary_author(authors: Option<&str>) -> Option<&str> {
    authors?
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

fn urlencode_minimal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write;
                let _ = write!(out, "%{b:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn isbn_or_slug_accepts_isbn_and_slug() {
        assert_eq!(isbn_or_slug("9781980053446"), Some("9781980053446"));
        assert_eq!(
            isbn_or_slug("9781980053446-ashes-of-man"),
            Some("9781980053446-ashes-of-man")
        );
        assert!(isbn_or_slug("B09ABCDEFG").is_none());
        assert!(isbn_or_slug("chirp-uuid-here").is_none());
    }

    #[test]
    fn parse_html_search_ashes_of_man() {
        let html = r#"
            <img alt="View audiobook of Ashes of Man by Christopher Ruocchio"
                 src="//covers.libro.fm/9781980053446_400.jpg" />
            <div role="heading" aria-level="2" class="title">Ashes of Man</div>
            <a href="/audiobooks/9781980053446-ashes-of-man">View audiobook</a>
            <a href="/audiobooks/9781039471412-man-of-action">Other</a>
        "#;
        let books = parse_libro_search_html(html, 2);
        assert_eq!(books.len(), 2);
        assert_eq!(books[0].isbn.as_deref(), Some("9781980053446"));
        assert_eq!(books[0].title.as_deref(), Some("Ashes of Man"));
        assert_eq!(books[0].authors.as_deref(), Some("Christopher Ruocchio"));
        assert_eq!(books[0].slug.as_deref(), Some("9781980053446-ashes-of-man"));
    }

    #[test]
    fn parse_libro_related_object() {
        let v = json!({
            "isbn": "9781234567890",
            "title": "Next Title",
            "authors": [{"name": "Ada Author"}],
            "series": {"name": "Test Series"},
            "series_num": 3
        });
        let c = parse_libro_book(&v).unwrap();
        assert_eq!(c.isbn.as_deref(), Some("9781234567890"));
        assert_eq!(c.authors.as_deref(), Some("Ada Author"));
        assert_eq!(c.series.as_deref(), Some("Test Series"));
        assert_eq!(c.series_index.as_deref(), Some("3"));
    }

    #[test]
    fn parse_libro_explore_audiobook_rich() {
        let v = json!({
            "isbn": 9781446462164u64,
            "title": "Caligula",
            "authors": ["Douglas Jackson"],
            "subtitle": "A Novel",
            "publisher": "Random House",
            "publication_date": "2011-03-03",
            "description": "<p>Ancient Rome.</p>",
            "cover_url": "https://covers.libro.fm/9781446462164_400.jpg",
            "genres": [{"name": "Historical Fiction"}],
            "audiobook_info": {
                "narrators": ["Russell Boulter"],
                "duration": 40761
            },
            "series": "Rufus",
            "series_num": "1"
        });
        let c = parse_libro_book(&v).unwrap();
        assert_eq!(c.isbn.as_deref(), Some("9781446462164"));
        assert_eq!(c.narrators.as_deref(), Some("Russell Boulter"));
        assert_eq!(c.length_minutes, Some(679));
        assert_eq!(c.publisher.as_deref(), Some("Random House"));
        assert_eq!(c.categories.as_deref(), Some("Historical Fiction"));
        assert!(c.description.as_deref().unwrap().contains("Ancient Rome"));
        assert_eq!(c.series.as_deref(), Some("Rufus"));
        assert_eq!(c.series_index.as_deref(), Some("1"));
    }

    #[test]
    fn parse_iso8601_duration() {
        assert_eq!(parse_iso8601_duration_minutes("PT11H19M21S"), Some(679));
        assert_eq!(parse_iso8601_duration_minutes("PT90M"), Some(90));
        assert_eq!(parse_iso8601_duration_minutes("PT1H"), Some(60));
    }

    #[test]
    fn parse_libro_json_ld_caligula_shape() {
        let html = r#"
            <script type="application/ld+json">
            {
              "@context": "https://schema.org",
              "@type": "Audiobook",
              "name": "Caligula",
              "isbn": "9781446462164",
              "description": "<p>Ancient Rome.</p>",
              "image": "https://covers.libro.fm/9781446462164_1120.jpg",
              "author": [{"@type": "Person", "name": "Douglas Jackson"}],
              "readBy": [{"@type": "Person", "name": "Russell Boulter"}],
              "publisher": "Random House",
              "datePublished": "2011-03-03",
              "inLanguage": "en",
              "duration": "PT11H19M21S",
              "abridged": "false",
              "offers": {"lowPrice": "12.99", "priceCurrency": "USD"}
            }
            </script>
            <div class="audiobook-genres">
              <a href="/genres/fiction-literature">Fiction</a>
              <a href="/genres/mystery-thriller">Mystery &amp; Thriller</a>
              <a href="/genres/historical-fiction">Historical Fiction</a>
            </div>
            <p><span>Unabridged</span></p>
        "#;
        let c =
            parse_libro_product_html(html, "https://libro.fm/audiobooks/9781446462164").unwrap();
        assert_eq!(c.title, "Caligula");
        assert_eq!(c.authors.as_deref(), Some("Douglas Jackson"));
        assert_eq!(c.narrators.as_deref(), Some("Russell Boulter"));
        assert_eq!(c.length_minutes, Some(679));
        assert_eq!(c.publisher.as_deref(), Some("Random House"));
        assert_eq!(c.language.as_deref(), Some("en"));
        assert_eq!(c.price_cents, Some(1299));
        assert_eq!(c.is_abridged, Some(false));
        assert_eq!(
            c.categories.as_deref(),
            Some("Fiction, Mystery & Thriller, Historical Fiction")
        );
        assert!(c.description.as_deref().unwrap().contains("Ancient Rome"));
    }

    #[test]
    fn parse_libro_html_genres_decodes_entities() {
        let html = r#"<div class="audiobook-genres"><a href="/g">Mystery &amp; Thriller</a></div>"#;
        assert_eq!(
            parse_libro_html_genres(html).as_deref(),
            Some("Mystery & Thriller")
        );
    }

    #[test]
    fn money_label_parsing() {
        assert_eq!(parse_money_label_to_cents("$2.99"), Some(299));
        assert_eq!(parse_money_label_to_cents("FREE"), Some(0));
        assert_eq!(format_money_label(299, "USD"), "$2.99");
    }

    #[test]
    fn html_dual_prices_member_cta() {
        let html = r#"
            <div class="price-sidebar">
              <div class="price-info"><p class="price">$37.79</p></div>
              <a href="/membership/new">Get for $14.99 with membership</a>
            </div>
            <script type="application/ld+json">
            {"offers":{"@type":"AggregateOffer","lowPrice":"14.99","highPrice":"37.79"}}
            </script>
        "#;
        let priced = parse_libro_html_prices(html).unwrap();
        assert_eq!(priced.list_cents, Some(3779));
        assert_eq!(priced.member_cents, Some(1499));
    }

    #[test]
    fn html_dual_prices_retail_line() {
        let html = r#"
            <div class="price-info">
              <p class="price">$17.96</p>
              <p class="retail">Retail price: $19.95</p>
            </div>
            <script type="application/ld+json">
            {"offers":{"lowPrice":"14.99","highPrice":"19.95"}}
            </script>
        "#;
        let priced = parse_libro_html_prices(html).unwrap();
        assert_eq!(priced.list_cents, Some(1995));
        assert_eq!(priced.member_cents, Some(1499));
    }
}
