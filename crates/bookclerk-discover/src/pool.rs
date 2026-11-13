//! Discover's view of the shared catalog pool.
//!
//! Storefront answers are stored once, as `store_listings` rows with their
//! prices in `listing_offers` (`bookclerk_library::CatalogListing`). This
//! module turns a store's catalog hit into the listing the pool records,
//! and a stored listing (plus what is personal to a subject) back into the
//! [`StorefrontCandidate`] the ranking works on.

use bookclerk_library::{
    CatalogListing, CatalogListingRecord, ListingDetail, ListingOffer, OfferAudience,
    StoredCandidate,
};
use bookclerk_source::CatalogHit;
use chrono::{DateTime, Utc};

use crate::candidates::StorefrontCandidate;

/// How long a stored price counts as current. An older one is left off the
/// candidate, so the fetch asks the store again (the daily refresh).
pub(crate) const OFFER_MAX_AGE: chrono::Duration = chrono::Duration::hours(24);

/// A candidate together with the pool listing it was read from.
#[derive(Debug, Clone)]
pub(crate) struct PooledCandidate {
    /// The `store_listings` row.
    pub listing_id: String,
    /// The candidate as the ranking sees it.
    pub candidate: StorefrontCandidate,
}

/// The listing the pool records for one catalog hit.
///
/// A price on the hit (search answers sometimes carry one) is what someone
/// without an account pays, so it becomes the listing's `non_member`
/// offer, quoted `now`.
///
/// # Arguments
///
/// * `plugin_key` - PluginKey of the store that answered.
/// * `marketplace` - Marketplace the store was asked about.
/// * `hit` - The hit as the store returned it.
/// * `now` - When the store answered.
pub(crate) fn catalog_listing(
    plugin_key: &str,
    marketplace: &str,
    hit: &CatalogHit,
    now: DateTime<Utc>,
) -> CatalogListing {
    let hit = hit.clone().decode_html_entities();
    let mut draft = bookclerk_source::listing::catalog_hit_draft(&hit, marketplace);
    draft.plugin_key = plugin_key.to_string();
    let offer = ListingOffer {
        audience: OfferAudience::NonMember,
        price_cents: hit.price_cents,
        price_label: hit.price_label.clone(),
        currency: hit.currency.clone(),
        observed_at: now,
    };
    CatalogListing {
        draft,
        detail: ListingDetail {
            subtitle: hit.subtitle.clone(),
            authors: hit.authors(),
            narrators: hit.narrators(),
            series: hit.series(),
            series_index: hit.series_index(),
            description: hit.description.clone(),
            publisher: hit.publisher.clone(),
            published_at: hit.published_at.clone(),
            length_minutes: hit.length_minutes,
            language: hit.language.clone(),
            cover_url: hit.cover_url.clone(),
            url: hit.url.clone(),
            rating_overall: hit.rating_overall,
            rating_count: hit.rating_count,
            is_abridged: hit.is_abridged,
        },
        offers: if offer.is_empty() {
            Vec::new()
        } else {
            vec![offer]
        },
    }
}

/// The candidate a stored listing reads as, before what is personal to a
/// subject is added.
///
/// The price is the listing's current `non_member` offer (Discover shows
/// what someone without an account pays); an offer older than
/// [`OFFER_MAX_AGE`] is left off so the store is asked again.
///
/// # Arguments
///
/// * `source` - Storefront plugin id the candidate is shown under (`audible`).
/// * `listing` - The stored listing.
/// * `now` - The time prices are judged against.
pub(crate) fn candidate_from_listing(
    source: &str,
    listing: &CatalogListingRecord,
    now: DateTime<Utc>,
) -> StorefrontCandidate {
    let detail = &listing.detail;
    let price = listing
        .offer(OfferAudience::NonMember)
        .filter(|o| o.observed_at + OFFER_MAX_AGE > now);
    StorefrontCandidate {
        source: source.to_string(),
        product_id: listing.product_id.clone(),
        title: listing.title.clone(),
        authors: detail.authors.clone(),
        narrators: detail.narrators.clone(),
        series: detail.series.clone(),
        series_index: detail.series_index.clone(),
        asin: listing.asin.clone(),
        isbn: listing.isbn.clone(),
        cover_url: detail.cover_url.clone(),
        seed_categories: None,
        origin: String::new(),
        seed_title: None,
        starter_genre: None,
        store_editions: Vec::new(),
        subtitle: detail.subtitle.clone(),
        description: detail.description.clone(),
        publisher: detail.publisher.clone(),
        length_minutes: detail.length_minutes,
        published_at: detail.published_at.clone(),
        categories: listing.categories.clone(),
        language: detail.language.clone(),
        price_cents: price.and_then(|o| o.price_cents),
        currency: price.and_then(|o| o.currency.clone()),
        price_label: price.and_then(|o| o.price_label.clone()),
        rating_overall: detail.rating_overall,
        rating_count: detail.rating_count,
        is_abridged: detail.is_abridged,
        store_rank: None,
        work_id: listing.work_id.clone(),
    }
}

