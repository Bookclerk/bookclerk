//! [`GraphicAudioSource`]: catalog lookups against the public GraphicAudio storefront.
//!
//! Account login, library scans and title fetches run in the guest RPC
//! handlers ([`crate::guest`]); the host stores the credentials they return.

use bookclerk_storefront::{
    CatalogHit, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, SourcePurchaseHint,
};

use crate::catalog::{
    catalog_http_client, expand_from_product_id, expand_from_search, MagentoCatalogProduct,
};
use crate::magento::DEFAULT_STORE_URL;

/// Describe / config id for this store (`graphicaudio`).
pub const ID: &str = "graphicaudio";

/// GraphicAudio catalog client used by the guest's discovery entrypoints.
///
/// The catalog lives on the public Magento storefront, so no account or
/// access path is involved.
#[derive(Debug, Clone)]
pub struct GraphicAudioSource {
    /// Magento storefront origin (production default or a wiremock override).
    store_url: String,
}

impl Default for GraphicAudioSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphicAudioSource {
    /// Production GraphicAudio storefront origin.
    #[must_use]
    pub fn new() -> Self {
        Self {
            store_url: DEFAULT_STORE_URL.to_string(),
        }
    }

    /// Override Magento storefront base (wiremock / staging).
    #[must_use]
    pub fn with_store_url(mut self, store_url: impl Into<String>) -> Self {
        self.store_url = store_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Searches the public catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the storefront request or its parse fails.
    pub async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
        let q = opts.query.trim();
        if q.is_empty() || opts.limit == 0 {
            return Ok(Vec::new());
        }
        let http = match catalog_http_client() {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };
        let page = opts.page.max(1);
        let products =
            match crate::catalog::search_catalog_page(&http, &self.store_url, q, page).await {
                Ok(p) => p,
                Err(err) => {
                    tracing::debug!(error = %err, "graphicaudio catalog search failed");
                    return Ok(Vec::new());
                }
            };
        Ok(products
            .into_iter()
            .take(opts.limit)
            .filter(|p| !p.product_id.trim().is_empty())
            .map(|p| ga_catalog_hit(&p, String::from("search")))
            .collect())
    }

    /// Finds up to `limit` titles related to `seed` (same author, series, …).
    ///
    /// # Errors
    ///
    /// Returns an error when the storefront request or its parse fails.
    pub async fn expand_candidates(
        &self,
        seed: &ExpandSeed,
        limit: usize,
    ) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let http = match catalog_http_client() {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };
        let mut by_id = std::collections::HashMap::new();

