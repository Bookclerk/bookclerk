//! Suggest storefronts where a title might be purchased (with live pricing).

use bookclerk_chirp::ChirpClient;
use bookclerk_enrich::{
    normalize_region, public_http_client, region_tld, search_catalog_asins, search_catalog_keywords,
};
use bookclerk_graphicaudio::{
    catalog_http_client, search_catalog as ga_search_catalog, DEFAULT_STORE_URL,
};
use serde::Deserialize;
use serde_json::Value;

use crate::error::Result;

/// A purchase / catalog availability hint (optionally priced at view time).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PurchaseHint {
    pub source: String,
    pub product_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    /// Lowest known sell price in minor units (cents). `0` = free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_cents: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
    /// Display string from the store (`$2.99`, `FREE`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_label: Option<String>,
}

impl PurchaseHint {
    /// URL-only catalog link (no price yet).
    #[must_use]
    pub fn link(
        source: impl Into<String>,
        product_id: impl Into<String>,
        title: Option<String>,
        url: Option<String>,
    ) -> Self {
        Self {
            source: source.into(),
            product_id: product_id.into(),
            title,
            url,
            price_cents: None,
            currency: None,
            price_label: None,
        }
    }

    fn with_price(mut self, cents: i64, currency: &str, label: impl Into<String>) -> Self {
        self.price_cents = Some(cents.max(0));
        self.currency = Some(currency.to_string());
        self.price_label = Some(label.into());
        self
    }
}

/// Inputs for view-time catalog + pricing lookup.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PurchaseHintsQuery {
    pub title: String,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub candidate_source: Option<String>,
    pub candidate_product_id: Option<String>,
    /// Known storefront editions already on the recommendation card.
    #[serde(default)]
    pub store_editions: Vec<crate::identity::StoreEdition>,
    pub region: Option<String>,
    /// Storefronts the caller has accounts for — used to pick “best” price
    /// among linked stores while still returning every catalog match.
    #[serde(default)]
    pub preferred_sources: Vec<String>,
}

/// Priced catalog matches for one title, sorted best-first.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PurchaseHintsResponse {
    pub hints: Vec<PurchaseHint>,
    /// Lowest-priced hint (or first catalog hit when no prices resolved).
    pub best: Option<PurchaseHint>,
}

/// Look up Audible (public catalog) and Libro.fm (explore) for a title.
///
/// URL-only; call [`resolve_purchase_hints`] for multi-store + live pricing.
pub async fn purchase_hints_for(
    title: &str,
    author: Option<&str>,
    asin: Option<&str>,
    isbn: Option<&str>,
    region: &str,
) -> Result<Vec<PurchaseHint>> {
    let http = public_http_client()?;
    let mut hints = Vec::new();
    let region = normalize_region(region);

    if let Some(asin) = asin.map(str::trim).filter(|s| !s.is_empty()) {
        hints.push(audible_hint(asin, Some(title.to_string()), &region));
    } else if !title.trim().is_empty() {
        let asins = search_catalog_asins(&http, &region, title, author).await?;
        if let Some(asin) = asins.into_iter().next() {
            hints.push(audible_hint(&asin, Some(title.to_string()), &region));
        } else if let Some(isbn) = isbn {
            let asins = search_catalog_keywords(&http, &region, isbn).await?;
            if let Some(asin) = asins.into_iter().next() {
                hints.push(audible_hint(&asin, Some(title.to_string()), &region));
            }
        }
    }

    if let Some(isbn) = isbn.map(str::trim).filter(|s| !s.is_empty()) {
        hints.push(libro_hint(isbn, Some(title.to_string())));
    } else if let Some(hit) = libro_explore_search(&http, title, author).await? {
        hints.push(hit);
    }

    Ok(hints)
}

/// Seed a deterministic storefront URL from a known candidate (no remote I/O).
#[must_use]
pub fn seed_purchase_hint(
    source: &str,
    product_id: &str,
    title: Option<String>,
    region: &str,
) -> Option<PurchaseHint> {
    let pid = product_id.trim();
    if pid.is_empty() {
        return None;
    }
    let region = normalize_region(region);
    match source.trim().to_ascii_lowercase().as_str() {
        "audible" => Some(audible_hint(pid, title, &region)),
        "libro" => Some(libro_hint(pid, title)),
        "chirp" => Some(PurchaseHint::link(
            "chirp",
            pid,
            title,
            Some(format!("https://www.chirpbooks.com/audiobooks/{pid}")),
        )),
        "graphicaudio" => Some(PurchaseHint::link(
            "graphicaudio",
            pid,
            title,
            Some(format!(
                "https://www.graphicaudio.net/catalog/product/view/id/{pid}"
            )),
        )),
        _ => None,
    }
}

