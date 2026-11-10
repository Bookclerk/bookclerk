//! Recommendation engine: storefront candidates scored against local taste.
//!
//! Flow:
//! 1. Build local taste seeds (finished / rated / listening)
//! 2. Expand **unowned** candidates from storefronts (Libro, Audible, Chirp, GA)
//! 3. Score those candidates with local signals + embedding similarity
//!    (series completion + active listening are first-class heuristics)
//! 4. Merge open title requests; attach purchase hints

use std::collections::{HashMap, HashSet};

use bookclerk_library::{
    AcquireStatus, BookRecord, GlobalQueueEntry, LibraryStore, ListeningProgressRecord,
};
use bookclerk_source::SourceRegistry;

use crate::candidates::{gather_storefront_candidates, select_taste_seeds, CandidateFetchOptions};
use crate::embed::{bytes_to_vector, cosine, open_embedder, Embedder};
use crate::error::Result;
use crate::identity::{
    identities_match, merge_global_queue_entries, merge_recommendation, push_edition,
    push_shelf_category, recommendation_map_key, StoreEdition, WorkIdentity,
};
use crate::purchase::{
    purchase_hints_for, seed_purchase_hint, seed_source_is_trusted, PurchaseHint,
};

/// Per open wish on the global queue. Large enough that multi-user demand
/// dominates typical local taste scores (~1–25) while recommend signals still
/// order titles that share the same wish count.
pub const WISH_COUNT_WEIGHT: f64 = 40.0;

/// Tunables for [`recommend`].
#[derive(Debug, Clone)]
pub struct RecommendOptions {
    /// Maximum number of recommendations to return.
    pub limit: usize,
    /// Embedding model id used for similarity (`all-minilm-l6-v2-q`, …).
    pub embedding_model: String,
    /// Marketplace / region code (`us`, `uk`, …) for catalog lookups.
    pub region: String,
    /// When true, attach buy links to each recommendation.
    pub include_purchase_hints: bool,
    /// When set, only listening rows for this external user influence ranking.
    /// Provider-agnostic — any integration that synced that user id.
    pub external_user_id: Option<String>,
    /// When false, ignore `listening_progress` entirely (owned-library taste only).
    /// Listening is always optional: empty/missing progress simply adds no signal.
    pub include_listening: bool,
    /// When non-empty, only use listening rows from these integration ids
    /// (`audiobookshelf`, plugin ids, …). Empty = all providers.
    pub listening_providers: Vec<String>,
    /// Pull unowned titles from storefront catalogs (the primary path).
    pub fetch_storefront_candidates: bool,
    /// Max owned titles used as storefront related-search seeds.
    pub storefront_seed_limit: usize,
    /// Hard cap on outbound catalog calls during recommend.
    pub storefront_max_remote_calls: usize,
    /// Drop GraphicAudio Magento series-set SKUs from discovery candidates.
    pub exclude_graphicaudio_series_sets: bool,
    /// Shelf kinds / ids to hide (`finish_series`, `author`, `genre`, `from_store`, …).
    /// Empty = offer every shelf Discover can build.
    pub disabled_shelves: Vec<String>,
    /// Where the ONNX model lives. `Some` means candidates are embedded with
    /// ONNX MiniLM when it loads (local-hash when it doesn't); `None` means
    /// local-hash only, without trying ONNX.
    pub models_dir: Option<std::path::PathBuf>,
    /// ONNX intra-op thread count for the embedder (clamped ≥ 1).
    pub embed_intra_threads: usize,
    /// When false, skip embedding similarity and use heuristic ranking only.
    /// Which embedder scores similarity is chosen by [`Self::models_dir`].
    pub embeddings_enabled: bool,
}

impl Default for RecommendOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            embedding_model: String::from(crate::embed::default_embedding_model_id()),
            region: String::from("us"),
            include_purchase_hints: true,
            external_user_id: None,
            include_listening: true,
            listening_providers: Vec::new(),
            fetch_storefront_candidates: true,
            storefront_seed_limit: 8,
            storefront_max_remote_calls: 32,
            exclude_graphicaudio_series_sets: false,
            disabled_shelves: Vec::new(),
            models_dir: None,
            embed_intra_threads: 1,
            embeddings_enabled: true,
        }
    }
}

/// One ranked recommendation (typically an unowned storefront title).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Recommendation {
    /// Library `works.id` UUID when the recommendation is tied to an owned work.
    #[schema(example = "5b7e2c1d-8a4f-4e3b-9c6d-2f1a0b9e8d7c")]
    pub work_id: Option<String>,
    /// Display title as shown on the storefront or library card.
    #[schema(example = "The Left Hand of Darkness")]
    pub title: String,
    /// Comma-separated author names when the storefront provides them.
    #[schema(example = "Ursula K. Le Guin")]
    pub authors: Option<String>,
    /// Comma-separated narrator names when known.
    #[schema(example = "George Guidall")]
    pub narrators: Option<String>,
    /// Series name when the title belongs to a named series.
    #[schema(example = "Hainish Cycle")]
    pub series: Option<String>,
    /// Position within the series (e.g. `1`, `1.5`) when known.
    #[schema(example = "4")]
    pub series_index: Option<String>,
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    #[schema(example = "B002V0QK4C")]
    pub asin: Option<String>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    #[schema(example = "9780441478125")]
    pub isbn: Option<String>,
    /// Ranking score (higher is better); units are algorithm-specific.
    #[schema(example = 120.87)]
    pub score: f64,
    /// Short human-readable explanations for why this item was ranked here.
    #[schema(example = json!(["wishlisted by 3 people", "matches liked author (Ursula K. Le Guin)"]))]
    pub reasons: Vec<String>,
    /// Buy / open-in-store links with optional member vs list pricing.
    pub purchase_hints: Vec<PurchaseHint>,
    /// True when sourced from an open title request rather than storefront discovery.
    #[schema(example = true)]
    pub from_request: bool,
    /// Wishlist / title-request UUID when this card came from a request.
    #[schema(example = "7c1e5a2b-3d4f-4e6a-9b8c-0d1e2f3a4b5c")]
    pub request_uuid: Option<String>,
    /// Storefront that proposed this title (`audible`, `libro`, …).
    #[schema(example = "libro")]
    pub candidate_source: Option<String>,
    /// Storefront product id of the edition that produced this card.
    #[schema(example = "9780441478125")]
    pub candidate_product_id: Option<String>,
    /// All known storefront editions (for multi-store purchase links).
    #[serde(default)]
    pub store_editions: Vec<StoreEdition>,
    /// Categories/subjects copied from the taste seed that produced this hit.
    #[schema(example = "Science Fiction & Fantasy > Science Fiction")]
    pub seed_categories: Option<String>,
    /// Stable shelf-kind tags for prefs (`finish_series`, `author`, `chirp_deals`, …).
    /// Dynamic shelf titles stay in `build_discover_feed`; membership uses these.
    #[serde(default)]
    #[schema(example = json!(["finish_series", "author"]))]
    pub categories: Vec<String>,
    /// Stable bibliographic key for wishlist / merge (`isbn:` / `asin:` / `soft:`…).
    #[serde(default)]
    #[schema(example = "isbn:9780441478125")]
    pub work_key: String,
    /// Cover art URL when known from a storefront catalog hit or wishlist row.
    #[serde(default)]
    #[schema(example = "https://images.example.com/covers/9780441478125.jpg")]
    pub cover_url: Option<String>,
    /// Same-origin path of this cover's processed images, set by the daemon
    /// when `cover_url` is an `https` URL. Append `/128`
    /// (thumbnail), `/360` (card) or `/full`, so a client can show covers
    /// without loading `cover_url` from another site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(
        example = "/api/covers/aHR0cHM6Ly9pbWFnZXMuZXhhbXBsZS5jb20vYy5qcGc.q2fW1n4bJm0Rg9xV3kTzPA"
    )]
    pub cover_path: Option<String>,
    /// Optional bibliographic extras from storefront catalog / Audnexus.
    #[serde(default)]
    #[schema(example = "A Hainish Novel")]
    pub subtitle: Option<String>,
    /// Publisher or storefront synopsis; may be truncated for embeddings.
    #[serde(default)]
    #[schema(
        example = "An envoy to the planet Gethen must win the trust of a people who have no fixed gender."
    )]
    pub description: Option<String>,
    /// Publisher imprint as reported by the catalog.
    #[serde(default)]
    #[schema(example = "Blackstone Publishing")]
    pub publisher: Option<String>,
    /// Audiobook runtime in whole minutes when known.
    #[serde(default)]
    #[schema(example = 560)]
    pub length_minutes: Option<i64>,
    /// Publication date string from the catalog (ISO or storefront format).
    #[serde(default)]
    #[schema(example = "2005-06-21")]
    pub published_at: Option<String>,
    /// Candidate genres (not taste-seed `seed_categories`).
    #[serde(default)]
    #[schema(example = "Science Fiction, Classics")]
    pub genres: Option<String>,
    /// BCP-47 / storefront language code (`en`, `de`, …) when known.
    #[serde(default)]
    #[schema(example = "en")]
    pub language: Option<String>,
    /// Distinct people who have this title on an open wishlist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub wishers: Vec<bookclerk_library::QueueWisher>,
    /// Number of distinct users who wishlisted this title (may exceed `wishers.len()`).
    #[serde(default, skip_serializing_if = "wish_count_is_zero")]
    #[schema(example = 3)]
    pub wish_count: i64,
    /// Title of the book or wish the storefront was asked about to find this
    /// card, when it came from such an expansion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Dune")]
    pub seed_title: Option<String>,
    /// Starter genre this card was browsed from while the taste was thin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "Science Fiction")]
    pub starter_genre: Option<String>,
}

