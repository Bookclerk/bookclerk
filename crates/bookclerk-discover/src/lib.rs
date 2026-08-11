//! Discovery: works graph, embeddings, recommendations, purchase hints, and requests.
//!
//! # Audience
//!
//! Host binaries (`bookclerkd`, CLI) that build Discover shelves, catalog search,
//! and wishlist ranking. Storefront plugins are not expected to depend on this
//! crate directly.
//!
//! See `docs/` Discover / library product docs for operator-facing behavior.

mod candidates;
mod catalog_search;
mod embed;
mod error;
mod identity;
mod openlibrary;
mod purchase;
mod recommend;
mod shelves;
mod title_meta;
mod ttl_cache;
mod works;

pub use bookclerk_source::{
    default_preferred_language, language_rank, normalize_language, CatalogSearchField,
    CatalogSearchSort, CatalogSortDir,
};
pub use candidates::{
    gather_storefront_candidates, select_taste_seeds, CandidateFetchOptions, StorefrontCandidate,
};
pub use catalog_search::{
    catalog_search, catalog_search_page, catalog_search_with_field, CatalogSearchFilters,
    CatalogSearchHit, CatalogSearchPage, CatalogSearchPageOpts,
};
pub use embed::{
    default_embedding_model_id, embed_dirty_works, embedding_model_id, open_embedder,
    text_for_work, CosineHit, Embedder, HashEmbedder, OnnxEmbedder, MODEL_ALL_MINILM_L6_V2_Q,
    MODEL_LOCAL_HASH_V1,
};
pub use identity::{
    candidate_map_key, hard_work_key, identities_match, merge_candidate_metadata,
    merge_global_queue_entries, merge_recommendation, parse_series_index, push_edition,
    push_shelf_category, recommendation_map_key, soft_work_key, work_map_key, works_match,
    StoreEdition, WorkIdentity,
};
pub use openlibrary::{
    enrich_books_from_openlibrary, enrich_books_from_openlibrary_with, OpenLibraryOptions,
};
pub use purchase::{
    best_purchase_hint, best_purchase_hint_preferring, format_money_label,
    parse_money_label_to_cents, purchase_hints_for, resolve_purchase_hints,
    resolve_purchase_hints_batch, seed_purchase_hint, PurchaseHint, PurchaseHintsQuery,
    PurchaseHintsResponse,
};
pub use recommend::{
    combine_wishlist_score, listening_engagement, rank_global_request_queue, recommend,
    recommend_feed, RankedQueueEntry, RecommendOptions, Recommendation, WISH_COUNT_WEIGHT,
};
pub use shelves::{
    build_discover_feed, flatten_feed, shelf_is_disabled, shelf_kind_catalog, DiscoverFeed,
    DiscoverShelf, ShelfKindInfo, ShelfTaste,
};
pub use title_meta::{
    resolve_title_meta, resolve_title_meta_batch, resolve_title_reviews, TitleMeta, TitleMetaQuery,
    TitleReview, TitleReviewsPage, TitleReviewsQuery,
};
pub use works::rebuild_works_from_library;
