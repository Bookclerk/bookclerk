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

mod availability;
mod candidates;
mod catalog_search;
mod embed;
/// Discover crate error types for shelves, embeddings, and catalog search.
mod error;
mod hide_prices;
mod identity;
mod listening;
mod metadata_enrich;
mod pool;
mod precompute;
mod purchase;
mod radar;
mod recommend;
mod request_queue;
mod scope_filter;
mod shelves;
mod stored_feed;
mod stored_taste;
mod title_meta;
mod ttl_cache;
mod wish_pool;

pub use availability::{
    join_branch_availability, join_search_branch_availability, BranchAvailability,
};
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
pub use embed::{
    bytes_to_vector, embed_dirty_works, embed_text, is_stand_in, open_embedder, prune_vectors,
    serving_space, stand_in_space, text_for_work, text_vectors, vector_to_bytes, EmbedFields,
    EmbedPass, Embedder, HashEmbedder, SharedEmbedder, VectorSource, EMBED_PAGE_WORKS,
    LOCAL_HASH_DIMENSIONS, MODEL_LOCAL_HASH_V1, OLD_SPACE_MAX_AGE, TEXT_VECTOR_MAX_AGE,
};
pub use error::DiscoverError;
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
    fetch_subject_candidates, load_subject_taste, rank_subject_feed, refresh_subject_taste,
    stored_subject_feed, stored_subject_feed_state, FetchReport, OwnedIdentities, RankReport,
    StoredSubjectFeed, SubjectTaste, HOUSEHOLD_BLEND_WEIGHT, OWN_TASTE_THRESHOLD, STARTER_GENRES,
    STARTER_GENRE_MIN_SEEDS, STORE_CALL_MAX_AGE, STORE_CALL_TIMEOUT, STORE_FETCH_DEADLINE,
};
pub use purchase::{
    best_purchase_hint, best_purchase_hint_preferring, format_money_label,
    parse_money_label_to_cents, purchase_hints_for, resolve_purchase_hints,
    resolve_purchase_hints_batch, retain_card_recording, retain_card_recordings,
    seed_purchase_hint, PurchaseHint, PurchaseHintsQuery, PurchaseHintsResponse, BATCH_DEADLINE,
    BREAKER_OPEN_FOR, BREAKER_TRIP_TIMEOUTS, RATE_LIMIT_FIRST_PAUSE, RATE_LIMIT_MAX_PAUSE,
};
pub use radar::{
    stored_taste_seeds, taste_axes, RadarSeed, StoredSeeds, TasteAxis, RADAR_AXIS_MIN_WORKS,
    RADAR_MAX_AXES, RADAR_MIN_AXES,
};
pub use recommend::{
    combine_wishlist_score, listening_engagement, rank_request_queue_taste, recommend,
    recommend_feed, RankedQueueEntry, RecommendOptions, Recommendation, WISH_COUNT_WEIGHT,
};
pub use request_queue::{read_request_queue, StoredRequestQueue, REQUEST_QUEUE_MAX_AGE};
pub use shelves::{
    build_discover_feed, flatten_feed, shelf_is_disabled, shelf_kind_catalog, DiscoverFeed,
    DiscoverShelf, FeedState, ShelfKindInfo, ShelfTaste, SERIES_CAP,
};
pub use title_meta::{
    check_title_meta_query, plan_title_meta_fill, resolve_title_meta, resolve_title_meta_answer,
    resolve_title_meta_batch, resolve_title_reviews, resolve_title_sample, FillLookup,
    FillStoreLookups, Settlement, TitleMeta, TitleMetaAnswer, TitleMetaQuery,
    TitleMetaQueryProblem, TitleReview, TitleReviewsPage, TitleReviewsQuery,
    TITLE_META_BATCH_DEADLINE, TITLE_META_MAX_STORE_EDITIONS, TITLE_META_RETRY_TTL,
};
pub use wish_pool::{
    pool_wish_editions, record_wish_member_offers, PooledEdition, MAX_WISH_EDITIONS,
};