/// Per-book multipliers on taste weight, keyed by book uuid; a book that is
/// not listed counts in full. Household books blended into a member's taste
/// carry a multiplier below one.
pub(crate) type TasteWeights = HashMap<String, f64>;

/// The multiplier `weights` gives `book` (one when unlisted).
fn taste_multiplier(weights: &TasteWeights, book: &BookRecord) -> f64 {
    weights.get(&book.uuid).copied().unwrap_or(1.0)
}

/// The multiplier `weights` gives a listening `row`: its book's multiplier,
/// so a blended household listen counts as much as the book it is for (one
/// when the row names no listed book).
fn listening_multiplier(weights: &TasteWeights, row: &ListeningProgressRecord) -> f64 {
    row.book_uuid
        .as_deref()
        .and_then(|uuid| weights.get(uuid))
        .copied()
        .unwrap_or(1.0)
}

/// True when `wish_count` should be omitted from JSON (zero / unset).
fn wish_count_is_zero(n: &i64) -> bool {
    *n == 0
}

/// Global wishlist queue entry ranked by overall/operator recommend taste +
/// heavy wish-count weight (shared order; not per-viewer personalized).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct RankedQueueEntry {
    /// Stable map key for this work (`isbn:…`, `asin:…`, or soft title+author).
    #[schema(example = "isbn:9780441478125")]
    pub work_key: String,
    /// Library work the entry's wishes resolve to, when the library could
    /// place them. Wishes for one work are one entry even when their
    /// `work_key`s differ, so a client matches its own wishes on this id
    /// first and on `work_key` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "0b7e2c1d-5f3a-4e6b-9c8d-1a2b3c4d5e6f")]
    pub work_id: Option<String>,
    /// Display title as shown on the storefront or library card.
    #[schema(example = "The Left Hand of Darkness")]
    pub title: String,
    /// Comma-separated author names when the storefront provides them.
    #[schema(example = "Ursula K. Le Guin")]
    pub authors: Option<String>,
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    #[schema(example = "B002V0QK4C")]
    pub asin: Option<String>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    #[schema(example = "9780441478125")]
    pub isbn: Option<String>,
    /// HTTPS URL for cover art when the catalog exposes one.
    #[schema(example = "https://images.example.com/covers/9780441478125.jpg")]
    pub cover_url: Option<String>,
    /// Same-origin path of this cover's processed images, set by the daemon
    /// when `cover_url` is an `https` URL. Append `/128`
    /// (thumbnail), `/360` (card) or `/full`, so a client can show covers
    /// without loading `cover_url` from another site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(
        example = "/api/covers/aHR0cHM6Ly9pbWFnZXMuZXhhbXBsZS5jb20vYy5qcGc.q2fW1n4bJm0Rg9xV3kTzPA"
    )]
    pub cover_path: Option<String>,
    /// Number of distinct users who wishlisted this title.
    #[schema(example = 3)]
    pub wish_count: i64,
    /// Distinct people who have this title on an open wishlist.
    #[serde(default)]
    pub wishers: Vec<bookclerk_library::QueueWisher>,
    /// Sample Bookclerk user UUIDs who requested this title (privacy-capped).
    #[schema(example = json!(["0b7e2c1d-5f3a-4e6b-9c8d-1a2b3c4d5e6f"]))]
    pub sample_uuids: Vec<String>,
    /// UTC timestamp of the earliest wishlist request for this work.
    #[schema(example = "2026-09-02T08:15:00Z")]
    pub first_requested_at: chrono::DateTime<chrono::Utc>,
    /// UTC timestamp of the latest wishlist request for this work.
    #[schema(example = "2026-10-29T10:12:00Z")]
    pub last_requested_at: chrono::DateTime<chrono::Utc>,
    /// Final rank score (`taste_score + wish_count * `[`WISH_COUNT_WEIGHT`]).
    #[schema(example = 120.61)]
    pub score: f64,
    /// Local taste / embedding component before the wish-count boost.
    #[schema(example = 0.61)]
    pub taste_score: f64,
    /// Short human-readable explanations for why this item was ranked here.
    #[schema(example = json!(["wishlisted by 3 people", "matches liked author (Ursula K. Le Guin)"]))]
    pub reasons: Vec<String>,
}

impl RankedQueueEntry {
    /// Ranks a global-queue row by combining local taste with a multi-user wishlist boost.
    fn from_global(entry: GlobalQueueEntry, taste_score: f64, mut reasons: Vec<String>) -> Self {
        if entry.wish_count > 1 {
            reasons.push(format!("wishlisted by {} people", entry.wish_count));
        } else if entry.wish_count == 1 {
            reasons.push(String::from("on the wishlist"));
        }
        let score = combine_wishlist_score(taste_score, entry.wish_count);
        Self {
            cover_path: None,
            work_key: entry.work_key,
            work_id: entry.work_id,
            title: entry.title,
            authors: entry.authors,
            asin: entry.asin,
            isbn: entry.isbn,
            cover_url: entry.cover_url,
            wish_count: entry.wish_count,
            wishers: entry.wishers,
            sample_uuids: entry.sample_uuids,
            first_requested_at: entry.first_requested_at,
            last_requested_at: entry.last_requested_at,
            score,
            taste_score,
            reasons,
        }
    }
}

/// Combine local recommend taste with a heavy multi-user wishlist boost.
///
/// # Arguments
///
/// * `taste_score` - `taste_score` input for this call.
/// * `wish_count` - Numeric `wish_count` value for this call.
///
/// # Returns
///
/// `f64` result.
#[must_use]
pub fn combine_wishlist_score(taste_score: f64, wish_count: i64) -> f64 {
    taste_score.max(0.0) + (wish_count.max(0) as f64) * WISH_COUNT_WEIGHT
}

/// Per-series local signal used for completion / listening heuristics.
#[derive(Debug, Clone, Default)]
struct SeriesAffinity {
    /// Owned titles in this series (any acquire status).
    owned_count: usize,
    /// Owned titles in this series marked finished.
    finished_count: usize,
    /// Owned books in this series that have listening activity.
    listening_count: usize,
    /// At least one in-progress (not finished) listen in this series.
    active_listening: bool,
    /// Max continuous engagement among in-progress titles (≈0–6).
    active_listen_weight: f64,
    /// Sum of continuous engagement across listened titles in the series.
    listen_engagement_sum: f64,
    /// Highest parsed series index among owned titles, used to boost the next book.
    max_owned_index: Option<f64>,
}

