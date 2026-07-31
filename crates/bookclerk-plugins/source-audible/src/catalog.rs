//! Public Audible catalog helpers for Discover (`search_catalog` / expand / purchase).
//!
//! Uses [`bookclerk_enrich`] public catalog HTTP — no account required.

use bookclerk_enrich::{
    is_valid_asin, normalize_region, public_http_client, region_tld, search_catalog_asins,
    search_catalog_by_narrator, search_catalog_by_series_asin, search_catalog_keywords,
    search_catalog_products, CatalogProduct,
};
use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, SourcePurchaseHint,
};
use serde_json::Value;

/// Search the public Audible catalog (title + keyword overlap, like Discover typeahead).
pub async fn search_catalog(opts: &CatalogSearchOpts) -> bookclerk_source::Result<Vec<CatalogHit>> {
    let q = opts.query.trim();
    if q.is_empty() || opts.limit == 0 {
        return Ok(Vec::new());
    }
    let http =
        public_http_client().map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    let region = normalize_region(&opts.region);
    let products = search_catalog_products(&http, &region, q, None, Some(q))
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    Ok(products
        .into_iter()
        .take(opts.limit)
        .filter(|p| !p.asin.trim().is_empty())
        .map(|p| product_to_hit(p, String::from("search")))
        .collect())
}

/// Expand author / series / narrator / series-ASIN candidates from a taste seed.
///
/// Soft-fails individual HTTP calls; budgets returned hits by `limit`.
pub async fn expand_candidates(
    seed: &ExpandSeed,
    limit: usize,
) -> bookclerk_source::Result<Vec<CatalogHit>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let http = match public_http_client() {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };
    let region = if seed.region.trim().is_empty() {
        String::from("us")
    } else {
        normalize_region(&seed.region)
    };
    let mut by_asin = std::collections::HashMap::new();

    let push = |by_asin: &mut std::collections::HashMap<String, CatalogHit>,
                p: CatalogProduct,
                origin: String| {
        if p.asin.trim().is_empty() {
            return;
        }
        by_asin
            .entry(p.asin.to_ascii_uppercase())
            .or_insert_with(|| product_to_hit(p, origin));
    };

    // Exact series ASIN listing (strongest series signal).
    if let Some(series_asin) = seed
        .series_asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        match search_catalog_by_series_asin(&http, &region, series_asin).await {
            Ok(products) => {
                let series_label = seed
                    .series
                    .clone()
                    .unwrap_or_else(|| series_asin.to_string());
                for mut p in products {
                    if p.series.is_none() {
                        p.series = Some(series_label.clone());
                    }
                    push(
                        &mut by_asin,
                        p,
                        format!("audible series ASIN ({series_asin})"),
                    );
                }
            }
            Err(err) => {
                tracing::debug!(
                    series_asin,
                    error = %err,
                    "audible series_asin search failed"
                );
            }
        }
    }

    // More by same author.
    if by_asin.len() < limit {
        if let Some(author) = primary_person(seed.authors.as_deref()) {
            match search_catalog_products(&http, &region, "", Some(author), None).await {
                Ok(products) => {
                    for mut p in products {
                        if p.authors.is_none() {
                            p.authors = Some(author.to_string());
                        }
                        push(&mut by_asin, p, format!("audible author search ({author})"));
                    }
                }
                Err(err) => {
                    tracing::debug!(author, error = %err, "audible author search failed");
                }
            }
        }
    }

    // Series keyword expansion when we lack series_asin.
    if by_asin.len() < limit && seed.series_asin.as_deref().is_none_or(str::is_empty) {
        if let Some(series) = seed
            .series
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match search_catalog_products(&http, &region, "", None, Some(series)).await {
                Ok(products) => {
                    for mut p in products {
                        if p.series.is_none() {
                            p.series = Some(series.to_string());
                        }
                        push(
                            &mut by_asin,
                            p,
                            format!("audible series search (“{series}”)"),
                        );
                    }
                }
                Err(err) => {
                    tracing::debug!(series, error = %err, "audible series search failed");
                }
            }
        }
    }

    // Narrator search when narrators present (noisier; still useful for Audible seeds).
    if by_asin.len() < limit {
        if let Some(narrator) = primary_person(seed.narrators.as_deref()) {
            match search_catalog_by_narrator(&http, &region, narrator).await {
                Ok(products) => {
                    for p in products {
                        push(
                            &mut by_asin,
                            p,
                            format!("audible narrator search ({narrator})"),
                        );
                    }
                }
                Err(err) => {
                    tracing::debug!(narrator, error = %err, "audible narrator search failed");
                }
            }
        }
    }

    // When the seed is an Audible ASIN (or product_id looks like one) and we still
    // have room, keyword-search the title for near-neighbors.
    if by_asin.len() < limit {
        let seed_asin = seed
            .asin
            .as_deref()
            .map(str::trim)
            .filter(|s| is_valid_asin(s))
            .or_else(|| {
                let pid = seed.product_id.trim();
                if seed.source.eq_ignore_ascii_case("audible") && is_valid_asin(pid) {
                    Some(pid)
                } else {
                    None
                }
            });
        if seed_asin.is_some() {
            let title = seed.title.trim();
            if !title.is_empty() {
                match search_catalog_products(&http, &region, title, None, Some(title)).await {
                    Ok(products) => {
                        for p in products {
                            // Skip the seed itself.
                            if seed_asin.is_some_and(|a| p.asin.eq_ignore_ascii_case(a)) {
                                continue;
                            }
                            push(
                                &mut by_asin,
                                p,
                                format!("audible related search (“{}”)", seed.title),
                            );
                        }
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "audible related title search failed");
                    }
                }
            }
        }
    }

    let mut hits: Vec<_> = by_asin.into_values().collect();
    hits.truncate(limit);
    Ok(hits)
}

