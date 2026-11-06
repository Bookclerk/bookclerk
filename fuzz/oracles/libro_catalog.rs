//! Shared oracle for the `libro_catalog` fuzz target and the
//! `bookclerk-plugin-source-libro` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one Libro.fm catalog answer, as the store's app API sends it
//! to the jailed `libro` guest. The first byte picks the parser: an even byte
//! is an `explore/search` page (the second byte is the row limit), an odd
//! byte an `explore/audiobook_details` body. The rest is the body.
//!
//! The invariants, checked against a walk of `serde_json`'s own `Value`
//! rather than the plugin's mapping:
//!
//! - parsing never panics, and accepts only valid JSON (invalid UTF-8 in
//!   a field the parser skips aside);
//! - the body re-serialized from its `Value` (no duplicate keys) parses
//!   exactly when its envelope has the documented shape: objects where the
//!   API sends objects, and a list (or nothing) where it sends rows;
//! - an accepted page keeps exactly the first `limit` rows that carry an ISBN
//!   (a string or an integer, not blank) and a title (`title`, or `name` when
//!   there is no `title`, not blank), in order;
//! - each hit keeps that row's trimmed ISBN and title, has a non-blank
//!   product id and a storefront URL, and never a scheme-relative cover;
//! - a details body's audiobook, related titles and sale flag follow the same
//!   rows and `purchase_info.for_sale` (missing means for sale), and a price
//!   appears only when `purchase_info.price` does, always in USD;
//! - parsing the same body twice gives the same records.

use bookclerk_plugin_source_libro::{parse_catalog_details, parse_catalog_search_page};
use bookclerk_source::records::IdentifierScheme;
use bookclerk_source::CatalogHit;
use serde_json::{Map, Value};

/// The ISBN and title a row must yield, or `None` when the row is skipped.
fn expected_row(row: &Value) -> Option<(String, String)> {
    let obj = row.as_object()?;
    let isbn = match obj.get("isbn")? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        _ => return None,
    };
    let title = match obj.get("title") {
        Some(t) => t.as_str()?,
        None => obj.get("name")?.as_str()?,
    }
    .trim()
    .to_string();
    (!isbn.is_empty() && !title.is_empty()).then_some((isbn, title))
}

/// Checks one hit against the row it came from.
fn check_hit(hit: &CatalogHit, (isbn, title): &(String, String)) {
    assert_eq!(&hit.title, title, "title differs from the row");
    assert!(
        hit.identifiers
            .iter()
            .any(|id| id.scheme == IdentifierScheme::Isbn && id.value == *isbn),
        "ISBN {isbn} not kept"
    );
    assert!(!hit.product_id.trim().is_empty(), "blank product id");
    assert!(hit.url.is_some(), "hit without a storefront URL");
    if let Some(cover) = &hit.cover_url {
        assert!(!cover.starts_with("//"), "scheme-relative cover {cover}");
    }
}

/// Checks hits against the rows the oracle expects, in order.
fn check_hits(hits: &[CatalogHit], rows: &[(String, String)]) {
    assert_eq!(
        hits.len(),
        rows.len(),
        "kept {} of {} rows",
        hits.len(),
        rows.len()
    );
    for (hit, row) in hits.iter().zip(rows) {
        check_hit(hit, row);
    }
}

/// What the parser must do with one envelope.
enum Shape<T> {
    /// The envelope is documented; the parser must accept it with these facts.
    Accept(T),
    /// A field has a type the API never sends; the parser must refuse it.
    Refuse,
    /// A form `serde` also reads (a struct as a list); not judged.
    Unjudged,
}

/// The rows a list field yields: absent is none, a list its rows, anything
/// else a refusal.
fn rows_of(obj: &Map<String, Value>, key: &str) -> Option<Vec<(String, String)>> {
    match obj.get(key) {
        None => Some(Vec::new()),
        Some(Value::Array(rows)) => Some(rows.iter().filter_map(expected_row).collect()),
        Some(_) => None,
    }
}

/// The rows a search page must yield, before the limit.
fn search_shape(value: &Value) -> Shape<Vec<(String, String)>> {
    let Some(top) = value.as_object() else {
        return Shape::Unjudged;
    };
    match top.get("audiobook_collection") {
        None | Some(Value::Null) => Shape::Accept(Vec::new()),
        Some(Value::Object(collection)) => match rows_of(collection, "audiobooks") {
            Some(rows) => Shape::Accept(rows),
            None => Shape::Refuse,
        },
        Some(Value::Array(_)) => Shape::Unjudged,
        Some(_) => Shape::Refuse,
    }
}

/// Facts a details body must yield.
struct DetailsFacts {
    /// The primary audiobook's row, when it has one.
    audiobook: Option<(String, String)>,
    /// Related rows, in order.
    related: Vec<(String, String)>,
    /// `purchase_info.for_sale`, defaulting to true.
    for_sale: bool,
    /// Whether `purchase_info.price` is present.
    has_price: bool,
}