/// Build ranked recommendations for the operator (or a specific external user).
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `registry` - Configured content-source or integration registry.
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `Vec<Recommendation>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn recommend(
    library: &LibraryStore,
    registry: &SourceRegistry,
    opts: &RecommendOptions,
) -> Result<Vec<Recommendation>> {
    let mut recs = recommend_all(library, registry, opts).await?;
    recs.truncate(opts.limit);
    Ok(recs)
}

/// Personalized Discover feed (Netflix-style shelves) from the same candidate pool.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `registry` - Configured content-source or integration registry.
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `crate::shelves::DiscoverFeed` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn recommend_feed(
    library: &LibraryStore,
    registry: &SourceRegistry,
    opts: &RecommendOptions,
) -> Result<crate::shelves::DiscoverFeed> {
    let recs = recommend_all(library, registry, opts).await?;
    let books = library.list_books(None).await?;
    let listening = load_listening(library, opts).await?;
    let taste = shelf_taste(&books, &TasteWeights::new(), &listening);
    Ok(feed_from_recommendations(&recs, &taste, opts))
}

/// Groups ranked recommendations into shelves for one taste.
///
/// # Arguments
///
/// * `recs` - Ranked recommendations (any order; shelves sort them).
/// * `taste` - The shelf taste, from [`shelf_taste`] plus the subject's
///   wishes and linked stores.
/// * `opts` - Supplies the per-shelf limit and hidden shelves.
pub(crate) fn feed_from_recommendations(
    recs: &[Recommendation],
    taste: &crate::shelves::ShelfTaste,
    opts: &RecommendOptions,
) -> crate::shelves::DiscoverFeed {
    crate::shelves::build_discover_feed(
        recs,
        taste,
        opts.limit.clamp(12, 48),
        &opts.disabled_shelves,
    )
}

/// Rank the global wishlist queue with **overall / operator** recommend taste
/// plus a heavy wish-count weight.
///
/// This ranking is **not** personalized: portal `external_user_id` filters are
/// ignored so every viewer sees the same shared queue order (household library
/// + all listening progress). Titles already in the library are omitted.
///
/// Wishlist rows keyed as ASIN vs ISBN for the same work are merged first.
///
/// # Arguments
///
/// * `library` - Open library store used for reads/writes.
/// * `registry` - Configured content-source or integration registry.
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `Vec<RankedQueueEntry>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn rank_global_request_queue(
    library: &LibraryStore,
    _registry: &SourceRegistry,
    opts: &RecommendOptions,
) -> Result<Vec<RankedQueueEntry>> {
    let mut opts = opts.clone();
    opts.external_user_id = None;

    let queue = merge_global_queue_entries(library.list_global_request_queue().await?);
    if queue.is_empty() {
        return Ok(Vec::new());
    }

    let books = library.list_books(None).await?;
    let listening = load_listening(library, &opts).await?;
    let profile =
        build_taste_profile(library, &books, &TasteWeights::new(), &listening, &opts).await?;
    let mut embedder = open_candidate_embedder(&opts)?;

    let mut ranked = Vec::new();
    for entry in queue {
        if library_owns_work(library, &entry).await? {
            continue;
        }
        let (taste_score, reasons, _) = score_work_against_taste(
            &entry.title,
            entry.authors.as_deref(),
            None,
            None,
            None,
            &profile,
            embedder.as_mut(),
        );
        ranked.push(RankedQueueEntry::from_global(entry, taste_score, reasons));
    }
    ranked.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.wish_count.cmp(&a.wish_count))
            .then_with(|| b.last_requested_at.cmp(&a.last_requested_at))
    });
    Ok(ranked)
}

/// Load optional listening rows for ranking (empty when disabled / no providers).
async fn load_listening(
    library: &LibraryStore,
    opts: &RecommendOptions,
) -> Result<Vec<ListeningProgressRecord>> {
    if !opts.include_listening {
        return Ok(Vec::new());
    }
    let mut rows = library
        .list_listening_progress(opts.external_user_id.as_deref())
        .await?;
    if !opts.listening_providers.is_empty() {
        rows.retain(|r| {
            opts.listening_providers
                .iter()
                .any(|p| p.eq_ignore_ascii_case(&r.provider))
        });
    }
    Ok(rows)
}

/// Full scored candidate pool (not truncated) used by flat list + shelves.
async fn recommend_all(
    library: &LibraryStore,
    registry: &SourceRegistry,
    opts: &RecommendOptions,
) -> Result<Vec<Recommendation>> {
    let books = library.list_books(None).await?;

    let (owned_asins, owned_isbns, owned_product_keys) = owned_identity_sets(&books);

    let listening = load_listening(library, opts).await?;
    let seeds = select_taste_seeds(&books, &engagement_by_book(&listening));
    let mut candidates = Vec::new();
    if opts.fetch_storefront_candidates && !seeds.is_empty() {
        let fetch_opts = CandidateFetchOptions {
            region: opts.region.clone(),
            seed_limit: opts.storefront_seed_limit,
            max_remote_calls: opts.storefront_max_remote_calls,
            exclude_graphicaudio_series_sets: opts.exclude_graphicaudio_series_sets,
            ..CandidateFetchOptions::default()
        };
        candidates = gather_storefront_candidates(
            library,
            registry,
            &seeds,
            &owned_asins,
            &owned_isbns,
            &owned_product_keys,
            &fetch_opts,
        )
        .await?;
    }
    rank_candidates(
        library,
        registry,
        &books,
        &TasteWeights::new(),
        &listening,
        candidates,
        opts,
    )
    .await
}

/// Strongest listening engagement per book uuid (rows without a book are skipped).
pub(crate) fn engagement_by_book(listening: &[ListeningProgressRecord]) -> HashMap<String, f64> {
    let mut by_uuid: HashMap<String, f64> = HashMap::new();
    for row in listening {
        let weight = listening_engagement(row);
        if weight <= 0.0 {
            continue;
        }
        if let Some(uuid) = &row.book_uuid {
            by_uuid
                .entry(uuid.clone())
                .and_modify(|e| *e = (*e).max(weight))
                .or_insert(weight);
        }
    }
    by_uuid
}

/// ASINs, canonical ISBNs and `source:product` keys the household owns.
///
/// # Returns
///
/// `(asins, isbns, product_keys)`; ASINs upper-cased, ISBNs canonicalized,
/// product keys both as `source:product_id` and bare `product_id`.
pub(crate) fn owned_identity_sets(
    books: &[BookRecord],
) -> (HashSet<String>, HashSet<String>, HashSet<String>) {
    let owned_asins: HashSet<String> = books
        .iter()
        .filter_map(|b| b.asin.clone())
        .map(|s| s.to_ascii_uppercase())
        .collect();
    let owned_isbns: HashSet<String> = books
        .iter()
        .filter_map(|b| b.isbn.as_deref())
        .map(bookclerk_enrich::canonicalize_isbn)
        .filter(|s| !s.is_empty())
        .collect();
    let owned_product_keys: HashSet<String> = books
        .iter()
        .flat_map(|b| {
            [
                format!("{}:{}", b.source, b.product_id),
                b.product_id.clone(),
            ]
        })
        .collect();
    (owned_asins, owned_isbns, owned_product_keys)
}