/// The candidate one stored row of a subject reads as: the shared listing
/// plus why the subject got it.
///
/// # Arguments
///
/// * `row` - The subject's stored candidate, joined with its listing.
/// * `now` - The time prices are judged against.
pub(crate) fn candidate_from_stored(
    row: &StoredCandidate,
    now: DateTime<Utc>,
) -> StorefrontCandidate {
    let mut c = candidate_from_listing(&row.source, &row.listing, now);
    c.origin.clone_from(&row.origin);
    c.seed_title.clone_from(&row.seed_title);
    c.seed_categories.clone_from(&row.seed_categories);
    c.starter_genre.clone_from(&row.starter_genre);
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(offer_age: chrono::Duration) -> CatalogListingRecord {
        let now = Utc::now();
        CatalogListingRecord {
            id: "l1".into(),
            plugin_key: "libro".into(),
            marketplace: "us".into(),
            product_id: "9780441172719".into(),
            edition_id: "e1".into(),
            work_id: Some("w1".into()),
            title: "Dune".into(),
            categories: Some("Science Fiction".into()),
            detail: ListingDetail {
                authors: Some("Frank Herbert".into()),
                description: Some("Spice.".into()),
                ..ListingDetail::default()
            },
            asin: None,
            isbn: Some("9780441172719".into()),
            offers: vec![
                ListingOffer {
                    audience: OfferAudience::Member,
                    price_cents: Some(999),
                    price_label: None,
                    currency: Some("USD".into()),
                    observed_at: now,
                },
                ListingOffer {
                    audience: OfferAudience::NonMember,
                    price_cents: Some(2499),
                    price_label: Some("$24.99".into()),
                    currency: Some("USD".into()),
                    observed_at: now - offer_age,
                },
            ],
            observed_at: now,
        }
    }

    #[test]
    fn a_candidate_shows_the_current_non_member_price() {
        let c = candidate_from_listing("libro", &record(chrono::Duration::hours(1)), Utc::now());
        assert_eq!(c.price_cents, Some(2499), "never the member price");
        assert_eq!(c.price_label.as_deref(), Some("$24.99"));
        assert_eq!(c.description.as_deref(), Some("Spice."));
        assert_eq!(c.isbn.as_deref(), Some("9780441172719"));
        assert_eq!(c.categories.as_deref(), Some("Science Fiction"));
    }

    #[test]
    fn a_stale_price_is_left_off_so_the_store_is_asked_again() {
        let c = candidate_from_listing("libro", &record(chrono::Duration::days(2)), Utc::now());
        assert_eq!(c.price_cents, None);
        assert_eq!(c.price_label, None);
    }

    #[test]
    fn a_hit_price_becomes_the_non_member_offer() {
        let hit = CatalogHit {
            product_id: "B0".into(),
            title: "Dune &amp; Co".into(),
            price_cents: Some(1499),
            currency: Some("USD".into()),
            description: Some("A &amp; B".into()),
            work_kind: bookclerk_source::records::WorkKind::Book,
            expression_kind: bookclerk_source::records::ExpressionKind::Narration,
            media_kind: bookclerk_source::records::MediaKind::Audio,
            ..CatalogHit::default()
        };
        let l = catalog_listing("audible", "us", &hit, Utc::now());
        assert_eq!(l.draft.plugin_key, "audible");
        assert_eq!(l.draft.title, "Dune & Co");
        assert_eq!(l.detail.description.as_deref(), Some("A & B"));
        assert_eq!(l.offers.len(), 1);
        assert_eq!(l.offers[0].audience, OfferAudience::NonMember);
        let unpriced = CatalogHit {
            product_id: "B1".into(),
            title: "X".into(),
            work_kind: bookclerk_source::records::WorkKind::Book,
            expression_kind: bookclerk_source::records::ExpressionKind::Narration,
            media_kind: bookclerk_source::records::MediaKind::Audio,
            ..CatalogHit::default()
        };
        assert!(catalog_listing("audible", "us", &unpriced, Utc::now())
            .offers
            .is_empty());
    }
}