        if seed.source.eq_ignore_ascii_case(ID) && !seed.product_id.is_empty() {
            match expand_from_product_id(&http, Some(&self.store_url), &seed.product_id).await {
                Ok(products) => {
                    for p in products {
                        by_id.entry(p.product_id.clone()).or_insert_with(|| {
                            ga_catalog_hit(
                                &p,
                                format!("graphicaudio related/series for “{}”", seed.title),
                            )
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        id = %seed.product_id,
                        error = %err,
                        "graphicaudio product expand failed"
                    );
                }
            }
        }

        // Series / title Magento search for GA seeds or when series is known.
        let worth = seed.source.eq_ignore_ascii_case(ID)
            || seed
                .series
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
        if worth && by_id.len() < limit {
            let query = seed
                .series
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(seed.title.as_str());
            match expand_from_search(&http, Some(&self.store_url), query).await {
                Ok(products) => {
                    for p in products {
                        by_id.entry(p.product_id.clone()).or_insert_with(|| {
                            ga_catalog_hit(&p, format!("graphicaudio catalog search (“{query}”)"))
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!(query, error = %err, "graphicaudio search failed");
                }
            }
        }

        let mut hits: Vec<_> = by_id.into_values().collect();
        hits.truncate(limit);
        Ok(hits)
    }

    /// Resolves the storefront price and link for a title; `None` when not sold.
    ///
    /// # Errors
    ///
    /// Returns an error when the storefront request or its parse fails.
    pub async fn purchase_hint(
        &self,
        opts: &PurchaseHintOpts,
    ) -> bookclerk_storefront::Result<Option<SourcePurchaseHint>> {
        let mut hint = if let Some(pid) = opts
            .product_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(SourcePurchaseHint {
                product_id: pid.to_string(),
                title: opts.title.clone(),
                url: Some(format!(
                    "{}/catalog/product/view/id/{pid}",
                    self.store_url.trim_end_matches('/')
                )),
                ..Default::default()
            })
        } else {
            let title = opts.title.as_deref().unwrap_or("").trim();
            if title.is_empty() {
                None
            } else {
                let http = match catalog_http_client() {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                };
                let q = match opts
                    .authors
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(a) => format!("{title} {a}"),
                    None => title.to_string(),
                };
                match crate::catalog::search_catalog(&http, &self.store_url, &q).await {
                    Ok(hits) => pick_ga_purchase_hit(title, &hits).map(|hit| {
                        let url = hit.url.clone().or_else(|| {
                            Some(format!(
                                "{}/catalog/product/view/id/{}",
                                self.store_url.trim_end_matches('/'),
                                hit.product_id
                            ))
                        });
                        SourcePurchaseHint {
                            product_id: hit.product_id.clone(),
                            title: Some(hit.title.clone()),
                            url,
                            ..Default::default()
                        }
                    }),
                    Err(err) => {
                        tracing::debug!(error = %err, "graphicaudio purchase search failed");
                        None
                    }
                }
            }
        };

        if opts.with_price {
            if let Some(ref mut h) = hint {
                if let Some(priced) =
                    fetch_ga_price(h.url.as_deref(), &h.product_id, &self.store_url).await
                {
                    h.price_cents = Some(priced.0);
                    h.currency = Some(String::from("USD"));
                    h.price_label = Some(priced.1);
                }
            }
        }
        Ok(hint)
    }
}

/// Maps a Magento catalog product onto a [`CatalogHit`] (no authors or pricing).
fn ga_catalog_hit(p: &MagentoCatalogProduct, origin: String) -> CatalogHit {
    CatalogHit {
        product_id: p.product_id.clone(),
        title: p.title.clone(),
        series_entries: bookclerk_storefront::records::series_entries(p.series.as_deref(), None),
        expression_kind: bookclerk_storefront::records::ExpressionKind::Dramatisation,
        url: p.url.clone(),
        cover_url: p.cover_url.clone(),
        origin,
        ..Default::default()
    }
}

/// Pick a Magento hit that actually matches the queried title (not the first rank).
fn pick_ga_purchase_hit<'a>(
    query_title: &str,
    hits: &'a [MagentoCatalogProduct],
) -> Option<&'a MagentoCatalogProduct> {
    let q = normalize_ga_title(query_title);
    if q.is_empty() {
        return None;
    }
    hits.iter()
        .filter(|h| !h.is_series_set())
        .filter(|h| ga_titles_match(&q, &normalize_ga_title(&h.title)))
        .max_by(|a, b| {
            let sa = ga_title_score(&q, &normalize_ga_title(&a.title));
            let sb = ga_title_score(&q, &normalize_ga_title(&b.title));
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
}

/// Lowercased alphanumeric fold with collapsed whitespace for GraphicAudio title compares.
fn normalize_ga_title(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// True on exact fold or a contiguous phrase match when the shorter title is ≥8 chars and ≥55% of the longer.
fn ga_titles_match(query: &str, hit: &str) -> bool {
    if query.is_empty() || hit.is_empty() {
        return false;
    }
    if query == hit {
        return true;
    }
    // Contiguous phrase match only when the shorter is a large fraction of the longer
    // (avoids "man" / "ashes" token collisions with unrelated Magento series).
    let (shorter, longer) = if query.chars().count() <= hit.chars().count() {
        (query, hit)
    } else {
        (hit, query)
    };
    if shorter.chars().count() < 8 {
        return false;
    }
    let ratio = shorter.chars().count() as f32 / longer.chars().count() as f32;
    ratio >= 0.55
        && (longer == shorter
            || longer.starts_with(&format!("{shorter} "))
            || longer.ends_with(&format!(" {shorter}"))
            || longer.contains(&format!(" {shorter} ")))
}

/// Ranking score in `[0, 1]`: 1.0 on exact fold, otherwise shorter/longer length when contained.
fn ga_title_score(query: &str, hit: &str) -> f32 {
    if query == hit {
        return 1.0;
    }
    let (shorter, longer) = if query.len() <= hit.len() {
        (query, hit)
    } else {
        (hit, query)
    };
    if longer.contains(shorter) {
        shorter.len() as f32 / longer.len() as f32
    } else {
        0.0
    }
}

/// Fetches a Magento product page and parses USD cents plus a display label; `None` on HTTP/parse failure.
async fn fetch_ga_price(
    product_url: Option<&str>,
    product_id: &str,
    store_url: &str,
) -> Option<(i64, String)> {
    let http = catalog_http_client().ok()?;
    let url = product_url.map(str::to_string).unwrap_or_else(|| {
        format!(
            "{}/catalog/product/view/id/{product_id}",
            store_url.trim_end_matches('/')
        )
    });
    let resp = http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let html = resp.text().await.ok()?;
    parse_ga_price_html(&html)
}

/// Reads `data-price-amount` or a `$` amount near Magento price wrappers; returns cents and a USD label.
fn parse_ga_price_html(html: &str) -> Option<(i64, String)> {
    if let Some(idx) = html.find("data-price-amount=\"") {
        let rest = &html[idx + "data-price-amount=\"".len()..];
        let end = rest.find('"')?;
        let raw = &rest[..end];
        if let Ok(amount) = raw.parse::<f64>() {
            let cents = (amount * 100.0).round() as i64;
            return Some((cents.max(0), format_usd(cents)));
        }
    }
    for marker in ["price-wrapper", "product-info-price", "price-box"] {
        if let Some(idx) = html.find(marker) {
            let window = &html[idx..html.len().min(idx + 800)];
            if let Some(cents) = window
                .split('$')
                .nth(1)
                .and_then(|s| parse_money_label_to_cents(&format!("${}", &s[..s.len().min(12)])))
            {
                return Some((cents, format_usd(cents)));
            }
        }
    }
    None
}

/// Parses `$12.34` / `FREE` into integer cents; commas ignored, first number wins.
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

/// Formats cents as `$D.CC`, or `FREE` when the amount is zero or negative.
fn format_usd(cents: i64) -> String {
    if cents <= 0 {
        return String::from("FREE");
    }
    format!("${}.{:02}", cents / 100, (cents % 100).unsigned_abs())
}