/// Resolve every catalog match and attach live prices (view-time).
pub async fn resolve_purchase_hints(query: &PurchaseHintsQuery) -> Result<PurchaseHintsResponse> {
    let region = normalize_region(query.region.as_deref().unwrap_or("us"));
    let title = query.title.trim();
    let authors = query
        .authors
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let asin = query
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let isbn = query
        .isbn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut hints: Vec<PurchaseHint> = Vec::new();

    for ed in &query.store_editions {
        if let Some(seed) =
            seed_purchase_hint(&ed.source, &ed.product_id, Some(title.to_string()), &region)
        {
            push_dedupe(&mut hints, seed);
        }
    }

    if let (Some(source), Some(pid)) = (
        query.candidate_source.as_deref(),
        query.candidate_product_id.as_deref(),
    ) {
        if let Some(seed) = seed_purchase_hint(source, pid, Some(title.to_string()), &region) {
            push_dedupe(&mut hints, seed);
        }
        // Candidate ASIN/ISBN may differ from product id.
        if source.eq_ignore_ascii_case("audible") {
            // already seeded
        } else if let Some(a) = asin {
            push_dedupe(
                &mut hints,
                audible_hint(a, Some(title.to_string()), &region),
            );
        }
        if source.eq_ignore_ascii_case("libro") {
            // already seeded
        } else if let Some(i) = isbn {
            push_dedupe(&mut hints, libro_hint(i, Some(title.to_string())));
        }
    } else {
        if let Some(a) = asin {
            push_dedupe(
                &mut hints,
                audible_hint(a, Some(title.to_string()), &region),
            );
        }
        if let Some(i) = isbn {
            push_dedupe(&mut hints, libro_hint(i, Some(title.to_string())));
        }
    }

    // Cross-store catalog expansion (Audible + Libro explore).
    match purchase_hints_for(title, authors, asin, isbn, &region).await {
        Ok(extra) => {
            for h in extra {
                push_dedupe(&mut hints, h);
            }
        }
        Err(err) => tracing::debug!(error = %err, "purchase catalog expand failed"),
    }

    // Chirp catalog search / known id.
    if let Err(err) = append_chirp_catalog(&mut hints, title, authors, query).await {
        tracing::debug!(error = %err, "chirp catalog expand failed");
    }

    // GraphicAudio Magento search / known id.
    if let Err(err) = append_ga_catalog(&mut hints, title, authors, query).await {
        tracing::debug!(error = %err, "graphicaudio catalog expand failed");
    }

    enrich_hints_with_prices(&mut hints, &region).await;
    let preferred = preferred_source_set(&query.preferred_sources);
    sort_hints_for_display(&mut hints, &preferred);
    let best = best_purchase_hint_preferring(&hints, &preferred).cloned();
    Ok(PurchaseHintsResponse { hints, best })
}

/// Pick the lowest-priced hint (ties keep earlier order). Unpriced sort after priced.
#[must_use]
pub fn best_purchase_hint(hints: &[PurchaseHint]) -> Option<&PurchaseHint> {
    hints.iter().min_by(|a, b| cmp_hint_price(a, b))
}

/// Prefer lowest **priced** offer among linked storefronts; otherwise global lowest.
///
/// Unpriced linked stores do not beat a cheaper priced offer elsewhere — we still
/// search every store and only bias the “best” highlight toward accounts the
/// caller can actually use when prices are known.
#[must_use]
pub fn best_purchase_hint_preferring<'a>(
    hints: &'a [PurchaseHint],
    preferred: &std::collections::HashSet<String>,
) -> Option<&'a PurchaseHint> {
    if !preferred.is_empty() {
        let among_linked_priced: Vec<_> = hints
            .iter()
            .filter(|h| {
                preferred.contains(&h.source.to_ascii_lowercase()) && h.price_cents.is_some()
            })
            .collect();
        if let Some(best) = among_linked_priced
            .into_iter()
            .min_by(|a, b| cmp_hint_price(a, b))
        {
            return Some(best);
        }
    }
    best_purchase_hint(hints)
}