/// Scores storefront candidates and open wishlist works against one taste.
///
/// # Arguments
///
/// * `library` - Library store (wishlist queue, stored embeddings).
/// * `registry` - Storefronts, used only for live purchase hints when asked.
/// * `taste_books` - Books whose finishes, ratings and series shape the taste
///   (the subject's own books, with the household's blended in while the
///   subject's own are few).
/// * `taste_weights` - How much each taste book counts (see [`TasteWeights`]).
/// * `listening` - The subject's listening rows.
/// * `candidates` - Unowned storefront candidates, already merged by identity.
/// * `opts` - Ranking options.
///
/// # Returns
///
/// Every recommendation, highest score first.
///
/// # Errors
///
/// Returns the library read error.
pub(crate) async fn rank_candidates(
    library: &LibraryStore,
    registry: &SourceRegistry,
    taste_books: &[BookRecord],
    taste_weights: &TasteWeights,
    listening: &[ListeningProgressRecord],
    candidates: Vec<crate::candidates::StorefrontCandidate>,
    opts: &RecommendOptions,
) -> Result<Vec<Recommendation>> {
    let profile = build_taste_profile(library, taste_books, taste_weights, listening, opts).await?;
    let mut embedder = open_candidate_embedder(opts)?;
    let mut scored: HashMap<String, Recommendation> = HashMap::new();

    for c in candidates {
        let (mut score, mut reasons, mut categories) = score_work_against_taste(
            &c.title,
            c.authors.as_deref(),
            c.narrators.as_deref(),
            c.series.as_deref(),
            c.series_index.as_deref(),
            &profile,
            embedder.as_mut(),
        );
        // Origin / category / deals are candidate-specific (not on wishlist rows).
        reasons.insert(0, c.origin.clone());
        if let Some(cats) = &c.seed_categories {
            for cat in split_tokens_display(cats) {
                let key = cat.to_lowercase();
                if let Some(w) = profile.liked_categories.get(&key) {
                    score += w * 0.45;
                    reasons.push(format!("matches liked category ({cat})"));
                    push_shelf_category(&mut categories, "genre");
                }
            }
        }
        if c.origin.contains("chirp top deals") || c.origin.contains("chirp free deals") {
            score += 1.5;
            push_shelf_category(&mut categories, "chirp_deals");
        }
        if c.origin.contains("related") {
            push_shelf_category(&mut categories, "because");
        }
        if c.starter_genre.is_some() {
            push_shelf_category(&mut categories, "starter_genre");
        }
        push_shelf_category(&mut categories, "from_store");

        let mut store_editions = c.store_editions;
        push_edition(
            &mut store_editions,
            StoreEdition::new(&c.source, &c.product_id),
        );

        let mut purchase_hints = Vec::new();
        // The price the catalog listed when the candidate was fetched shows
        // on the card at once, as the store's list price, until the live
        // lookup refreshes it. Only Audible is linked from its id without a
        // live check (see `resolve_purchase_hints`); the others carry the
        // price alone and get their link from the live lookup.
        if let Some(label) = c.price_label.clone().filter(|s| !s.is_empty()) {
            let mut hint = if seed_source_is_trusted(&c.source) {
                seed_purchase_hint(
                    &c.source,
                    &c.product_id,
                    Some(c.title.clone()),
                    &opts.region,
                )
            } else {
                None
            }
            .unwrap_or_else(|| {
                PurchaseHint::link(
                    c.source.as_str(),
                    c.product_id.as_str(),
                    Some(c.title.clone()),
                    None,
                )
            });
            hint.price_cents = c.price_cents;
            hint.list_price_cents = c.price_cents;
            hint.currency = c.currency.clone();
            hint.list_price_label = Some(label.clone());
            hint.price_label = Some(label);
            purchase_hints.push(hint);
        }
        let mut rec = Recommendation {
            work_id: None,
            title: c.title,
            authors: c.authors,
            narrators: c.narrators,
            series: c.series,
            series_index: c.series_index,
            asin: c.asin.clone(),
            isbn: c.isbn.clone(),
            score,
            reasons,
            purchase_hints,
            from_request: false,
            request_uuid: None,
            candidate_source: Some(c.source),
            candidate_product_id: Some(c.product_id),
            store_editions,
            seed_categories: c.seed_categories,
            categories,
            work_key: String::new(),
            cover_url: c.cover_url,
            subtitle: c.subtitle,
            description: c.description,
            publisher: c.publisher,
            length_minutes: c.length_minutes,
            published_at: c.published_at,
            genres: c.categories,
            language: c.language,
            seed_title: c.seed_title,
            starter_genre: c.starter_genre,
            ..Default::default()
        };
        rec.work_key = recommendation_map_key(&rec);
        upsert_recommendation(&mut scored, rec);
    }

    // Global wishlist works: recommend taste + heavy multi-user wish boost.
    for entry in merge_global_queue_entries(library.list_global_request_queue().await?) {
        if library_owns_work(library, &entry).await? {
            continue;
        }
        let (taste_score, mut reasons, mut categories) = score_work_against_taste(
            &entry.title,
            entry.authors.as_deref(),
            None,
            None,
            None,
            &profile,
            embedder.as_mut(),
        );
        if entry.wish_count > 1 {
            reasons.push(format!("wishlisted by {} people", entry.wish_count));
        } else {
            reasons.push(String::from("on the wishlist"));
        }
        push_shelf_category(&mut categories, "requests");
        let store_editions: Vec<StoreEdition> = entry
            .store_editions
            .into_iter()
            .map(|e| StoreEdition::new(&e.source, &e.product_id))
            .collect();
        let purchase_hints: Vec<PurchaseHint> = entry
            .purchase_hints
            .into_iter()
            .map(|h| PurchaseHint {
                source: h.source,
                product_id: h.product_id,
                title: h.title,
                url: h.url,
                price_cents: h.price_cents,
                currency: h.currency,
                price_label: h.price_label,
                list_price_cents: h.list_price_cents,
                list_price_label: h.list_price_label,
                member_price_cents: h.member_price_cents,
                member_price_label: h.member_price_label,
            })
            .collect();
        let mut rec = Recommendation {
            title: entry.title,
            authors: entry.authors,
            narrators: entry.narrators,
            series: entry.series,
            series_index: entry.series_index,
            asin: entry.asin,
            isbn: entry.isbn,
            score: combine_wishlist_score(taste_score, entry.wish_count),
            reasons,
            from_request: true,
            request_uuid: entry.sample_uuids.first().cloned(),
            categories,
            work_key: entry.work_key.clone(),
            cover_url: entry.cover_url,
            description: entry.description,
            subtitle: entry.subtitle,
            publisher: entry.publisher,
            length_minutes: entry.length_minutes,
            published_at: entry.published_at,
            genres: entry.genres,
            language: entry.language,
            store_editions,
            purchase_hints,
            wishers: entry.wishers,
            wish_count: entry.wish_count,
            ..Default::default()
        };
        if rec.work_key.trim().is_empty() {
            rec.work_key = recommendation_map_key(&rec);
        }
        upsert_recommendation(&mut scored, rec);
    }

    let mut recs: Vec<Recommendation> = scored.into_values().collect();

    if opts.include_purchase_hints {
        attach_purchase_hints(&mut recs, registry, opts).await;
    }

    // Sort flat list for callers that still want a single ranking.
    recs.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(recs)
}

/// Weighted author/narrator/category likes, series affinity, and optional embedding centroid.
struct TasteProfile {
    /// Lowercased author → finish/rating/listen weight.
    liked_authors: HashMap<String, f64>,
    /// Lowercased narrator → finish/rating weight.
    liked_narrators: HashMap<String, f64>,
    /// Lowercased category/subject → finish/rating weight.
    liked_categories: HashMap<String, f64>,
    /// Lowercased series name → ownership and listening affinity.
    series_affinity: HashMap<String, SeriesAffinity>,
    /// Mean embedding of seed works when embeddings are available.
    seed_centroid: Option<Vec<f32>>,
}

