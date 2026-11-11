//! GraphicAudio product pages read as full catalog records.
//!
//! Catalog search and series grids only show a title, a link and a cover. A
//! product page (`/catalog/product/view/id/<id>`) also shows the author, the
//! cast, the synopsis, the ISBN, the release date, the running time, the
//! genre, the review score and the price. [`parse_product_detail`] reads
//! those from the page's HTML without any I/O, so the `graphicaudio_catalog`
//! fuzz target and the unit tests drive the same code the guest runs.
//!
//! The page is untrusted store output: every field is optional, text is
//! trimmed and entity-decoded, numbers are range-checked, and only `https`
//! links are kept.

use bookclerk_storefront::records::{self, CreditRole, ExpressionKind, IdentifierScheme};
use bookclerk_storefront::CatalogHit;
use scraper::{ElementRef, Html, Node, Selector};

/// The publisher every GraphicAudio production is sold under.
const PUBLISHER: &str = "GraphicAudio";

/// Longest running time accepted from a page, in minutes (a week); a larger
/// value is a parse of something else and is dropped.
const MAX_LENGTH_MINUTES: i64 = 7 * 24 * 60;

/// Largest price accepted from a page, in cents ($100,000).
const MAX_PRICE_CENTS: i64 = 10_000_000;

/// The suffix GraphicAudio adds to page titles of its dramatisations.
const DRAMATIZED_SUFFIX: &str = "[Dramatized Adaptation]";

/// Reads one GraphicAudio product page as a catalog record.
///
/// # Arguments
///
/// * `html` - The product page's HTML, as the store sent it.
///
/// # Returns
///
/// The record, or `None` when the page names no numeric product id or no
/// title (a search page, an error page, or something else entirely).
#[must_use]
pub fn parse_product_detail(html: &str) -> Option<CatalogHit> {
    let doc = Html::parse_document(html);
    let product_id = product_id(&doc, html)?;
    let title = title(&doc)?;
    // Titles outside a series sit under a "Stand-Alone Title" pseudo-series.
    let series = first_text(&doc, "h1 .series-name").filter(|s| !is_stand_alone(s));
    let series_index = series.as_ref().and_then(|_| series_index(&doc));

    let mut credits = records::credits_from_names(
        texts(&doc, ".product-author [itemprop=\"author\"]"),
        CreditRole::Author,
    );
    // A dramatisation's voices are its narration: the flat views show them
    // as narrators, as the other stores list a full cast.
    credits.extend(records::credits_from_names(
        attribute_values(&doc, "Starring"),
        CreditRole::Narrator,
    ));

    let identifiers = labelled(&doc, ".product-isbn [aria-hidden=\"true\"]", "ISBN #:")
        .and_then(|raw| records::normalize_identifier(IdentifierScheme::Isbn, &raw))
        .filter(|isbn| isbn.len() == 13 && isbn.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|isbn| records::identifier(IdentifierScheme::Isbn, &isbn, None))
        .into_iter()
        .collect();

    let (rating_overall, rating_count) = rating(&doc);
    let (price_cents, currency, price_label) = price(&doc);

    Some(CatalogHit {
        product_id,
        title,
        url: meta(&doc, "meta[property=\"og:url\"]").filter(|u| is_https(u)),
        cover_url: attr(&doc, "link[itemprop=\"image\"]", "href").filter(|u| is_https(u)),
        origin: String::from("details"),
        description: description(&doc),
        publisher: Some(String::from(PUBLISHER)),
        length_minutes: labelled(&doc, ".product-runningtime", "Approximate Running Time:")
            .and_then(|t| running_minutes(&t)),
        published_at: labelled(&doc, ".product-releasedate", "Release Date:")
            .and_then(|t| release_date(&t)),
        categories: labelled(&doc, ".product-genre", "Genre:"),
        price_cents,
        currency,
        price_label,
        rating_overall,
        rating_count,
        expression_kind: ExpressionKind::Dramatisation,
        identifiers,
        credits,
        series_entries: records::series_entries(series.as_deref(), series_index.as_deref()),
        ..Default::default()
    })
}