/// The facts a details body must yield.
fn details_shape(value: &Value) -> Shape<DetailsFacts> {
    let Some(top) = value.as_object() else {
        return Shape::Unjudged;
    };
    let data = match top.get("data") {
        None | Some(Value::Null) => {
            return Shape::Accept(DetailsFacts {
                audiobook: None,
                related: Vec::new(),
                for_sale: true,
                has_price: false,
            })
        }
        Some(Value::Object(data)) => data,
        Some(Value::Array(_)) => return Shape::Unjudged,
        Some(_) => return Shape::Refuse,
    };
    let Some(related) = rows_of(data, "related_audiobooks") else {
        return Shape::Refuse;
    };
    let purchase = data.get("purchase_info").filter(|p| !p.is_null());
    Shape::Accept(DetailsFacts {
        audiobook: data.get("audiobook").and_then(expected_row),
        related,
        for_sale: purchase
            .and_then(|p| p.get("for_sale"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        has_price: purchase.and_then(|p| p.get("price")).is_some(),
    })
}

/// Whether `body` is JSON once invalid UTF-8 is replaced. `serde_json` skips
/// the fields a parser ignores without checking their UTF-8, so such a body
/// may parse; the fields it reads are always checked.
fn only_bad_utf8(body: &[u8]) -> bool {
    serde_json::from_str::<Value>(&String::from_utf8_lossy(body)).is_ok()
}

/// Serialized records, for the determinism check.
fn as_json<T: serde::Serialize>(records: &T) -> String {
    serde_json::to_string(records).unwrap_or_default()
}

/// Runs every invariant on one input; panics on a violation.
pub fn check(data: &[u8]) {
    let Some((&selector, rest)) = data.split_first() else {
        return;
    };
    if selector % 2 == 0 {
        let Some((&limit, body)) = rest.split_first() else {
            return;
        };
        check_search(body, usize::from(limit));
    } else {
        check_details(rest);
    }
}

/// Search invariants on one body; the canonical pass judges acceptance.
fn search_case(body: &[u8], limit: usize, canonical: bool) {
    let parsed = parse_catalog_search_page(body, limit);
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        assert!(
            parsed.is_err() || only_bad_utf8(body),
            "invalid JSON parsed as a search page"
        );
        return;
    };
    match (search_shape(&value), parsed) {
        (Shape::Accept(rows), Ok(hits)) => {
            check_hits(&hits, &rows[..rows.len().min(limit)]);
            let again = parse_catalog_search_page(body, limit).unwrap_or_default();
            assert_eq!(as_json(&hits), as_json(&again), "search not deterministic");
        }
        (Shape::Accept(_), Err(err)) => {
            assert!(!canonical, "a documented search page was refused: {err}");
        }
        (Shape::Refuse, Ok(_)) => {
            assert!(!canonical, "a malformed search page was accepted");
        }
        (Shape::Refuse | Shape::Unjudged, _) => {}
    }
}

/// Invariants of one search page and of its re-serialized form.
fn check_search(body: &[u8], limit: usize) {
    search_case(body, limit, false);
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Ok(canonical) = serde_json::to_vec(&value) {
            search_case(&canonical, limit, true);
        }
    }
}

/// Details invariants on one body; the canonical pass judges acceptance.
fn details_case(body: &[u8], canonical: bool) {
    let parsed = parse_catalog_details(body);
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        assert!(
            parsed.is_err() || only_bad_utf8(body),
            "invalid JSON parsed as details"
        );
        return;
    };
    match (details_shape(&value), parsed) {
        (Shape::Accept(facts), Ok(details)) => {
            assert_eq!(details.for_sale, facts.for_sale, "sale flag differs");
            assert_eq!(
                details.audiobook.is_some(),
                facts.audiobook.is_some(),
                "audiobook"
            );
            if let (Some(hit), Some(row)) = (&details.audiobook, &facts.audiobook) {
                check_hit(hit, row);
                if hit.price_cents.is_some() {
                    assert!(facts.has_price, "a price without purchase_info.price");
                    assert_eq!(hit.currency.as_deref(), Some("USD"), "price not in USD");
                }
            }
            check_hits(&details.related, &facts.related);
            let again = parse_catalog_details(body).ok();
            assert_eq!(
                as_json(&(&details.audiobook, &details.related, details.for_sale)),
                as_json(&again.map(|d| (d.audiobook, d.related, d.for_sale))),
                "details not deterministic"
            );
        }
        (Shape::Accept(_), Err(err)) => {
            assert!(!canonical, "a documented details body was refused: {err}");
        }
        (Shape::Refuse, Ok(_)) => {
            assert!(!canonical, "a malformed details body was accepted");
        }
        (Shape::Refuse | Shape::Unjudged, _) => {}
    }
}

/// Invariants of one details body and of its re-serialized form.
fn check_details(body: &[u8]) {
    details_case(body, false);
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        if let Ok(canonical) = serde_json::to_vec(&value) {
            details_case(&canonical, true);
        }
    }
}