/// Accumulates liked people/categories, series affinity, and a seed embedding centroid.
async fn build_taste_profile(
    library: &LibraryStore,
    books: &[BookRecord],
    weights: &TasteWeights,
    listening: &[ListeningProgressRecord],
    opts: &RecommendOptions,
) -> Result<TasteProfile> {
    let mut liked_authors: HashMap<String, f64> = HashMap::new();
    let mut liked_narrators: HashMap<String, f64> = HashMap::new();
    let mut liked_categories: HashMap<String, f64> = HashMap::new();
    let mut seed_work_ids: HashSet<String> = HashSet::new();
    let mut seed_book_uuids: Vec<String> = Vec::new();
    let mut listening_engagement_by_uuid: HashMap<String, f64> = HashMap::new();

    for book in books {
        let mut weight = 0.0;
        if book.is_finished {
            weight += 3.0;
        }
        if let Some(r) = book.rating_overall {
            if r >= 4.0 {
                weight += 2.0;
            } else if r >= 3.0 {
                weight += 1.0;
            }
        }
        if book.acquire_status == AcquireStatus::Acquired {
            weight += 0.5;
        }
        weight *= taste_multiplier(weights, book);
        if weight <= 0.0 {
            continue;
        }
        seed_book_uuids.push(book.uuid.clone());
        if let Some(authors) = &book.authors {
            for a in split_tokens_display(authors) {
                *liked_authors.entry(a.to_lowercase()).or_default() += weight;
            }
        }
        if let Some(narrators) = &book.narrators {
            for n in split_tokens_display(narrators) {
                *liked_narrators.entry(n.to_lowercase()).or_default() += weight;
            }
        }
        if let Some(cats) = book.categories.as_ref().or(book.subjects.as_ref()) {
            for c in split_tokens_display(cats) {
                *liked_categories.entry(c.to_lowercase()).or_default() += weight;
            }
        }
    }

    // One bulk lookup for every liked title's work. If it fails, look each
    // title up on its own, so a failed read only loses its own seed.
    match library.work_ids_for_books(&seed_book_uuids).await {
        Ok(works) => seed_work_ids.extend(works.into_values()),
        Err(_) => {
            for uuid in &seed_book_uuids {
                if let Ok(Some(wid)) = library.work_id_for_book(uuid).await {
                    seed_work_ids.insert(wid);
                }
            }
        }
    }

    for row in listening {
        let weight = listening_engagement(row) * listening_multiplier(weights, row);
        if weight <= 0.0 {
            continue;
        }
        if let Some(wid) = &row.work_id {
            seed_work_ids.insert(wid.clone());
        }
        if let Some(uuid) = &row.book_uuid {
            listening_engagement_by_uuid
                .entry(uuid.clone())
                .and_modify(|e| *e = (*e).max(weight))
                .or_insert(weight);
        }
        if let Some(authors) = &row.authors {
            for a in split_tokens_display(authors) {
                *liked_authors.entry(a.to_lowercase()).or_default() += weight;
            }
        }
    }

    let series_affinity = build_series_affinity(books, listening, &listening_engagement_by_uuid);
    let seed_centroid =
        seed_embedding_centroid(library, &seed_work_ids, &opts.embedding_model).await?;
    Ok(TasteProfile {
        liked_authors,
        liked_narrators,
        liked_categories,
        series_affinity,
        seed_centroid,
    })
}

/// Scores a candidate against liked people, series completion, and embedding similarity.
fn score_work_against_taste(
    title: &str,
    authors: Option<&str>,
    narrators: Option<&str>,
    series: Option<&str>,
    series_index: Option<&str>,
    profile: &TasteProfile,
    embedder: Option<&mut Box<dyn Embedder>>,
) -> (f64, Vec<String>, Vec<String>) {
    let mut score = 1.0;
    let mut reasons = Vec::new();
    let mut categories = Vec::new();

    if let Some(authors) = authors {
        for a in split_tokens_display(authors) {
            let key = a.to_lowercase();
            if let Some(w) = profile.liked_authors.get(&key) {
                score += w * 0.9;
                reasons.push(format!("matches liked author ({a})"));
                push_shelf_category(&mut categories, "author");
            }
        }
    }
    if let Some(narrators) = narrators {
        for n in split_tokens_display(narrators) {
            let key = n.to_lowercase();
            if let Some(w) = profile.liked_narrators.get(&key) {
                score += w * 0.7;
                reasons.push(format!("matches liked narrator ({n})"));
                push_shelf_category(&mut categories, "narrator");
            }
        }
    }

    apply_series_completion_score(
        &series.map(str::to_string),
        series_index,
        &profile.series_affinity,
        &mut score,
        &mut reasons,
        &mut categories,
    );

    if let (Some(centroid), Some(embedder)) = (&profile.seed_centroid, embedder) {
        let text = wishlist_embed_text(title, authors, narrators, series);
        if let Ok(vectors) = embedder.embed(&[text]) {
            if let Some(v) = vectors.first() {
                let sim = cosine(centroid, v);
                if sim > 0.15 {
                    score += f64::from(sim) * 12.0;
                    reasons.push(format!("similar to titles you finish (sim {sim:.2})"));
                    push_shelf_category(&mut categories, "similar_taste");
                }
            }
        }
    }

    (score, reasons, categories)
}

/// Joins title, authors, narrators, and series into embedder input text.
fn wishlist_embed_text(
    title: &str,
    authors: Option<&str>,
    narrators: Option<&str>,
    series: Option<&str>,
) -> String {
    let mut parts = vec![title.to_string()];
    if let Some(a) = authors {
        parts.push(a.to_string());
    }
    if let Some(n) = narrators {
        parts.push(n.to_string());
    }
    if let Some(s) = series {
        parts.push(s.to_string());
    }
    parts.join("\n")
}

/// Seeds trusted storefront purchase URLs, then looks up remaining hints live.
async fn attach_purchase_hints(
    recs: &mut [Recommendation],
    registry: &SourceRegistry,
    opts: &RecommendOptions,
) {
    for rec in recs.iter_mut() {
        // Seed every known storefront edition so the card can price them at view time.
        let mut editions = rec.store_editions.clone();
        if let (Some(source), Some(pid)) = (
            rec.candidate_source.as_deref(),
            rec.candidate_product_id.as_deref(),
        ) {
            push_edition(&mut editions, StoreEdition::new(source, pid));
        }
        rec.store_editions = editions.clone();

        for ed in &editions {
            // Align with resolve_purchase_hints: only Audible is a trusted URL
            // seed. Libro ISBN≠membership (404s); Chirp/GA need title-matched
            // live purchase_hint verification before advertising a link.
            if !seed_source_is_trusted(&ed.source) {
                continue;
            }
            if let Some(hint) = seed_purchase_hint(
                &ed.source,
                &ed.product_id,
                Some(rec.title.clone()),
                &opts.region,
            ) {
                if !rec.purchase_hints.iter().any(|h| {
                    h.source.eq_ignore_ascii_case(&hint.source)
                        && h.product_id.eq_ignore_ascii_case(&hint.product_id)
                }) {
                    rec.purchase_hints.push(hint);
                }
            }
        }

        if rec.purchase_hints.is_empty() {
            match purchase_hints_for(
                registry,
                &rec.title,
                rec.authors.as_deref(),
                rec.asin.as_deref(),
                rec.isbn.as_deref(),
                &opts.region,
            )
            .await
            {
                Ok(hints) => rec.purchase_hints = hints,
                Err(err) => tracing::debug!(error = %err, "purchase hint lookup failed"),
            }
        }
    }
}

/// Whether the household library holds the work a wishlist entry is for.
///
/// An entry that resolved to a work when it was wished is owned when a
/// library title resolves to that work. Otherwise the library's resolver
/// decides, as it does for a scanned listing (shared identifiers, then the
/// work's identifiers, then the work-level title-and-author match), so a
/// sequel that shares its series' name with an owned book is its own work
/// and stays on the queue.
///
/// # Errors
///
/// Returns the library read error.
async fn library_owns_work(library: &LibraryStore, entry: &GlobalQueueEntry) -> Result<bool> {
    // A wish resolved to a work when it was created: ask about that work.
    if let Some(work_id) = entry.work_id.as_deref().filter(|id| !id.trim().is_empty()) {
        return Ok(!library.book_uuids_for_work(work_id).await?.is_empty());
    }
    let query = bookclerk_library::TitleQuery {
        title: &entry.title,
        authors: entry.authors.as_deref(),
        asin: entry.asin.as_deref(),
        isbn: entry.isbn.as_deref(),
        series: entry.series.as_deref(),
        series_index: entry.series_index.as_deref(),
    };
    Ok(library.owned_work_for(&query).await?.is_some())
}

/// Inserts or merges a recommendation when ASIN/ISBN/title identity already exists.
fn upsert_recommendation(map: &mut HashMap<String, Recommendation>, rec: Recommendation) {
    let match_key = map.iter().find_map(|(key, existing)| {
        if identities_match(
            WorkIdentity::new(
                rec.asin.as_deref(),
                rec.isbn.as_deref(),
                &rec.title,
                rec.authors.as_deref(),
            )
            .with_series(rec.series.as_deref())
            .with_series_index(rec.series_index.as_deref()),
            WorkIdentity::new(
                existing.asin.as_deref(),
                existing.isbn.as_deref(),
                &existing.title,
                existing.authors.as_deref(),
            )
            .with_series(existing.series.as_deref())
            .with_series_index(existing.series_index.as_deref()),
        ) {
            Some(key.clone())
        } else {
            None
        }
    });

    if let Some(old_key) = match_key {
        let mut existing = map.remove(&old_key).expect("just found");
        merge_recommendation(&mut existing, rec);
        let new_key = recommendation_map_key(&existing);
        map.insert(new_key, existing);
        return;
    }

    let key = recommendation_map_key(&rec);
    map.insert(key, rec);
}

