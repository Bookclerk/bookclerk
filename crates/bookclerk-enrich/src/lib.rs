//! Source-agnostic Audible metadata enrichment.
//!
//! Matches library rows that lack an Audible ASIN using public Audible catalog
//! search + Audnexus (AudioBookshelf-style confidence scoring). Intended for
//! Libro.fm, Chirp, GraphicAudio, and any future non-Audible sources.
//!
//! A confident match stores the Audible ASIN and catalog fields on the row;
//! acquire then overlays Audible chapter trees (and related tags) onto plain
//! audio that lacks Audible brand intro/outro.

mod enrich;
/// Enrichment error type and [`Result`] alias for Audnexus / catalog lookups.
mod error;
mod match_score;
mod public_meta;

pub use bookclerk_library::decode_html_entities;
pub use enrich::{
    apply_enrichment_to_book, confidence_percent_to_fraction, enrich_books_from_audible,
    enrichment_for_asin, enrichment_for_asin_with_client, lookup_by_metadata,
    lookup_by_metadata_with_client, Enrichment, ScoredMatch, DEFAULT_ENRICH_MIN_CONFIDENCE,
};
pub use error::{EnrichError, Result};
pub use match_score::{
    calculate_match_confidence, canonicalize_isbn, clean_author_for_compares,
    clean_title_for_compares, is_valid_asin, isbn_exact_match, levenshtein_distance,
    levenshtein_similarity, normalize_isbn, MatchQuery, ScoreInput,
};
pub use public_meta::{
    fetch_audible_catalog_rating, fetch_audible_catalog_reviews,
    fetch_audible_catalog_reviews_page, fetch_audnexus_book, fetch_audnexus_chapters,
    fetch_public_chapter_info, is_audible_podcast_product, normalize_region, normalize_review_body,
    public_http_client, region_tld, resolve_genre_category_id, search_catalog_asins,
    search_catalog_by_genre_name, search_catalog_by_narrator, search_catalog_by_series_asin,
    search_catalog_by_series_name, search_catalog_keywords, search_catalog_products,
    search_catalog_products_ex, search_catalog_products_ex2, search_catalog_products_paged,
    search_catalog_products_rich, search_catalog_storefront, CatalogProduct, CatalogRating,
    CatalogReview, CatalogReviewsPage, CatalogReviewsSort,
};
