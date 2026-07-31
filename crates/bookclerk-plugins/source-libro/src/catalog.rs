//! Public Libro.fm explore catalog helpers for Discover.
//!
//! Mirrors former `bookclerk-discover` explore HTTP (no account required).
//! This crate intentionally does **not** depend on `bookclerk-enrich`.

use std::time::Duration;

use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts, SourcePurchaseHint,
};
use serde::Deserialize;
use serde_json::Value;

const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

fn public_http_client() -> Result<reqwest::Client, bookclerk_source::SourceError> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .user_agent(concat!("bookclerk/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))
}

/// Explore search → [`CatalogHit`]s keyed by ISBN.
pub async fn search_catalog(opts: &CatalogSearchOpts) -> bookclerk_source::Result<Vec<CatalogHit>> {
    let q = opts.query.trim();
    if q.is_empty() || opts.limit == 0 {
        return Ok(Vec::new());
    }
    let http = public_http_client()?;
    let url = format!(
        "https://libro.fm/explore/search?page=1&q={}",
        urlencode_minimal(q)
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    if !resp.status().is_success() {
        return Ok(Vec::new());
    }
    let body: LibroExploreSearch = match resp.json().await {
        Ok(b) => b,
        Err(_) => return Ok(Vec::new()),
    };
    let books = body
        .audiobook_collection
        .map(|c| c.audiobooks)
        .unwrap_or_default();
    Ok(books
        .into_iter()
        .take(opts.limit)
        .filter_map(|book| {
            let isbn = book.isbn.filter(|s| !s.is_empty())?;
            let title = book
                .title
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| isbn.clone());
            Some(CatalogHit {
                product_id: isbn.clone(),
                title,
                authors: book.authors.filter(|s| !s.is_empty()),
                narrators: None,
                series: None,
                series_index: None,
                asin: None,
                isbn: Some(isbn),
                url: None,
                origin: String::from("search"),
            })
        })
        .collect())
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

    let http = match public_http_client() {
        Ok(c) => c,
        Err(_) => return Ok(Vec::new()),
    };
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

/// Resolve a Libro.fm purchase URL (ISBN or explore search), optionally priced.
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

    let mut hint = if let Some(isbn) = opts
        .isbn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            opts.product_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
        }) {
        Some(SourcePurchaseHint {
            product_id: isbn.to_string(),
            title: title.map(str::to_string),
            url: Some(format!("https://libro.fm/audiobooks/{isbn}")),
            price_cents: None,
            currency: None,
            price_label: None,
        })
    } else if let Some(t) = title {
        let http = match public_http_client() {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        match libro_explore_search(&http, t, author).await {
            Ok(h) => h,
            Err(err) => {
                tracing::debug!(error = %err, "libro explore search failed");
                None
            }
        }
    } else {
        None
    };

    if opts.with_price {
        if let Some(ref mut h) = hint {
            if let Some(priced) = fetch_libro_price(&h.product_id).await {
                h.price_cents = Some(priced.cents.max(0));
                h.currency = Some(priced.currency);
                h.price_label = Some(priced.label);
            }
        }
    }

    Ok(hint)
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
    #[serde(default)]
    authors: Option<String>,
    slug: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LibroDetailsResponse {
    #[serde(default)]
    data: Option<LibroDetailsData>,
}

#[derive(Debug, Deserialize)]
struct LibroDetailsData {
    #[serde(default)]
    related_audiobooks: Vec<Value>,
}

async fn libro_related(
    http: &reqwest::Client,
    isbn: &str,
) -> bookclerk_source::Result<Vec<CatalogHit>> {
    let url = format!("https://libro.fm/explore/audiobook_details/{isbn}");
    let resp = http
        .get(&url)
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
    Ok(related.iter().filter_map(parse_libro_book).collect())
}

fn parse_libro_book(v: &Value) -> Option<CatalogHit> {
    let isbn = v
        .get("isbn")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?
        .to_string();
    let authors = v
        .get("authors")
        .and_then(|a| {
            if let Some(s) = a.as_str() {
                Some(s.to_string())
            } else if let Some(arr) = a.as_array() {
                let names: Vec<&str> = arr
                    .iter()
                    .filter_map(|x| x.as_str().or_else(|| x.get("name").and_then(Value::as_str)))
                    .collect();
                if names.is_empty() {
                    None
                } else {
                    Some(names.join(", "))
                }
            } else {
                None
            }
        })
        .or_else(|| v.get("author").and_then(Value::as_str).map(str::to_string));
    let narrators = v
        .get("narrators")
        .and_then(Value::as_str)
        .map(str::to_string);
    let series = v.get("series").and_then(|s| {
        s.as_str()
            .map(str::to_string)
            .or_else(|| s.get("name").and_then(Value::as_str).map(str::to_string))
    });
    Some(CatalogHit {
        product_id: isbn.clone(),
        title,
        authors,
        narrators,
        series,
        series_index: None,
        asin: None,
        isbn: Some(isbn),
        url: None,
        origin: String::from("libro related"),
    })
}

async fn libro_explore_search(
    http: &reqwest::Client,
    title: &str,
    author: Option<&str>,
) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
    let q = match author {
        Some(a) if !a.is_empty() => format!("{title} {a}"),
        _ => title.to_string(),
    };
    let url = format!(
        "https://libro.fm/explore/search?page=1&q={}",
        urlencode_minimal(&q)
    );
    let resp = http
        .get(&url)
        .send()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
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
    Ok(Some(SourcePurchaseHint {
        product_id: isbn,
        title: book.title,
        url,
        price_cents: None,
        currency: None,
        price_label: None,
    }))
}

struct Priced {
    cents: i64,
    currency: String,
    label: String,
}

async fn fetch_libro_price(isbn_or_slug: &str) -> Option<Priced> {
    let http = public_http_client().ok()?;
    let url = format!("https://libro.fm/explore/audiobook_details/{isbn_or_slug}");
    let resp = http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    find_price_in_json(&body)
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
            });
        }
    }
    if let Some(n) = v.as_f64() {
        let cents = if n >= 1000.0 {
            n.round() as i64
        } else {
            (n * 100.0).round() as i64
        };
        return Some(Priced {
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
            return Some(Priced {
                cents: cents.max(0),
                currency: currency.to_string(),
                label: format_money_label(cents, currency),
            });
        }
    }
    None
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
    fn parse_libro_related_object() {
        let v = json!({
            "isbn": "9781234567890",
            "title": "Next Title",
            "authors": [{"name": "Ada Author"}],
            "series": {"name": "Test Series"}
        });
        let c = parse_libro_book(&v).unwrap();
        assert_eq!(c.isbn.as_deref(), Some("9781234567890"));
        assert_eq!(c.authors.as_deref(), Some("Ada Author"));
        assert_eq!(c.series.as_deref(), Some("Test Series"));
    }

    #[test]
    fn money_label_parsing() {
        assert_eq!(parse_money_label_to_cents("$2.99"), Some(299));
        assert_eq!(parse_money_label_to_cents("FREE"), Some(0));
        assert_eq!(format_money_label(299, "USD"), "$2.99");
    }
}