/// Resolve many purchase-hint queries with bounded concurrency (order preserved).
pub async fn resolve_purchase_hints_batch(
    queries: &[PurchaseHintsQuery],
    max_concurrent: usize,
) -> Vec<Result<PurchaseHintsResponse>> {
    let limit = max_concurrent.clamp(1, 8);
    let mut out = Vec::with_capacity(queries.len());
    for chunk in queries.chunks(limit) {
        let mut set = tokio::task::JoinSet::new();
        for (offset, q) in chunk.iter().enumerate() {
            let q = q.clone();
            set.spawn(async move { (offset, resolve_purchase_hints(&q).await) });
        }
        let mut slot: Vec<Option<Result<PurchaseHintsResponse>>> =
            (0..chunk.len()).map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((offset, result)) => slot[offset] = Some(result),
                Err(err) => tracing::debug!(error = %err, "purchase-hints batch task failed"),
            }
        }
        for result in slot {
            out.push(result.unwrap_or_else(|| {
                Err(crate::error::DiscoverError::message(
                    "purchase-hints batch task cancelled",
                ))
            }));
        }
    }
    out
}

fn preferred_source_set(raw: &[String]) -> std::collections::HashSet<String> {
    raw.iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn cmp_hint_price(a: &PurchaseHint, b: &PurchaseHint) -> std::cmp::Ordering {
    match (a.price_cents, b.price_cents) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn sort_hints_for_display(
    hints: &mut [PurchaseHint],
    preferred: &std::collections::HashSet<String>,
) {
    hints.sort_by(|a, b| {
        let a_pref = preferred.contains(&a.source.to_ascii_lowercase());
        let b_pref = preferred.contains(&b.source.to_ascii_lowercase());
        match (a_pref, b_pref) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => cmp_hint_price(a, b),
        }
    });
}

fn push_dedupe(hints: &mut Vec<PurchaseHint>, hint: PurchaseHint) {
    let key = (
        hint.source.to_ascii_lowercase(),
        hint.product_id.to_ascii_lowercase(),
    );
    if hints
        .iter()
        .any(|h| h.source.eq_ignore_ascii_case(&key.0) && h.product_id.eq_ignore_ascii_case(&key.1))
    {
        return;
    }
    // One row per source: keep first (usually the proposing / known id).
    if hints
        .iter()
        .any(|h| h.source.eq_ignore_ascii_case(&hint.source))
    {
        return;
    }
    hints.push(hint);
}

fn audible_hint(asin: &str, title: Option<String>, region: &str) -> PurchaseHint {
    let asin = asin.to_ascii_uppercase();
    PurchaseHint::link(
        "audible",
        asin.clone(),
        title,
        Some(format!(
            "https://www.audible{}/pd/{}",
            region_host_suffix(region),
            asin
        )),
    )
}

fn libro_hint(isbn_or_slug: &str, title: Option<String>) -> PurchaseHint {
    PurchaseHint::link(
        "libro",
        isbn_or_slug,
        title,
        Some(format!("https://libro.fm/audiobooks/{isbn_or_slug}")),
    )
}

fn region_host_suffix(region: &str) -> &'static str {
    match region {
        "uk" => ".co.uk",
        "ca" => ".ca",
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

#[derive(Debug, Deserialize)]
struct LibroExploreSearch {
    #[serde(default)]
    audiobook_collection: Option<LibroCollection>,
}

#[derive(Debug, Deserialize)]
struct LibroCollection {
    #[serde(default)]
    audiobooks: Vec<LibroBook>,
}

#[derive(Debug, Deserialize)]
struct LibroBook {
    isbn: Option<String>,
    title: Option<String>,
    slug: Option<String>,
}

async fn libro_explore_search(
    http: &reqwest::Client,
    title: &str,
    author: Option<&str>,
) -> Result<Option<PurchaseHint>> {
    let q = match author {
        Some(a) if !a.is_empty() => format!("{title} {a}"),
        _ => title.to_string(),
    };
    let url = format!(
        "https://libro.fm/explore/search?page=1&q={}",
        crate_urlencode(&q)
    );
    let resp = http.get(&url).send().await?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), "libro explore search non-success");
        return Ok(None);
    }
    let body: LibroExploreSearch = match resp.json().await {
        Ok(b) => b,
        Err(err) => {
            tracing::debug!(error = %err, "libro explore search parse failed");
            return Ok(None);
        }
    };
    let Some(book) = body
        .audiobook_collection
        .and_then(|c| c.audiobooks.into_iter().next())
    else {
        return Ok(None);
    };
    let Some(isbn) = book.isbn.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let url = book
        .slug
        .map(|s| format!("https://libro.fm/audiobooks/{s}"))
        .or_else(|| Some(format!("https://libro.fm/audiobooks/{isbn}")));
    Ok(Some(PurchaseHint::link("libro", isbn, book.title, url)))
}