/// Resolve a purchase / catalog URL (optionally with live price).
pub async fn purchase_hint(
    opts: &PurchaseHintOpts,
) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
    let region = normalize_region(&opts.region);
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

    let mut asin = opts
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| is_valid_asin(s))
        .map(|s| s.to_ascii_uppercase())
        .or_else(|| {
            opts.product_id
                .as_deref()
                .map(str::trim)
                .filter(|s| is_valid_asin(s))
                .map(|s| s.to_ascii_uppercase())
        });

    if asin.is_none() {
        let http = match public_http_client() {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        if let Some(t) = title {
            match search_catalog_asins(&http, &region, t, author).await {
                Ok(asins) => asin = asins.into_iter().next().map(|a| a.to_ascii_uppercase()),
                Err(err) => {
                    tracing::debug!(error = %err, "audible catalog asin search failed");
                }
            }
            if asin.is_none() {
                if let Some(isbn) = opts
                    .isbn
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    match search_catalog_keywords(&http, &region, isbn).await {
                        Ok(asins) => {
                            asin = asins.into_iter().next().map(|a| a.to_ascii_uppercase())
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "audible isbn keyword search failed");
                        }
                    }
                }
            }
        } else if let Some(isbn) = opts
            .isbn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            match search_catalog_keywords(&http, &region, isbn).await {
                Ok(asins) => asin = asins.into_iter().next().map(|a| a.to_ascii_uppercase()),
                Err(err) => {
                    tracing::debug!(error = %err, "audible isbn keyword search failed");
                }
            }
        }
    }

    let Some(asin) = asin else {
        return Ok(None);
    };

    let mut hint = SourcePurchaseHint {
        product_id: asin.clone(),
        title: title.map(str::to_string),
        url: Some(format!(
            "https://www.audible{}/pd/{}",
            region_host_suffix(&region),
            asin
        )),
        price_cents: None,
        currency: None,
        price_label: None,
    };

    if opts.with_price {
        if let Some(priced) = fetch_audible_price(&asin, &region).await {
            hint.price_cents = Some(priced.cents.max(0));
            hint.currency = Some(priced.currency);
            hint.price_label = Some(priced.label);
        }
    }

    Ok(Some(hint))
}

fn product_to_hit(p: CatalogProduct, origin: String) -> CatalogHit {
    CatalogHit {
        product_id: p.asin.clone(),
        title: p.title.unwrap_or_else(|| p.asin.clone()),
        authors: p.authors,
        narrators: p.narrators,
        series: p.series,
        series_index: p.series_sequence,
        asin: Some(p.asin),
        isbn: None,
        url: None,
        origin,
    }
}

fn primary_person(people: Option<&str>) -> Option<&str> {
    people?
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

fn region_host_suffix(region: &str) -> &'static str {
    match normalize_region(region).as_str() {
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

struct Priced {
    cents: i64,
    currency: String,
    label: String,
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
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
