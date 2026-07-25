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
mod error;
mod match_score;
mod public_meta;

pub use enrich::{
    apply_enrichment_to_book, confidence_percent_to_fraction, enrich_books_from_audible,
    lookup_by_metadata, lookup_by_metadata_with_client, Enrichment, ScoredMatch,
    DEFAULT_ENRICH_MIN_CONFIDENCE,
};
pub use error::{EnrichError, Result};
pub use match_score::{
    calculate_match_confidence, clean_author_for_compares, clean_title_for_compares, is_valid_asin,
    isbn_exact_match, levenshtein_distance, levenshtein_similarity, normalize_isbn, MatchQuery,
    ScoreInput,
};
pub use public_meta::{
    fetch_audnexus_book, fetch_audnexus_chapters, fetch_public_chapter_info, normalize_region,
    public_http_client, region_tld, search_catalog_asins, search_catalog_keywords,
};
