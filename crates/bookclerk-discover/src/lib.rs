//! Discovery: works graph, embeddings, recommendations, purchase hints, and requests.
//!
//! # Audience
//!
//! Host binaries (`bookclerkd`, CLI) that build Discover shelves, catalog search,
//! and wishlist ranking. Storefront plugins are not expected to depend on this
//! crate directly.
//!
//! See `docs/` Discover / library product docs for operator-facing behavior.

// Pre-existing sites, grandfathered until fixed: https://github.com/Bookclerk/bookclerk/issues/284
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::expect_used
)]

mod candidates;
mod catalog_search;
mod embed;
/// Discover crate error types for shelves, embeddings, and catalog search.
mod error;
mod identity;
mod listening;
mod metadata_enrich;
mod precompute;
mod purchase;
mod recommend;
mod request_queue;
mod shelves;
mod stored_feed;
mod title_meta;
mod ttl_cache;

pub use bookclerk_source::{
    default_preferred_language, language_rank, normalize_language, CatalogSearchField,
    CatalogSearchSort, CatalogSortDir,
};
pub use candidates::{
    gather_storefront_candidates, select_taste_seeds, CandidateFetchOptions, SeedTitle,
    StorefrontCandidate,
};
pub use catalog_search::{
    catalog_search, catalog_search_page, catalog_search_with_field, CatalogSearchFilters,
    CatalogSearchHit, CatalogSearchPage, CatalogSearchPageOpts,
};
#[cfg(feature = "onnx")]
pub use embed::OnnxEmbedder;
pub use embed::{
    default_embedding_model_id, embed_dirty_works, embedding_model_id, open_embedder,
    text_for_work, CosineHit, Embedder, HashEmbedder, EMBED_PAGE_WORKS, MODEL_ALL_MINILM_L6_V2_Q,
    MODEL_LOCAL_HASH_V1,
};
pub use identity::{
    candidate_map_key, hard_work_key, identities_match, merge_candidate_metadata,
    merge_recommendation, parse_series_index, push_edition, push_shelf_category,
    recommendation_map_key, soft_work_key, work_map_key, works_match, StoreEdition, WorkIdentity,
};
pub use metadata_enrich::{
    enrich_books_from_metadata, MetadataEnrichOptions, DEFAULT_MAX_REQUESTS_PER_RUN,
    METADATA_ENRICH_SOURCE_PREFIX,
};
pub use precompute::{
    fetch_subject_candidates, load_subject_taste, rank_subject_feed, stored_subject_feed,
    stored_subject_feed_state, FetchReport, RankReport, StoredSubjectFeed, SubjectTaste,
    HOUSEHOLD_BLEND_WEIGHT, OWN_TASTE_THRESHOLD, STARTER_GENRES, STARTER_GENRE_MIN_SEEDS,
    STORE_CALL_MAX_AGE, STORE_CALL_TIMEOUT, STORE_FETCH_DEADLINE,
};
pub use purchase::{
    best_purchase_hint, best_purchase_hint_preferring, format_money_label,
    parse_money_label_to_cents, purchase_hints_for, resolve_purchase_hints,
    resolve_purchase_hints_batch, seed_purchase_hint, PurchaseHint, PurchaseHintsQuery,
    PurchaseHintsResponse, BATCH_DEADLINE, BREAKER_OPEN_FOR, BREAKER_TRIP_TIMEOUTS,
    RATE_LIMIT_FIRST_PAUSE, RATE_LIMIT_MAX_PAUSE,
};
pub use recommend::{
    combine_wishlist_score, listening_engagement, rank_request_queue_taste, recommend,
    recommend_feed, RankedQueueEntry, RecommendOptions, Recommendation, WISH_COUNT_WEIGHT,
};
pub use request_queue::{
    queue_entry_key, read_request_queue, StoredRequestQueue, REQUEST_QUEUE_MAX_AGE,
};
pub use shelves::{
    build_discover_feed, flatten_feed, shelf_is_disabled, shelf_kind_catalog, DiscoverFeed,
    DiscoverShelf, FeedState, ShelfKindInfo, ShelfTaste, SERIES_CAP,
};
pub use title_meta::{
    resolve_title_meta, resolve_title_meta_batch, resolve_title_reviews, TitleMeta, TitleMetaQuery,
    TitleReview, TitleReviewsPage, TitleReviewsQuery,
};