/// The page's own product id: Magento's page-cache tag first (it names only
/// the viewed product), else the first `data-product-id`. Digits only.
fn product_id(doc: &Html, html: &str) -> Option<String> {
    let marker = "catalog_product_view_id_";
    let from_tag = html.find(marker).map(|idx| {
        html[idx + marker.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
    });
    from_tag
        .filter(|id| !id.is_empty())
        .or_else(|| {
            attr(doc, "[data-product-id]", "data-product-id")
                .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter(|id| !id.is_empty() && id.len() <= 20)
}

/// The product's title: the page's `title` meta without the dramatisation
/// suffix, else the episode name under the heading.
fn title(doc: &Html) -> Option<String> {
    meta(doc, "meta[name=\"title\"]")
        .map(|t| {
            t.strip_suffix(DRAMATIZED_SUFFIX)
                .unwrap_or(&t)
                .trim()
                .to_string()
        })
        .filter(|t| !t.is_empty())
        .or_else(|| first_text(doc, "h1 .episode-name"))
}

/// Whether a series name is GraphicAudio's label for titles in no series.
fn is_stand_alone(name: &str) -> bool {
    let key: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    key.starts_with("standalone")
}

/// The leading number of the episode name (`1 : The Final Empire (1 of 3)`),
/// read without its screen-reader prefix.
fn series_index(doc: &Html) -> Option<String> {
    let sel = Selector::parse("h1 .episode-name").ok()?;
    let el = doc.select(&sel).next()?;
    let own: String = el
        .children()
        .filter_map(|child| match child.value() {
            Node::Text(t) => Some(t.to_string()),
            _ => None,
        })
        .collect();
    let digits: String = own
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty() && digits.len() <= 4).then_some(digits)
}

/// The synopsis as plain text: `<br>` becomes a line break, blank runs
/// collapse, and markup is dropped.
fn description(doc: &Html) -> Option<String> {
    let sel = Selector::parse(".product-description").ok()?;
    let el = doc.select(&sel).next()?;
    let mut raw = String::new();
    for node in el.descendants() {
        match node.value() {
            Node::Text(t) => raw.push_str(t),
            Node::Element(e) if e.name() == "br" => raw.push('\n'),
            _ => {}
        }
    }
    let mut out = String::new();
    let mut blank = false;
    for line in raw.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        out.push_str(&line);
        blank = false;
    }
    (!out.is_empty()).then_some(out)
}

/// The review score out of five and the review count, when the page shows
/// them and they are in range.
fn rating(doc: &Html) -> (Option<f64>, Option<i64>) {
    let score = attr(doc, ".aw-ar-rating-container[title]", "title")
        .and_then(|t| t.split_whitespace().next().map(str::to_string))
        .and_then(|n| n.parse::<f64>().ok())
        .filter(|n| n.is_finite() && (0.0..=5.0).contains(n));
    let count = texts(doc, ".aw-advanced-reviews-summary-container a.action.view")
        .into_iter()
        .find_map(|t| {
            let open = t.find('(')?;
            let digits: String = t[open + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<i64>().ok()
        })
        .filter(|n| (0..=100_000_000).contains(n));
    (score, count.filter(|_| score.is_some()))
}

/// The price the page offers, from its `product:price` metas.
fn price(doc: &Html) -> (Option<i64>, Option<String>, Option<String>) {
    let currency = meta(doc, "meta[property=\"product:price:currency\"]")
        .filter(|c| c.len() == 3 && c.bytes().all(|b| b.is_ascii_uppercase()));
    let cents = meta(doc, "meta[property=\"product:price:amount\"]")
        .and_then(|a| decimal_cents(&a))
        .filter(|c| (0..=MAX_PRICE_CENTS).contains(c));
    match (cents, currency) {
        (Some(cents), Some(currency)) => {
            let label = (currency == "USD").then(|| format!("${}.{:02}", cents / 100, cents % 100));
            (Some(cents), Some(currency), label)
        }
        _ => (None, None, None),
    }
}

/// Cents of a plain decimal amount (`9.9`, `24.99`, `10`); `None` for any
/// other form or more than two decimals.
fn decimal_cents(text: &str) -> Option<i64> {
    let (whole, frac) = text.trim().split_once('.').unwrap_or((text.trim(), ""));
    if whole.is_empty()
        || whole.len() > 9
        || frac.len() > 2
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !frac.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: i64 = whole.parse().ok()?;
    let frac: i64 = match frac.len() {
        0 => 0,
        1 => frac.parse::<i64>().ok()? * 10,
        _ => frac.parse().ok()?,
    };
    whole.checked_mul(100)?.checked_add(frac)
}

/// Minutes of a running time such as `6 Hours`, `6.5 Hours` or
/// `5 Hours 30 Minutes`; `None` when no hours or minutes are named, or the
/// total is not between one minute and a week.
fn running_minutes(text: &str) -> Option<i64> {
    let words: Vec<String> = text
        .split_whitespace()
        .map(str::to_ascii_lowercase)
        .collect();
    let mut total: i64 = 0;
    let mut seen = false;
    for pair in words.windows(2) {
        let Some(hundredths) = decimal_cents(&pair[0]) else {
            continue;
        };
        let minutes = if pair[1].starts_with("hour") || pair[1].starts_with("hr") {
            hundredths.checked_mul(60)? / 100
        } else if pair[1].starts_with("min") {
            hundredths / 100
        } else {
            continue;
        };
        total = total.checked_add(minutes)?;
        seen = true;
    }
    (seen && (1..=MAX_LENGTH_MINUTES).contains(&total)).then_some(total)
}

/// A release date such as `Dec 1, 2013` as `YYYY-MM-DD`.
fn release_date(text: &str) -> Option<String> {
    for format in ["%b %d, %Y", "%B %d, %Y", "%m/%d/%Y", "%Y-%m-%d"] {
        if let Ok(date) = chrono::NaiveDate::parse_from_str(text.trim(), format) {
            return Some(date.format("%Y-%m-%d").to_string());
        }
    }
    None
}

/// The names in a "Director & Cast" attribute with this label.
fn attribute_values(doc: &Html, label: &str) -> Vec<String> {
    let Ok(rows) = Selector::parse(".additional-attributes-wrapper .attribute") else {
        return Vec::new();
    };
    let (Ok(label_sel), Ok(value_sel)) = (
        Selector::parse(".attribute-label"),
        Selector::parse(".attribute-value a"),
    ) else {
        return Vec::new();
    };
    doc.select(&rows)
        .find(|row| {
            row.select(&label_sel)
                .next()
                .is_some_and(|l| clean(&text_of(l)).eq_ignore_ascii_case(label))
        })
        .map(|row| {
            row.select(&value_sel)
                .map(|a| clean(&text_of(a)))
                .filter(|n| !n.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The text of the first element matching `selector`, after `prefix`.
fn labelled(doc: &Html, selector: &str, prefix: &str) -> Option<String> {
    let text = first_text(doc, selector)?;
    let rest = text.strip_prefix(prefix).unwrap_or(&text).trim();
    (!rest.is_empty()).then(|| rest.to_string())
}

/// The cleaned text of the first element matching `selector`, when not blank.
fn first_text(doc: &Html, selector: &str) -> Option<String> {
    texts(doc, selector).into_iter().next()
}

/// The cleaned, non-blank texts of every element matching `selector`.
fn texts(doc: &Html, selector: &str) -> Vec<String> {
    let Ok(sel) = Selector::parse(selector) else {
        return Vec::new();
    };
    doc.select(&sel)
        .map(|el| clean(&text_of(el)))
        .filter(|t| !t.is_empty())
        .collect()
}

/// The `content` of the first meta matching `selector`, cleaned.
fn meta(doc: &Html, selector: &str) -> Option<String> {
    attr(doc, selector, "content")
}

/// Attribute `name` of the first element matching `selector`, cleaned.
fn attr(doc: &Html, selector: &str, name: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    doc.select(&sel)
        .filter_map(|el| el.value().attr(name))
        .map(clean)
        .find(|v| !v.is_empty())
}

/// All text under an element.
fn text_of(el: ElementRef<'_>) -> String {
    el.text().collect()
}

/// Entity-decoded text with whitespace runs collapsed to one space.
fn clean(raw: &str) -> String {
    html_escape::decode_html_entities(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `url` is an absolute `https` link.
fn is_https(url: &str) -> bool {
    url.starts_with("https://") && url.len() > "https://".len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The parts of a real product page (Mistborn 1, part 1 of 3) the parser
    /// reads, as the store served them on 2026-11-11.
    const PAGE: &str = include_str!("../tests/fixtures/product-1716.html");

    #[test]
    fn a_product_page_reads_as_a_full_record() {
        let hit = parse_product_detail(PAGE).expect("record");
        assert_eq!(hit.product_id, "1716");
        assert_eq!(hit.title, "Mistborn 1: The Final Empire 1 of 3");
        assert_eq!(hit.authors().as_deref(), Some("Brandon Sanderson"));
        let narrators = hit.narrators().expect("cast");
        assert!(
            narrators.starts_with("Terence Aselford, David Jourdan"),
            "{narrators}"
        );
        assert_eq!(hit.isbn().as_deref(), Some("9781648812989"));
        assert_eq!(hit.series().as_deref(), Some("Mistborn"));
        assert_eq!(hit.series_index().as_deref(), Some("1"));
        assert_eq!(hit.length_minutes, Some(360));
        assert_eq!(hit.published_at.as_deref(), Some("2013-12-01"));
        assert_eq!(hit.categories.as_deref(), Some("Fantasy"));
        assert_eq!(hit.publisher.as_deref(), Some("GraphicAudio"));
        assert_eq!(hit.rating_overall, Some(4.92));
        assert_eq!(hit.rating_count, Some(86));
        assert_eq!(hit.price_cents, Some(990));
        assert_eq!(hit.currency.as_deref(), Some("USD"));
        assert_eq!(hit.price_label.as_deref(), Some("$9.90"));
        assert_eq!(
            hit.url.as_deref(),
            Some("https://www.graphicaudio.net/mistborn-1-the-final-empire-1-of-3.html")
        );
        assert!(hit
            .cover_url
            .as_deref()
            .is_some_and(|c| c.contains("9781648812989")));
        let description = hit.description.expect("synopsis");
        assert!(description.starts_with("Brandon Sanderson, fantasy's newest master"));
        assert!(description.contains("\n\nFor a thousand years the ash fell"));
        assert!(!description.contains('<'), "{description}");
        assert_eq!(hit.expression_kind, ExpressionKind::Dramatisation);
    }

    #[test]
    fn a_page_without_a_product_is_none() {
        assert!(parse_product_detail("").is_none());
        assert!(
            parse_product_detail("<html><body><h1>Search results</h1></body></html>").is_none()
        );
        // An id but no title is not a record either.
        assert!(parse_product_detail("<div data-product-id=\"12\"></div>").is_none());
    }

    #[test]
    fn a_stand_alone_title_has_no_series() {
        let page = PAGE.replace(
            "<span aria-hidden=\"trie\">Mistborn</span>",
            "<span aria-hidden=\"trie\">Stand-Alone Title</span>",
        );
        assert_ne!(page, PAGE, "the fixture names its series");
        let hit = parse_product_detail(&page).expect("record");
        assert!(hit.series_entries.is_empty(), "{:?}", hit.series_entries);
    }

    #[test]
    fn running_times_read_in_minutes() {
        assert_eq!(running_minutes("6 Hours"), Some(360));
        assert_eq!(running_minutes("6.5 Hours"), Some(390));
        assert_eq!(running_minutes("5 Hours 30 Minutes"), Some(330));
        assert_eq!(running_minutes("45 min"), Some(45));
        assert_eq!(running_minutes("about six hours"), None);
        assert_eq!(running_minutes("0 Hours"), None);
        assert_eq!(running_minutes("100000 Hours"), None);
        assert_eq!(running_minutes("-3 Hours"), None);
        assert_eq!(running_minutes("NaN Hours"), None);
    }

    #[test]
    fn prices_read_in_cents() {
        assert_eq!(decimal_cents("9.9"), Some(990));
        assert_eq!(decimal_cents("24.99"), Some(2499));
        assert_eq!(decimal_cents("10"), Some(1000));
        assert_eq!(decimal_cents("1.999"), None);
        assert_eq!(decimal_cents("-1"), None);
        assert_eq!(decimal_cents("1e3"), None);
        assert_eq!(decimal_cents(".5"), None);
    }

    #[test]
    fn release_dates_read_as_iso() {
        assert_eq!(release_date("Dec 1, 2013").as_deref(), Some("2013-12-01"));
        assert_eq!(
            release_date("December 1, 2013").as_deref(),
            Some("2013-12-01")
        );
        assert_eq!(release_date("soon"), None);
    }
}