/// Builds shelf-ranking taste from owned sources, liked people, and listening weights.
///
/// # Arguments
///
/// * `books` - Books whose authors, narrators and genres name shelves.
/// * `weights` - How much each book counts (see [`TasteWeights`]).
/// * `listening` - Listening rows that add to the liked authors.
pub(crate) fn shelf_taste(
    books: &[BookRecord],
    weights: &TasteWeights,
    listening: &[ListeningProgressRecord],
) -> crate::shelves::ShelfTaste {
    let mut taste = crate::shelves::ShelfTaste::default();
    for book in books {
        if !book.source.trim().is_empty() {
            taste.owned_sources.insert(book.source.to_ascii_lowercase());
        }
        let mut weight = 0.0;
        if book.is_finished {
            weight += 3.0;
        }
        if let Some(r) = book.rating_overall {
            if r >= 4.0 {
                weight += 2.0;
            } else if r >= 3.0 {
                weight += 1.0;
            }
        }
        weight *= taste_multiplier(weights, book);
        if weight <= 0.0 {
            continue;
        }
        if let Some(authors) = &book.authors {
            for a in split_tokens_display(authors) {
                let key = a.to_lowercase();
                let entry = taste.authors.entry(key).or_insert((a.clone(), 0.0));
                entry.0 = a;
                entry.1 += weight;
            }
            taste
                .seed_authors_by_title
                .insert(book.title.to_lowercase(), split_tokens_display(authors));
        }
        if let Some(narrators) = &book.narrators {
            for n in split_tokens_display(narrators) {
                let key = n.to_lowercase();
                let entry = taste.narrators.entry(key).or_insert((n.clone(), 0.0));
                entry.0 = n;
                entry.1 += weight;
            }
        }
        if let Some(cats) = book.categories.as_ref().or(book.subjects.as_ref()) {
            for c in split_tokens_display(cats) {
                let key = c.to_lowercase();
                let entry = taste.categories.entry(key).or_insert((c.clone(), 0.0));
                entry.0 = c;
                entry.1 += weight;
            }
        }
    }
    for row in listening {
        let weight = listening_engagement(row) * listening_multiplier(weights, row);
        if weight <= 0.0 {
            continue;
        }
        if let Some(authors) = &row.authors {
            for a in split_tokens_display(authors) {
                let key = a.to_lowercase();
                let entry = taste.authors.entry(key).or_insert((a.clone(), 0.0));
                entry.0 = a;
                entry.1 += weight;
            }
        }
    }
    taste
}