async fn append_chirp_catalog(
    hints: &mut Vec<PurchaseHint>,
    title: &str,
    authors: Option<&str>,
    query: &PurchaseHintsQuery,
) -> Result<()> {
    let client = ChirpClient::default();
    if let Some(pid) = query.candidate_product_id.as_deref().filter(|_| {
        query
            .candidate_source
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("chirp"))
    }) {
        let url = format!("https://www.chirpbooks.com/audiobooks/{pid}");
        push_dedupe(
            hints,
            PurchaseHint::link("chirp", pid, Some(title.to_string()), Some(url)),
        );
        return Ok(());
    }
    if title.is_empty() {
        return Ok(());
    }
    let q = match authors {
        Some(a) => format!("{title} {a}"),
        None => title.to_string(),
    };
    let hits = client.typeahead(&q).await.unwrap_or_default();
    if let Some(hit) = hits.audiobooks.into_iter().next() {
        let url = hit
            .url
            .map(|u| {
                if u.starts_with("http") {
                    u
                } else {
                    format!("https://www.chirpbooks.com{u}")
                }
            })
            .or_else(|| Some(format!("https://www.chirpbooks.com/audiobooks/{}", hit.id)));
        push_dedupe(
            hints,
            PurchaseHint::link("chirp", hit.id, hit.display_title, url),
        );
    }
    Ok(())
}

async fn append_ga_catalog(
    hints: &mut Vec<PurchaseHint>,
    title: &str,
    authors: Option<&str>,
    query: &PurchaseHintsQuery,
) -> Result<()> {
    if let Some(pid) = query.candidate_product_id.as_deref().filter(|_| {
        query
            .candidate_source
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("graphicaudio"))
    }) {
        push_dedupe(
            hints,
            PurchaseHint::link(
                "graphicaudio",
                pid,
                Some(title.to_string()),
                Some(format!(
                    "https://www.graphicaudio.net/catalog/product/view/id/{pid}"
                )),
            ),
        );
        return Ok(());
    }
    if title.is_empty() {
        return Ok(());
    }
    let http = match catalog_http_client() {
        Ok(c) => c,
        Err(err) => {
            tracing::debug!(error = %err, "graphicaudio http client failed");
            return Ok(());
        }
    };
    let q = match authors {
        Some(a) => format!("{title} {a}"),
        None => title.to_string(),
    };
    let hits = ga_search_catalog(&http, DEFAULT_STORE_URL, &q)
        .await
        .unwrap_or_default();
    if let Some(hit) = hits.into_iter().next() {
        let url = hit.url.or_else(|| {
            Some(format!(
                "https://www.graphicaudio.net/catalog/product/view/id/{}",
                hit.product_id
            ))
        });
        push_dedupe(
            hints,
            PurchaseHint::link("graphicaudio", hit.product_id, Some(hit.title), url),
        );
    }
    Ok(())
}

async fn enrich_hints_with_prices(hints: &mut [PurchaseHint], region: &str) {
    // Sequential soft-fail lookups — keep VPS-friendly and simple.
    for hint in hints.iter_mut() {
        match hint.source.as_str() {
            "audible" => {
                if let Some(priced) = fetch_audible_price(&hint.product_id, region).await {
                    *hint = std::mem::take(hint).with_price(
                        priced.cents,
                        &priced.currency,
                        priced.label,
                    );
                }
            }
            "chirp" => {
                if let Some(priced) = fetch_chirp_price(&hint.product_id).await {
                    *hint = std::mem::take(hint).with_price(
                        priced.cents,
                        &priced.currency,
                        priced.label,
                    );
                    if let Some(url) = priced.purchase_url {
                        hint.url = Some(url);
                    }
                }
            }
            "libro" => {
                if let Some(priced) = fetch_libro_price(&hint.product_id).await {
                    *hint = std::mem::take(hint).with_price(
                        priced.cents,
                        &priced.currency,
                        priced.label,
                    );
                }
            }
            "graphicaudio" => {
                if let Some(priced) = fetch_ga_price(hint.url.as_deref(), &hint.product_id).await {
                    *hint = std::mem::take(hint).with_price(
                        priced.cents,
                        &priced.currency,
                        priced.label,
                    );
                }
            }
            _ => {}
        }
    }
}

