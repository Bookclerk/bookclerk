//! Merge per-storefront wishlist snapshots into the richest bibliographic view.

use crate::models::{TitleRequestRecord, WishlistPurchaseHint, WishlistStoreEdition};

/// Trims `s` and returns `None` when empty.
fn nonempty(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// True when `raw` contains both `<` and `>` (used to prefer HTML blurbs).
fn has_html(raw: &str) -> bool {
    raw.contains('<') && raw.contains('>')
}

/// Prefer HTML blurbs over plain text; otherwise the longer string wins.
///
/// # Arguments
///
/// * `a` - First candidate description (may be HTML or plain).
/// * `b` - Second candidate description.
///
/// # Returns
///
/// The richer non-empty description, or `None` when both are empty.
#[must_use]
pub fn pick_better_description(a: Option<&str>, b: Option<&str>) -> Option<String> {
    let left = a.map(str::trim).filter(|s| !s.is_empty());
    let right = b.map(str::trim).filter(|s| !s.is_empty());
    match (left, right) {
        (None, None) => None,
        (Some(l), None) => Some(l.to_string()),
        (None, Some(r)) => Some(r.to_string()),
        (Some(l), Some(r)) => {
            let left_html = has_html(l);
            let right_html = has_html(r);
            if right_html && !left_html {
                Some(r.to_string())
            } else if left_html && !right_html {
                Some(l.to_string())
            } else if r.len() > l.len() {
                Some(r.to_string())
            } else {
                Some(l.to_string())
            }
        }
    }
}

/// First non-empty trimmed string of `a` then `b`.
fn pick_str(a: Option<&str>, b: Option<&str>) -> Option<String> {
    nonempty(a).or_else(|| nonempty(b))
}

/// Scores a purchase hint by URL, display price, and list/member cents (higher wins on merge).
fn hint_richness(h: &WishlistPurchaseHint) -> u8 {
    let mut score = 0u8;
    if nonempty(h.url.as_deref()).is_some() {
        score += 1;
    }
    if h.price_cents.is_some() || nonempty(h.price_label.as_deref()).is_some() {
        score += 2;
    }
    if h.member_price_cents.is_some() || h.list_price_cents.is_some() {
        score += 1;
    }
    score
}

/// Fills empty title/url/price fields on `into` from `from` without overwriting set values.
fn merge_hint(into: &mut WishlistPurchaseHint, from: &WishlistPurchaseHint) {
    if nonempty(into.title.as_deref()).is_none() {
        into.title = from.title.clone();
    }
    if nonempty(into.url.as_deref()).is_none() {
        into.url = from.url.clone();
    }
    if into.price_cents.is_none() {
        into.price_cents = from.price_cents;
    }
    if nonempty(into.currency.as_deref()).is_none() {
        into.currency = from.currency.clone();
    }
    if nonempty(into.price_label.as_deref()).is_none() {
        into.price_label = from.price_label.clone();
    }
    if into.list_price_cents.is_none() {
        into.list_price_cents = from.list_price_cents;
    }
    if nonempty(into.list_price_label.as_deref()).is_none() {
        into.list_price_label = from.list_price_label.clone();
    }
    if into.member_price_cents.is_none() {
        into.member_price_cents = from.member_price_cents;
    }
    if nonempty(into.member_price_label.as_deref()).is_none() {
        into.member_price_label = from.member_price_label.clone();
    }
}

/// Apply merged bib / editions / hints onto a wishlist row from its source snapshots.
///
/// Updates `row` in place from `row.sources`, clearing merged fields when no
/// sources remain.
///
/// # Arguments
///
/// * `row` - Wishlist title-request record whose `sources` snapshots are merged.
pub fn apply_merged_sources(row: &mut TitleRequestRecord) {
    let sources = row.sources.clone();
    if sources.is_empty() {
        row.description = None;
        row.subtitle = None;
        row.narrators = None;
        row.series = None;
        row.series_index = None;
        row.publisher = None;
        row.length_minutes = None;
        row.published_at = None;
        row.genres = None;
        row.language = None;
        row.store_editions.clear();
        row.purchase_hints.clear();
        return;
    }

    let mut description: Option<String> = None;
    let mut subtitle: Option<String> = None;
    let mut narrators: Option<String> = None;
    let mut series: Option<String> = None;
    let mut series_index: Option<String> = None;
    let mut publisher: Option<String> = None;
    let mut length_minutes: Option<i64> = None;
    let mut published_at: Option<String> = None;
    let mut genres: Option<String> = None;
    let mut language: Option<String> = None;
    let mut cover_url = nonempty(row.cover_url.as_deref());
    let mut asin = nonempty(row.asin.as_deref());
    let mut isbn = nonempty(row.isbn.as_deref());
    let mut authors = nonempty(row.authors.as_deref());
    let mut title = Some(row.title.clone());

    let mut editions: Vec<WishlistStoreEdition> = Vec::new();
    let mut hints: Vec<WishlistPurchaseHint> = Vec::new();

    for src in &sources {
        description = pick_better_description(description.as_deref(), src.description.as_deref());
        subtitle = pick_str(subtitle.as_deref(), src.subtitle.as_deref());
        narrators = pick_str(narrators.as_deref(), src.narrators.as_deref());
        series = pick_str(series.as_deref(), src.series.as_deref());
        series_index = pick_str(series_index.as_deref(), src.series_index.as_deref());
        publisher = pick_str(publisher.as_deref(), src.publisher.as_deref());
        if length_minutes.is_none() {
            length_minutes = src.length_minutes;
        }
        published_at = pick_str(published_at.as_deref(), src.published_at.as_deref());
        genres = pick_str(genres.as_deref(), src.categories.as_deref());
        language = pick_str(language.as_deref(), src.language.as_deref());
        cover_url = pick_str(cover_url.as_deref(), src.cover_url.as_deref());
        asin = pick_str(asin.as_deref(), src.asin.as_deref());
        isbn = pick_str(isbn.as_deref(), src.isbn.as_deref());
        authors = pick_str(authors.as_deref(), src.authors.as_deref());
        title = pick_str(title.as_deref(), src.title.as_deref()).or(title);

        let source = src.source.trim().to_ascii_lowercase();
        let product_id = src.product_id.trim().to_string();
        if source.is_empty() || product_id.is_empty() {
            continue;
        }
        if !editions
            .iter()
            .any(|e| e.source == source && e.product_id == product_id)
        {
            editions.push(WishlistStoreEdition {
                source: source.clone(),
                product_id: product_id.clone(),
            });
        }
        let hint = WishlistPurchaseHint {
            source: source.clone(),
            product_id,
            title: src.title.clone(),
            url: src.url.clone(),
            price_cents: src.price_cents,
            currency: src.currency.clone(),
            price_label: src.price_label.clone(),
            list_price_cents: src.list_price_cents,
            list_price_label: src.list_price_label.clone(),
            member_price_cents: src.member_price_cents,
            member_price_label: src.member_price_label.clone(),
        };
        if let Some(existing) = hints
            .iter_mut()
            .find(|h| h.source == hint.source && h.product_id == hint.product_id)
        {
            let incoming = hint_richness(&hint);
            let current = hint_richness(existing);
            if incoming > current {
                let mut richer = hint;
                merge_hint(&mut richer, existing);
                *existing = richer;
            } else {
                merge_hint(existing, &hint);
            }
        } else {
            hints.push(hint);
        }
    }

    hints.sort_by(|a, b| {
        hint_richness(b)
            .cmp(&hint_richness(a))
            .then_with(|| a.source.cmp(&b.source))
    });

    if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
        row.title = t;
    }
    row.authors = authors;
    row.asin = asin;
    row.isbn = isbn;
    row.cover_url = cover_url;
    row.description = description;
    row.subtitle = subtitle;
    row.narrators = narrators;
    row.series = series;
    row.series_index = series_index;
    row.publisher = publisher;
    row.length_minutes = length_minutes;
    row.published_at = published_at;
    row.genres = genres;
    row.language = language;
    row.store_editions = editions;
    row.purchase_hints = hints;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefer_html_description() {
        let plain = Some("Short teaser...");
        let html = Some("<p>A <b>full</b> blurb with detail.</p>");
        assert!(pick_better_description(plain, html)
            .unwrap()
            .contains("<p>"));
    }
}