/// Aggregates per-series ownership, finish counts, and in-progress listen depth.
fn build_series_affinity(
    books: &[BookRecord],
    listening: &[ListeningProgressRecord],
    listening_engagement_by_uuid: &HashMap<String, f64>,
) -> HashMap<String, SeriesAffinity> {
    let mut by_series: HashMap<String, SeriesAffinity> = HashMap::new();
    let book_by_uuid: HashMap<&str, &BookRecord> =
        books.iter().map(|b| (b.uuid.as_str(), b)).collect();

    for book in books {
        let Some(series) = book
            .series
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let key = series.to_lowercase();
        let entry = by_series.entry(key).or_default();
        entry.owned_count += 1;
        if book.is_finished {
            entry.finished_count += 1;
        }
        if let Some(w) = listening_engagement_by_uuid.get(&book.uuid) {
            if *w > 0.0 {
                entry.listening_count += 1;
                entry.listen_engagement_sum += *w;
            }
        }
        if let Some(idx) = parse_series_index(book.series_index.as_deref()) {
            entry.max_owned_index = Some(match entry.max_owned_index {
                Some(m) => m.max(idx),
                None => idx,
            });
        }
    }

    // Continuous in-progress listen depth via progress rows linked to owned books.
    for row in listening {
        if row.is_finished {
            continue;
        }
        let weight = listening_engagement(row);
        if weight <= 0.0 {
            continue;
        }
        let series = row
            .book_uuid
            .as_deref()
            .and_then(|u| book_by_uuid.get(u))
            .and_then(|b| b.series.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(series) = series else {
            continue;
        };
        let entry = by_series.entry(series.to_lowercase()).or_default();
        entry.active_listening = true;
        entry.active_listen_weight = entry.active_listen_weight.max(weight);
    }

    by_series
}

/// Score an unowned candidate against incomplete-series / listening affinity.
fn apply_series_completion_score(
    candidate_series: &Option<String>,
    candidate_index: Option<&str>,
    affinity: &HashMap<String, SeriesAffinity>,
    score: &mut f64,
    reasons: &mut Vec<String>,
    categories: &mut Vec<String>,
) {
    let Some(series) = candidate_series
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(aff) = affinity.get(&series.to_lowercase()) else {
        return;
    };
    if aff.owned_count == 0 {
        return;
    }

    // Owning some of the series and seeing an unowned sibling ⇒ complete the series.
    *score += 6.0 + (aff.owned_count.min(5) as f64 - 1.0) * 1.5;
    reasons.push(format!(
        "complete series (“{series}”; own {})",
        aff.owned_count
    ));
    push_shelf_category(categories, "finish_series");

    if let Some(cand_idx) = parse_series_index(candidate_index) {
        if let Some(max_owned) = aff.max_owned_index {
            if cand_idx > max_owned && cand_idx <= max_owned + 1.5 {
                *score += 8.0;
                reasons.push(format!("next book in series (after {max_owned})"));
            } else if cand_idx > max_owned {
                *score += 3.0;
                reasons.push(format!("later book in series (#{cand_idx})"));
            }
        }
    }

    if aff.active_listen_weight > 0.0 || aff.active_listening {
        // Continuous: a deep listen outweighs a brief open (~2–8).
        let depth = aff
            .active_listen_weight
            .max(if aff.active_listening { 0.5 } else { 0.0 });
        *score += 2.0 + depth * 1.5;
        push_shelf_category(categories, "keep_listening");
        reasons.push(format!(
            "series “{series}” actively being listened to (engagement {depth:.2})"
        ));
    }
    if aff.listen_engagement_sum > 0.0 {
        // Continuous across titles — hours/progress stacked, not a boolean.
        *score += 1.0 + aff.listen_engagement_sum.min(8.0) * 0.65;
        if aff.listening_count >= 2 {
            reasons.push(format!(
                "multiple books in “{series}” being listened to ({}; engagement {:.2})",
                aff.listening_count, aff.listen_engagement_sum
            ));
        } else {
            reasons.push(format!(
                "listening activity in “{series}” (engagement {:.2})",
                aff.listen_engagement_sum
            ));
        }
    }
}

/// Continuous engagement from a listening progress row (≈0.0–6.0).
///
/// **Absolute hours heard** are the primary signal — 50% of a 30‑hour title
/// (15 h) outweighs 50% of a 3‑hour title (1.5 h). Percent complete is only a
/// secondary completion bonus, not the main weight.
///
/// # Arguments
///
/// * `row` - SeaORM query row to project into an RPC DTO.
///
/// # Returns
///
/// `f64` result.
#[must_use]
pub fn listening_engagement(row: &ListeningProgressRecord) -> f64 {
    let mut progress = row.progress.unwrap_or(0.0).clamp(0.0, 1.0);
    if row.is_finished {
        progress = progress.max(1.0);
    }
    if progress <= 0.0 {
        if let (Some(cur), Some(dur)) = (row.current_time_seconds, row.duration_seconds) {
            if dur > 0.0 {
                progress = (cur / dur).clamp(0.0, 1.0);
            }
        }
    }

    // Prefer wall-clock position; fall back to progress × duration for stores
    // that only report a fraction.
    let seconds = row
        .current_time_seconds
        .filter(|s| *s > 0.0)
        .or_else(|| {
            row.duration_seconds
                .filter(|d| *d > 0.0)
                .map(|d| progress * d)
        })
        .unwrap_or(0.0)
        .max(0.0);

    if !row.is_finished && progress <= 0.0 && seconds <= 0.0 {
        return 0.0;
    }

    let hours = seconds / 3600.0;
    // Soft saturation so 15 h ≫ 1.5 h without letting a single marathon dominate.
    // 1.5 h ≈ 1.0, 3 h ≈ 1.7, 15 h ≈ 3.6, 30 h ≈ 4.2
    let from_hours = (hours / (hours + 6.0)) * 5.0;

    // Secondary: finishing (or near-finishing) still matters a little — a
    // completed short book is real engagement, just not equal to 15 hours in.
    let from_completion = if row.is_finished { 1.0 } else { progress * 0.5 };

    (from_hours + from_completion).clamp(0.0, 6.0)
}

pub use crate::identity::parse_series_index;

/// Averages stored work embeddings for seed ids; returns `None` when none are stored.
async fn seed_embedding_centroid(
    library: &LibraryStore,
    seed_work_ids: &HashSet<String>,
    model: &str,
) -> Result<Option<Vec<f32>>> {
    let mut query: Option<Vec<f32>> = None;
    let mut count = 0usize;
    for wid in seed_work_ids {
        if let Some((_, blob)) = library.get_embedding_vector("work", wid, model).await? {
            let v = bytes_to_vector(&blob);
            match &mut query {
                Some(acc) => {
                    if acc.len() == v.len() {
                        for i in 0..acc.len() {
                            acc[i] += v[i];
                        }
                        count += 1;
                    }
                }
                None => {
                    query = Some(v);
                    count = 1;
                }
            }
        }
    }
    if let Some(mut q) = query {
        if count > 0 {
            for x in &mut q {
                *x /= count as f32;
            }
        }
        Ok(Some(q))
    } else {
        Ok(None)
    }
}

/// Opens the candidate embedder: none when similarity is off, local-hash
/// without a models dir, otherwise ONNX with the local-hash fallback.
fn open_candidate_embedder(opts: &RecommendOptions) -> Result<Option<Box<dyn Embedder>>> {
    if !opts.embeddings_enabled {
        return Ok(None);
    }
    let Some(dir) = &opts.models_dir else {
        return Ok(Some(Box::new(crate::embed::HashEmbedder::new(384))));
    };
    Ok(Some(open_embedder(dir, opts.embed_intra_threads, true)?))
}

/// Splits a display list on `,; /|&` and trims empty tokens.
fn split_tokens_display(s: &str) -> Vec<String> {
    s.split([',', ';', '/', '|', '&'])
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_use_local_hash_without_a_models_dir() {
        let opts = RecommendOptions {
            models_dir: None,
            embeddings_enabled: true,
            ..RecommendOptions::default()
        };
        let embedder = open_candidate_embedder(&opts)
            .unwrap()
            .expect("similarity stays on without ONNX");
        assert_eq!(embedder.model_id(), crate::embed::MODEL_LOCAL_HASH_V1);
    }

    #[test]
    fn candidates_skip_similarity_when_it_is_off() {
        let opts = RecommendOptions {
            embeddings_enabled: false,
            ..RecommendOptions::default()
        };
        assert!(open_candidate_embedder(&opts).unwrap().is_none());
    }

    /// A ranked queue entry names the library work its wishes resolve to, so
    /// a client can match its own wish whatever key that wish was stored
    /// under; an entry the library could not place carries none.
    #[test]
    fn a_ranked_entry_names_its_work() {
        let entry = |work_id: Option<&str>| GlobalQueueEntry {
            work_key: "asin:B002V1OF70".into(),
            work_id: work_id.map(str::to_string),
            title: "Dune".into(),
            authors: None,
            asin: None,
            isbn: None,
            cover_url: None,
            description: None,
            subtitle: None,
            narrators: None,
            series: None,
            series_index: None,
            publisher: None,
            length_minutes: None,
            published_at: None,
            genres: None,
            language: None,
            store_editions: Vec::new(),
            purchase_hints: Vec::new(),
            wish_count: 2,
            wishers: Vec::new(),
            sample_uuids: Vec::new(),
            first_requested_at: chrono::Utc::now(),
            last_requested_at: chrono::Utc::now(),
        };
        let ranked = RankedQueueEntry::from_global(entry(Some("work-dune")), 0.0, Vec::new());
        let json = serde_json::to_value(&ranked).unwrap();
        assert_eq!(json["work_id"], "work-dune");
        assert_eq!(json["work_key"], "asin:B002V1OF70");
        let unplaced = RankedQueueEntry::from_global(entry(None), 0.0, Vec::new());
        let json = serde_json::to_value(&unplaced).unwrap();
        assert!(json.get("work_id").is_none(), "{json}");
    }

    /// The price a catalog listed shows on the card as the store's list
    /// price: an Audible card is linked from its id, while another store's
    /// card carries the price and waits for the live lookup for its link.
    #[tokio::test]
    async fn a_listed_price_shows_on_the_card() {
        let (store, books) = rated_library().await;
        let candidate =
            |source: &str, product_id: &str, title: &str| crate::candidates::StorefrontCandidate {
                source: source.into(),
                product_id: product_id.into(),
                title: title.into(),
                authors: Some("Ada Author".into()),
                narrators: None,
                series: None,
                series_index: None,
                asin: None,
                isbn: None,
                cover_url: None,
                seed_categories: None,
                origin: "author search".into(),
                seed_title: None,
                starter_genre: None,
                store_editions: Vec::new(),
                subtitle: None,
                description: None,
                publisher: None,
                length_minutes: None,
                published_at: None,
                categories: None,
                language: None,
                price_cents: Some(1499),
                currency: Some("USD".into()),
                price_label: Some("$14.99".into()),
                rating_overall: None,
                rating_count: None,
                is_abridged: None,
                audible_rank: None,
            };
        let opts = RecommendOptions {
            include_purchase_hints: false,
            embeddings_enabled: false,
            ..RecommendOptions::default()
        };
        let recs = rank_candidates(
            &store,
            &SourceRegistry::new(),
            &books,
            &TasteWeights::new(),
            &[],
            vec![
                candidate("audible", "B0AUDIBLE1", "Audible Pick"),
                candidate("chirp", "chirp-42", "Chirp Pick"),
            ],
            &opts,
        )
        .await
        .unwrap();
        for (title, source) in [("Audible Pick", "audible"), ("Chirp Pick", "chirp")] {
            let rec = recs
                .iter()
                .find(|r| r.title == title)
                .unwrap_or_else(|| panic!("{title} missing: {recs:?}"));
            let hint = rec
                .purchase_hints
                .iter()
                .find(|h| h.source == source)
                .unwrap_or_else(|| panic!("{title} has no {source} hint"));
            assert_eq!(hint.price_cents, Some(1499));
            assert_eq!(hint.list_price_cents, Some(1499));
            assert_eq!(hint.currency.as_deref(), Some("USD"));
            assert_eq!(hint.price_label.as_deref(), Some("$14.99"));
            assert_eq!(hint.list_price_label.as_deref(), Some("$14.99"));
        }
        let audible = recs.iter().find(|r| r.title == "Audible Pick").unwrap();
        assert!(audible.purchase_hints[0].url.is_some(), "{audible:?}");
    }

    /// A store holding one unfinished book rated 5 by "Ada Author", and its rows.
    async fn rated_library() -> (LibraryStore, Vec<BookRecord>) {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("audible", "acct", "us", Some("Main"), true, "audible")
            .await
            .unwrap();
        let mut book = bookclerk_library::NewBook::minimal("B000001", "acct", "us", "Liked");
        book.authors = Some("Ada Author".into());
        let book = store.upsert_book(&book).await.unwrap();
        store
            .update_user_fields(
                &book.uuid,
                "acct",
                &bookclerk_library::UserBookFields {
                    rating_overall: Some(5.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let books = store.list_books(None).await.unwrap();
        (store, books)
    }

    /// Owning the first book of a series does not count as owning its
    /// sequel, so a wish for the sequel reaches the queue and the shelves.
    #[tokio::test]
    async fn owning_a_series_namesake_does_not_own_its_sequel() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        store
            .upsert_account("audible", "acct", "us", Some("Main"), true, "audible")
            .await
            .unwrap();
        let mut dune = bookclerk_library::NewBook::minimal("B002V1OF70", "acct", "us", "Dune");
        dune.authors = Some("Frank Herbert".into());
        dune.asin = Some("B002V1OF70".into());
        dune.series = Some("Dune".into());
        dune.series_index = Some("1".into());
        store.upsert_book(&dune).await.unwrap();
        let entry = |title: &str, isbn: Option<&str>| GlobalQueueEntry {
            work_key: String::new(),
            work_id: None,
            title: title.into(),
            authors: Some("Frank Herbert".into()),
            asin: None,
            isbn: isbn.map(str::to_string),
            cover_url: None,
            description: None,
            subtitle: None,
            narrators: None,
            series: None,
            series_index: None,
            publisher: None,
            length_minutes: None,
            published_at: None,
            genres: None,
            language: None,
            store_editions: Vec::new(),
            purchase_hints: Vec::new(),
            wish_count: 1,
            wishers: Vec::new(),
            sample_uuids: Vec::new(),
            first_requested_at: chrono::Utc::now(),
            last_requested_at: chrono::Utc::now(),
        };
        assert!(library_owns_work(&store, &entry("Dune", None))
            .await
            .unwrap());
        assert!(
            !library_owns_work(&store, &entry("Dune Messiah", Some("9781427202376")))
                .await
                .unwrap()
        );
        assert!(!library_owns_work(&store, &entry("Children of Dune", None))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_highly_rated_book_makes_its_author_liked() {
        let (store, books) = rated_library().await;
        let profile = build_taste_profile(
            &store,
            &books,
            &TasteWeights::new(),
            &[],
            &RecommendOptions::default(),
        )
        .await
        .unwrap();
        let weight = profile.liked_authors.get("ada author").copied();
        assert!(
            weight.is_some_and(|w| w >= 2.0),
            "a 5-star rating adds at least 2 to its author: {weight:?}"
        );
    }

    #[tokio::test]
    async fn shelf_taste_weights_a_highly_rated_book() {
        let (_store, books) = rated_library().await;
        let taste = shelf_taste(&books, &TasteWeights::new(), &[]);
        assert_eq!(
            taste.authors.get("ada author"),
            Some(&("Ada Author".to_owned(), 2.0))
        );
        assert!(!taste.owned_sources.is_empty());
    }

    #[test]
    fn a_blended_listen_counts_at_its_book_weight() {
        let row = |book_uuid: &str| bookclerk_library::ListeningProgressRecord {
            id: 1,
            identity_id: None,
            provider: String::from("abs"),
            external_user_id: String::from("u1"),
            book_uuid: Some(book_uuid.to_owned()),
            work_id: None,
            external_item_id: String::from("item"),
            title: None,
            authors: Some(String::from("Ann Author")),
            asin: None,
            isbn: None,
            progress: Some(0.5),
            current_time_seconds: Some(3_600.0),
            duration_seconds: Some(7_200.0),
            is_finished: false,
            last_listened_at: None,
            updated_at: chrono::Utc::now(),
        };
        let full = listening_engagement(&row("mine"));
        let mut weights = TasteWeights::new();
        weights.insert(String::from("household"), 0.5);
        let taste = shelf_taste(&[], &weights, &[row("mine"), row("household")]);
        let weight = taste.authors.get("ann author").map(|(_, w)| *w);
        assert_eq!(weight, Some(full * 1.5));
    }

    #[test]
    fn wish_count_dominates_taste_in_combined_score() {
        // Strong taste, single wish vs weak taste, two wishes.
        let solo = combine_wishlist_score(25.0, 1);
        let duo = combine_wishlist_score(5.0, 2);
        assert!(
            duo > solo,
            "two wishers should outrank one even with weaker taste ({duo} vs {solo})"
        );
        // Equal wish counts: taste breaks the tie.
        assert!(combine_wishlist_score(12.0, 2) > combine_wishlist_score(3.0, 2));
        assert_eq!(combine_wishlist_score(0.0, 1), WISH_COUNT_WEIGHT);
    }

    #[test]
    fn parse_series_index_variants() {
        assert_eq!(parse_series_index(Some("1")), Some(1.0));
        assert_eq!(parse_series_index(Some("1.5")), Some(1.5));
        assert_eq!(parse_series_index(Some("Book 3")), Some(3.0));
        assert_eq!(parse_series_index(Some("02")), Some(2.0));
        assert_eq!(parse_series_index(Some("")), None);
        assert_eq!(parse_series_index(None), None);
    }

    #[test]
    fn series_completion_boosts_next_and_listening() {
        let mut affinity = HashMap::new();
        affinity.insert(
            String::from("mistborn"),
            SeriesAffinity {
                owned_count: 2,
                finished_count: 1,
                listening_count: 2,
                active_listening: true,
                active_listen_weight: 3.0,
                listen_engagement_sum: 6.0,
                max_owned_index: Some(2.0),
            },
        );
        let mut score = 1.0;
        let mut reasons = Vec::new();
        let mut categories = Vec::new();
        apply_series_completion_score(
            &Some(String::from("Mistborn")),
            Some("3"),
            &affinity,
            &mut score,
            &mut reasons,
            &mut categories,
        );
        assert!(categories.iter().any(|c| c == "finish_series"));
        assert!(categories.iter().any(|c| c == "keep_listening"));
        assert!(
            score > 20.0,
            "expected strong completion+listening score, got {score}"
        );
        assert!(reasons.iter().any(|r| r.contains("complete series")));
        assert!(reasons.iter().any(|r| r.contains("next book")));
        assert!(reasons
            .iter()
            .any(|r| r.contains("actively being listened")));
        assert!(reasons.iter().any(|r| r.contains("multiple books")));
    }

    #[test]
    fn listening_engagement_scales_with_hours_not_just_percent() {
        use bookclerk_library::ListeningProgressRecord;
        use chrono::Utc;

        fn row(
            progress: Option<f64>,
            current_time_seconds: Option<f64>,
            duration_seconds: Option<f64>,
            is_finished: bool,
        ) -> ListeningProgressRecord {
            ListeningProgressRecord {
                id: 1,
                identity_id: None,
                provider: String::from("abs"),
                external_user_id: String::from("u1"),
                book_uuid: None,
                work_id: None,
                external_item_id: String::from("item"),
                title: None,
                authors: None,
                asin: None,
                isbn: None,
                progress,
                current_time_seconds,
                duration_seconds,
                is_finished,
                last_listened_at: None,
                updated_at: Utc::now(),
            }
        }

        // Same 50% completion — long title (15 h in) must beat short (1.5 h in).
        let half_of_long = listening_engagement(&row(
            Some(0.5),
            Some(15.0 * 3600.0),
            Some(30.0 * 3600.0),
            false,
        ));
        let half_of_short = listening_engagement(&row(
            Some(0.5),
            Some(1.5 * 3600.0),
            Some(3.0 * 3600.0),
            false,
        ));
        assert!(
            half_of_long > half_of_short * 1.5,
            "hours should dominate percent: long={half_of_long} short={half_of_short}"
        );

        let finished_short = listening_engagement(&row(
            Some(1.0),
            Some(3.0 * 3600.0),
            Some(3.0 * 3600.0),
            true,
        ));
        let finished_long = listening_engagement(&row(
            Some(1.0),
            Some(30.0 * 3600.0),
            Some(30.0 * 3600.0),
            true,
        ));
        assert!(
            finished_long > finished_short,
            "longer finished titles should weigh more: long={finished_long} short={finished_short}"
        );

        // A deep listen on a long book should outrank finishing a short one.
        assert!(
            half_of_long > finished_short,
            "15h into a long book > finishing a 3h book: half_long={half_of_long} fin_short={finished_short}"
        );

        let none = listening_engagement(&row(None, None, None, false));
        assert_eq!(none, 0.0);
    }
}