struct Priced {
    cents: i64,
    currency: String,
    label: String,
    purchase_url: Option<String>,
}

async fn fetch_audible_price(asin: &str, region: &str) -> Option<Priced> {
    let http = public_http_client().ok()?;
    let region = normalize_region(region);
    let url = format!(
        "https://api.audible{}/1.0/catalog/products",
        region_tld(&region)
    );
    let resp = http
        .get(&url)
        .query(&[
            ("asins", asin),
            ("num_results", "1"),
            ("response_groups", "price,product_desc,product_attrs"),
        ])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let products = body.get("products")?.as_array()?;
    let product = products.iter().find(|p| {
        p.get("asin")
            .and_then(Value::as_str)
            .is_some_and(|a| a.eq_ignore_ascii_case(asin))
    })?;
    parse_audible_price_value(product.get("price")?)
}

/// Parse Audible `price` response-group object.
fn parse_audible_price_value(price: &Value) -> Option<Priced> {
    let lowest = price
        .get("lowest_price")
        .or_else(|| price.get("list_price"))?;
    let amount = lowest.get("base")?.as_f64()?;
    let currency = lowest
        .get("currency_code")
        .and_then(Value::as_str)
        .unwrap_or("USD");
    let cents = (amount * 100.0).round() as i64;
    Some(Priced {
        cents: cents.max(0),
        currency: currency.to_string(),
        label: format_money_label(cents, currency),
        purchase_url: None,
    })
}

async fn fetch_chirp_price(audiobook_id: &str) -> Option<Priced> {
    let client = ChirpClient::default();
    let pricing = client.audiobook_pricing(audiobook_id).await.ok()??;
    let label = pricing.discount_price.trim();
    let (cents, label) = if pricing.is_free_listing
        || label.eq_ignore_ascii_case("free")
        || label.eq_ignore_ascii_case("free!")
        || pricing.discounted_price_cents == Some(0)
    {
        (0, String::from("FREE"))
    } else if let Some(c) = pricing.discounted_price_cents.filter(|c| *c > 0) {
        let display = if label.is_empty() {
            format_money_label(c, "USD")
        } else {
            label.to_string()
        };
        (c, display)
    } else {
        let c = parse_money_label_to_cents(label)?;
        (c, label.to_string())
    };
    let purchase_url = pricing.purchase_url.map(|u| {
        if u.starts_with("http") {
            u
        } else {
            format!("https://www.chirpbooks.com{u}")
        }
    });
    Some(Priced {
        cents,
        currency: String::from("USD"),
        label,
        purchase_url,
    })
}

async fn fetch_libro_price(isbn_or_slug: &str) -> Option<Priced> {
    // Explore details often blocks datacenter IPs; soft-fail without poisoning the link.
    let http = public_http_client().ok()?;
    let url = format!("https://libro.fm/explore/audiobook_details/{isbn_or_slug}");
    let resp = http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    find_price_in_json(&body)
}

async fn fetch_ga_price(product_url: Option<&str>, product_id: &str) -> Option<Priced> {
    let http = catalog_http_client().ok()?;
    let url = product_url.map(str::to_string).unwrap_or_else(|| {
        format!("https://www.graphicaudio.net/catalog/product/view/id/{product_id}")
    });
    let resp = http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let html = resp.text().await.ok()?;
    parse_ga_price_html(&html)
}

