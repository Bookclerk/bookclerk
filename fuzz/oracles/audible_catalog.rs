//! Shared oracle for the `audible_catalog` fuzz target and the
//! `bookclerk-audible-catalog` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one answer from Audible's public catalog, as the store sends
//! it to the `audible` guest's Discover search and expansion and to host
//! enrichment. The first byte picks the list: an even byte reads `products`
//! (a search), an odd byte `similar_products` (a title's similar titles).
//! The rest is the body.
//!
//! The invariants, checked against a walk of `serde_json`'s own `Value`
//! rather than the crate's mapping:
//!
//! - parsing never panics, and accepts exactly the bodies that are JSON;
//! - an accepted answer whose list is missing or not an array has no titles;
//! - otherwise the titles are exactly the list's rows that are objects with a
//!   non-empty string `asin` and are not podcasts, in order, each keeping its
//!   row's ASIN;
//! - a title's genres are exactly the trimmed, non-blank rung names of its
//!   row's `category_ladders`, first spelling of each kept (case-insensitive),
//!   joined with `; `, and absent when there are none: nothing else in the
//!   row (such as `thesaurus_subject_keywords`, Audible's internal
//!   merchandising flags) ever becomes a genre;
//! - parsing the same body twice gives the same titles.

use bookclerk_audible_catalog::{parse_catalog_products, CatalogList, CatalogProduct};
use serde_json::Value;

/// The genres a row must yield, restated from the catalog's ladder shape.
fn expected_genres(row: &Value) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    let ladders = row.get("category_ladders").and_then(Value::as_array);
    for ladder in ladders.into_iter().flatten() {
        let rungs = match ladder {
            Value::Object(obj) => obj.get("ladder").and_then(Value::as_array),
            Value::Array(rungs) => Some(rungs),
            _ => None,
        };
        for rung in rungs.into_iter().flatten() {
            let Some(name) = rung.get("name").and_then(Value::as_str) else {
                continue;
            };
            let name = name.trim();
            if !name.is_empty() && !names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
                names.push(name.to_string());
            }
        }
    }
    (!names.is_empty()).then(|| names.join("; "))
}

/// Whether a row is a podcast show or episode, restated from the catalog's
/// `content_type` and `content_delivery_type` values (case-insensitive; a
/// missing or non-string value is no kind).
fn is_podcast(row: &Value) -> bool {
    let kind = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
    };
    let content = kind("content_type");
    let delivery = kind("content_delivery_type");
    [
        "podcast",
        "podcastepisode",
        "episode",
        "parent",
        "podcastparent",
        "podcast_parent",
        "season",
    ]
    .contains(&content.as_str())
        || ["podcastparent", "podcastepisode", "episode"].contains(&delivery.as_str())
}

/// The ASIN of a row that must become a title, or `None` when it is skipped.
fn expected_asin(row: &Value) -> Option<&str> {
    if !row.is_object() || is_podcast(row) {
        return None;
    }
    row.get("asin")
        .and_then(Value::as_str)
        .filter(|a| !a.is_empty())
}

/// Checks one input against every invariant.
///
/// # Panics
///
/// Panics when the parser breaks an invariant.
pub fn check(data: &[u8]) {
    let Some((&selector, body)) = data.split_first() else {
        return;
    };
    let list = if selector % 2 == 0 {
        CatalogList::Products
    } else {
        CatalogList::SimilarProducts
    };
    let parsed = parse_catalog_products(body, list);
    let value = serde_json::from_slice::<Value>(body);
    let (titles, value) = match (parsed, value) {
        (Ok(titles), Ok(value)) => (titles, value),
        (Err(_), Err(_)) => return,
        (Ok(_), Err(err)) => panic!("accepted a body that is not JSON: {err}"),
        (Err(err), Ok(_)) => panic!("refused a JSON body: {err}"),
    };
    // The documented key of each list, written out rather than read from
    // `CatalogList::key`, so a wrong key in the crate is caught.
    let key = if selector % 2 == 0 {
        "products"
    } else {
        "similar_products"
    };
    let rows: Vec<&Value> = value
        .get(key)
        .and_then(Value::as_array)
        .map(|rows| rows.iter().collect())
        .unwrap_or_default();
    let expected: Vec<(&str, Option<String>)> = rows
        .into_iter()
        .filter_map(|row| expected_asin(row).map(|asin| (asin, expected_genres(row))))
        .collect();
    assert_eq!(
        titles.len(),
        expected.len(),
        "kept {} titles of {} rows",
        titles.len(),
        expected.len()
    );
    for (title, (asin, genres)) in titles.iter().zip(&expected) {
        check_title(title, asin, genres.as_deref());
    }
    // Compared through `Debug`: a rating of NaN (`"NaN"` parses as one) is
    // not equal to itself under `PartialEq`, but prints the same both times.
    let again = parse_catalog_products(body, list).unwrap_or_default();
    assert_eq!(
        format!("{titles:?}"),
        format!("{again:?}"),
        "parsing twice differs"
    );
}

/// Checks one title against the row it came from.
fn check_title(title: &CatalogProduct, asin: &str, genres: Option<&str>) {
    assert_eq!(title.asin, asin, "ASIN differs from the row");
    assert_eq!(
        title.categories.as_deref(),
        genres,
        "genres are not the row's ladder names"
    );
}
