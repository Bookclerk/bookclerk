//! Unauthenticated Audible catalog HTTP: keyword, author, narrator, series
//! and genre search, plus ratings and reviews, from the public
//! `api.audible{tld}` catalog. No Audible login is involved.
//!
//! The Audible storefront guest uses it for Discover search, expand and
//! purchase hints; host enrichment (`bookclerk-enrich`) uses it to match
//! titles from other stores to an ASIN. It links only the plugin SDK's HTTP
//! client and `bookclerk-storefront`, so the guest does not pull in the host's
//! library stack.

/// [`CatalogError`] and the crate [`Result`] alias.
mod error;
mod public_meta;

pub use error::{CatalogError, Result};
pub use public_meta::{
    fetch_audible_catalog_rating, fetch_audible_catalog_reviews,
    fetch_audible_catalog_reviews_page, is_audible_podcast_product, normalize_region,
    normalize_review_body, public_http_client, region_tld, resolve_genre_category_id,
    search_catalog_asins, search_catalog_by_genre_name, search_catalog_by_narrator,
    search_catalog_by_series_asin, search_catalog_by_series_name, search_catalog_keywords,
    search_catalog_products, search_catalog_products_ex, search_catalog_products_ex2,
    search_catalog_products_paged, search_catalog_products_rich, search_catalog_storefront,
    CatalogProduct, CatalogRating, CatalogReview, CatalogReviewsPage, CatalogReviewsSort,
};

/// True when `s` (trimmed) has the shape of an Audible ASIN: ten ASCII
/// letters or digits.
///
/// Any ten-letter word also passes; callers that must tell a title from an
/// ASIN need a stricter check (see `bookclerk_enrich::is_asin_title`).
///
/// # Arguments
///
/// * `s` - Candidate ASIN, possibly with surrounding whitespace.
#[must_use]
pub fn is_valid_asin(s: &str) -> bool {
    let s = s.trim();
    s.len() == 10 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::is_valid_asin;

    #[test]
    fn asin_shape_is_ten_ascii_alphanumerics() {
        assert!(is_valid_asin("B0BOOK0001"));
        assert!(is_valid_asin(" 1427209952 "));
        assert!(!is_valid_asin("B0BOOK000"));
        assert!(!is_valid_asin("B0BOOK-001"));
        assert!(!is_valid_asin("B0BOOK000É"));
    }
}