fn find_price_in_json(value: &Value) -> Option<Priced> {
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

fn price_from_json_node(v: &Value) -> Option<Priced> {
    if let Some(s) = v.as_str() {
        if let Some(cents) = parse_money_label_to_cents(s) {
            return Some(Priced {
                cents,
                currency: String::from("USD"),
                label: s.trim().to_string(),
                purchase_url: None,
            });
        }
    }
    if let Some(n) = v.as_f64() {
        let cents = if n >= 1000.0 {
            // Heuristic: large ints are often cents already.
            n.round() as i64
        } else {
            (n * 100.0).round() as i64
        };
        return Some(Priced {
            cents: cents.max(0),
            currency: String::from("USD"),
            label: format_money_label(cents, "USD"),
            purchase_url: None,
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
            return Some(Priced {
                cents: cents.max(0),
                currency: currency.to_string(),
                label: format_money_label(cents, currency),
                purchase_url: None,
            });
        }
    }
    None
}

fn parse_ga_price_html(html: &str) -> Option<Priced> {
    // Magento data-price-amount is authoritative when present.
    if let Some(idx) = html.find("data-price-amount=\"") {
        let rest = &html[idx + "data-price-amount=\"".len()..];
        let end = rest.find('"')?;
        let raw = &rest[..end];
        if let Ok(amount) = raw.parse::<f64>() {
            let cents = (amount * 100.0).round() as i64;
            return Some(Priced {
                cents: cents.max(0),
                currency: String::from("USD"),
                label: format_money_label(cents, "USD"),
                purchase_url: None,
            });
        }
    }
    // Fallback: first $x.xx in a price box.
    for marker in ["price-wrapper", "product-info-price", "price-box"] {
        if let Some(idx) = html.find(marker) {
            let window = &html[idx..html.len().min(idx + 800)];
            if let Some(cents) = window
                .split('$')
                .nth(1)
                .and_then(|s| parse_money_label_to_cents(&format!("${}", &s[..s.len().min(12)])))
            {
                return Some(Priced {
                    cents,
                    currency: String::from("USD"),
                    label: format_money_label(cents, "USD"),
                    purchase_url: None,
                });
            }
        }
    }
    None
}

/// Parse `$12.34` / `12.34` / `FREE` into cents.
#[must_use]
pub fn parse_money_label_to_cents(raw: &str) -> Option<i64> {
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

#[must_use]
pub fn format_money_label(cents: i64, currency: &str) -> String {
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

fn crate_urlencode(s: &str) -> String {
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
    use serde_json::json;

    #[test]
    fn money_label_parsing() {
        assert_eq!(parse_money_label_to_cents("$2.99"), Some(299));
        assert_eq!(parse_money_label_to_cents("FREE"), Some(0));
        assert_eq!(parse_money_label_to_cents("12.5"), Some(1250));
        assert_eq!(format_money_label(299, "USD"), "$2.99");
        assert_eq!(format_money_label(0, "USD"), "FREE");
    }

    #[test]
    fn audible_price_json() {
        let price = json!({
            "credit_price": 1.0,
            "list_price": {
                "base": 25.219999313354492,
                "currency_code": "USD",
                "type": "list"
            },
            "lowest_price": {
                "base": 14.95,
                "currency_code": "USD",
                "type": "member"
            }
        });
        let priced = parse_audible_price_value(&price).unwrap();
        assert_eq!(priced.cents, 1495);
        assert_eq!(priced.label, "$14.95");
    }

    #[test]
    fn best_hint_prefers_lowest_price() {
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None).with_price(1999, "USD", "$19.99"),
            PurchaseHint::link("chirp", "C", None, None).with_price(299, "USD", "$2.99"),
            PurchaseHint::link("libro", "L", None, None),
        ];
        let best = best_purchase_hint(&hints).unwrap();
        assert_eq!(best.source, "chirp");
        assert_eq!(best.price_cents, Some(299));
    }

    #[test]
    fn best_hint_prefers_priced_linked_store() {
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None).with_price(1999, "USD", "$19.99"),
            PurchaseHint::link("chirp", "C", None, None).with_price(299, "USD", "$2.99"),
            PurchaseHint::link("libro", "L", None, None).with_price(999, "USD", "$9.99"),
        ];
        let preferred = std::collections::HashSet::from([String::from("audible")]);
        let best = best_purchase_hint_preferring(&hints, &preferred).unwrap();
        assert_eq!(best.source, "audible");
    }

    #[test]
    fn unpriced_linked_does_not_beat_cheaper_foreign() {
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None),
            PurchaseHint::link("chirp", "C", None, None).with_price(299, "USD", "$2.99"),
        ];
        let preferred = std::collections::HashSet::from([String::from("audible")]);
        let best = best_purchase_hint_preferring(&hints, &preferred).unwrap();
        assert_eq!(best.source, "chirp");
    }

    #[test]
    fn ga_price_from_data_attribute() {
        let html = r#"<div class="price-box"><span data-price-amount="19.99" data-price-type="finalPrice"></span></div>"#;
        let priced = parse_ga_price_html(html).unwrap();
        assert_eq!(priced.cents, 1999);
    }
}
