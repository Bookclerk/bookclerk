//! Shared oracle for the `graphicaudio_catalog` fuzz target and the
//! `bookclerk-plugin-source-graphicaudio` corpus replay test
//! (`docs/adr/untrusted-input-fuzzing.md`).
//!
//! The input is one GraphicAudio product page, as the public Magento store
//! sends it to the jailed `graphicaudio` guest (decoded as the guest's HTTP
//! client decodes a body, replacing invalid UTF-8). The invariants:
//!
//! - parsing never panics, and parsing the same page twice gives the same
//!   record;
//! - a page that carries neither Magento's `catalog_product_view_id_` tag nor
//!   a `data-product-id` attribute (checked on the raw text, not through the
//!   HTML parser) yields no record;
//! - a record's product id is 1 to 20 ASCII digits that appear in the page,
//!   and its title is non-blank, trimmed and on one line;
//! - every credit name and the genre are non-blank, trimmed and on one line;
//! - an ISBN is 13 ASCII digits;
//! - the running time is 1 to 10,080 minutes, the release date is
//!   `YYYY-MM-DD`, the score is a finite number from 0 to 5 and a review
//!   count only comes with a score;
//! - a price is 0 to 10,000,000 cents with a three-letter upper-case
//!   currency, and its label is the dollar form of those cents (USD only);
//! - the product URL and the cover are absolute `https` links;
//! - the synopsis is non-blank with trimmed lines and no runs of blank lines.

use bookclerk_plugin_source_graphicaudio::parse_product_detail;
use bookclerk_storefront::records::IdentifierScheme;

/// Whether `text` is non-blank, trimmed and on one line.
fn one_line(text: &str) -> bool {
    !text.is_empty() && text.trim() == text && !text.contains(['\n', '\r'])
}

/// Checks one input against every invariant.
///
/// # Panics
///
/// Panics when an invariant does not hold.
pub fn check(data: &[u8]) {
    let html = String::from_utf8_lossy(data);
    let hit = parse_product_detail(&html);
    let again = parse_product_detail(&html);
    assert_eq!(
        serde_json::to_string(&hit).ok(),
        serde_json::to_string(&again).ok(),
        "two parses of one page differ"
    );
    let Some(hit) = hit else { return };
    assert!(
        html.contains("catalog_product_view_id_") || html.contains("data-product-id"),
        "a record from a page with no product id marker"
    );

    let id = &hit.product_id;
    assert!(
        (1..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit()),
        "product id {id:?}"
    );
    assert!(
        html.contains(id.as_str()),
        "product id {id} not in the page"
    );
    assert!(one_line(&hit.title), "title {:?}", hit.title);

    for credit in &hit.credits {
        assert!(one_line(&credit.name), "credit {:?}", credit.name);
    }
    if let Some(genre) = &hit.categories {
        assert!(one_line(genre), "genre {genre:?}");
    }
    for id in &hit.identifiers {
        assert_eq!(id.scheme, IdentifierScheme::Isbn, "only an ISBN is read");
        assert!(
            id.value.len() == 13 && id.value.bytes().all(|b| b.is_ascii_digit()),
            "ISBN {:?}",
            id.value
        );
    }
    if let Some(minutes) = hit.length_minutes {
        assert!((1..=7 * 24 * 60).contains(&minutes), "length {minutes}");
    }
    if let Some(date) = &hit.published_at {
        let b = date.as_bytes();
        assert!(
            b.len() == 10
                && b[4] == b'-'
                && b[7] == b'-'
                && b.iter()
                    .enumerate()
                    .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit()),
            "release date {date:?}"
        );
    }
    if let Some(score) = hit.rating_overall {
        assert!(
            score.is_finite() && (0.0..=5.0).contains(&score),
            "score {score}"
        );
    }
    if let Some(count) = hit.rating_count {
        assert!(
            hit.rating_overall.is_some(),
            "a review count without a score"
        );
        assert!(count >= 0, "review count {count}");
    }
    match (hit.price_cents, &hit.currency) {
        (None, None) => assert!(hit.price_label.is_none(), "a label without a price"),
        (Some(cents), Some(currency)) => {
            assert!((0..=10_000_000).contains(&cents), "price {cents}");
            assert!(
                currency.len() == 3 && currency.bytes().all(|b| b.is_ascii_uppercase()),
                "currency {currency:?}"
            );
            let expected =
                (currency == "USD").then(|| format!("${}.{:02}", cents / 100, cents % 100));
            assert_eq!(hit.price_label, expected, "price label");
        }
        other => panic!("a price without a currency, or the reverse: {other:?}"),
    }
    for link in [&hit.url, &hit.cover_url].into_iter().flatten() {
        assert!(
            link.starts_with("https://") && link.len() > "https://".len(),
            "link {link:?}"
        );
    }
    if let Some(text) = &hit.description {
        assert!(!text.trim().is_empty(), "blank synopsis");
        assert!(!text.contains("\n\n\n"), "a run of blank lines");
        for line in text.split('\n') {
            assert_eq!(line.trim(), line, "untrimmed synopsis line {line:?}");
        }
    }
}
