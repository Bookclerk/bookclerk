//! Recommendation engine: storefront candidates scored against local taste.
//!
//! Flow:
//! 1. Build local taste seeds (finished / rated / listening)
//! 2. Expand **unowned** candidates from storefronts (Libro, Audible, Chirp, GA)
//! 3. Score those candidates with local signals + embedding similarity
//!    (series completion + active listening are first-class heuristics)
//! 4. Merge open title requests; attach purchase hints

use std::collections::{HashMap, HashSet};

use bookclerk_library::{GlobalQueueEntry, LibraryStore, TasteTitle};
use bookclerk_source::SourceRegistry;

use crate::listening::ListeningSignal;

use crate::candidates::{gather_storefront_candidates, select_taste_seeds, CandidateFetchOptions};
use crate::embed::{
    bytes_to_vector, cosine, embed_text, is_stand_in, text_hash, text_vectors, EmbedFields,
    VectorSource,
};
use crate::error::Result;
use crate::identity::{
    merge_recommendation, push_edition, push_shelf_category, recommendation_map_key,
    same_edition_identifiers, StoreEdition,
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
    /// The vectors compared for similarity: the space the library's
    /// rankings read, and what embeds a text that has no vector there.
    pub vectors: VectorSource,
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
    /// When false, skip embedding similarity and use heuristic ranking only.
    pub embeddings_enabled: bool,
}

impl Default for RecommendOptions {
    fn default() -> Self {
        Self {
            limit: 20,
            vectors: VectorSource::stand_in(),
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
            embeddings_enabled: true,
        }
    }
}

/// One ranked recommendation (typically an unowned storefront title).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct Recommendation {
    /// Library `works.id` of the work the card's edition belongs to, when
    /// the library's data model places it (a wish, or a store listing from
    /// the shared listing pool). Cards for one work are one card.
    #[schema(example = "5b7e2c1d-8a4f-4e3b-9c6d-2f1a0b9e8d7c")]
    pub work_id: Option<String>,
    /// `expressions.id` of the card's edition, when known: the recording
    /// (or text) it is. Only listings of this expression add prices to the
    /// card. Used while ranking; never stored or sent.
    #[serde(skip)]
    pub expression_id: Option<String>,
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
    /// When `Some(true)`, a storefront marks the edition explicit (adult)
    /// content; `None` when none said (a wishlist entry says nothing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub is_explicit: Option<bool>,
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
    /// Branches of this library that currently list this title (they
    /// answered a catalog lookup for its work). Filled when the feed is
    /// read, never stored with it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available_through: Vec<crate::availability::BranchAvailability>,
}

/// Per-book multipliers on taste weight, keyed by book uuid; a book that is
/// not listed counts in full. Household books blended into a member's taste
/// carry a multiplier below one.
pub(crate) type TasteWeights = HashMap<String, f64>;

/// The multiplier `weights` gives `book` (one when unlisted).
fn taste_multiplier(weights: &TasteWeights, book: &TasteTitle) -> f64 {
    weights.get(&book.work_id).copied().unwrap_or(1.0)
}

/// The multiplier `weights` gives a listening `row`: its book's multiplier,
/// so a blended household listen counts as much as the book it is for (one
/// when the row names no listed book).
fn listening_multiplier(weights: &TasteWeights, row: &ListeningSignal) -> f64 {
    row.work_id
        .as_deref()
        .and_then(|work| weights.get(work))
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
    /// The work every wish of this entry is for. Wishes for one work are
    /// one entry even when their `work_key`s differ, so a client matches
    /// its own wishes on this id first and on `work_key` otherwise.
    #[schema(example = "0b7e2c1d-5f3a-4e6b-9c8d-1a2b3c4d5e6f")]
    pub work_id: String,
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
    pub(crate) fn from_global(
        entry: GlobalQueueEntry,
        taste_score: f64,
        mut reasons: Vec<String>,
    ) -> Self {
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
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SeriesAffinity {
    /// Owned titles in this series (any acquire status).
    pub(crate) owned_count: usize,
    /// Owned titles in this series marked finished.
    finished_count: usize,
    /// Owned books in this series that have listening activity.
    listening_count: usize,
    /// At least one in-progress (not finished) listen in this series.
    active_listening: bool,
    /// Max continuous engagement among in-progress titles (≈0–6).
    active_listen_weight: f64,
    /// Sum of continuous engagement across listened titles in the series.
    pub(crate) listen_engagement_sum: f64,
    /// Highest parsed series index among owned titles, used to boost the next book.
    max_owned_index: Option<f64>,
}

/// Every work the household holds, with everyone's finishes, ratings and
/// tags merged onto it.
///
/// # Errors
///
/// Returns the library read error.
pub(crate) async fn household_taste(library: &LibraryStore) -> Result<Vec<TasteTitle>> {
    let mut titles = library.taste_titles().await?;
    let work_ids: Vec<String> = titles.iter().map(|t| t.work_id.clone()).collect();
    let states = library.user_title_states_for_works(&work_ids).await?;
    bookclerk_library::apply_user_states(&mut titles, &states, &[]);
    Ok(titles)
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
    let books = household_taste(library).await?;
    let mut recs = recommend_all(library, registry, &books, opts).await?;
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
    // One library read serves both the candidates and the shelf taste.
    let books = household_taste(library).await?;
    let recs = recommend_all(library, registry, &books, opts).await?;
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

/// Ranks every entry of the shared request queue against **household**
/// taste, for the `discover_request_queue` job to store.
///
/// This ranking is **not** personalized: portal `external_user_id` filters are
/// ignored so every viewer sees the same shared queue order (the household
/// library and all listening progress). Every wish names its work, so one
/// ranking per work matches the entry the read builds.
///
/// Only taste is ranked here: the wish-count boost and the wish reasons are
/// added when the queue is read, from the live counts, so a new wish moves an
/// entry before the next pass. Entries the library already holds are kept
/// with `owned` set, so the read can hide them.
///
/// # Arguments
///
/// * `library` - Open library store.
/// * `opts` - Ranking options (embedder, listening filters).
///
/// # Returns
///
/// One ranking per merged queue entry (empty when the queue is).
///
/// # Errors
///
/// Returns the library read error, or the embedder's open error.
pub async fn rank_request_queue_taste(
    library: &LibraryStore,
    opts: &RecommendOptions,
) -> Result<Vec<bookclerk_library::QueueRank>> {
    let mut opts = opts.clone();
    opts.external_user_id = None;

    let queue = library.list_global_request_queue().await?;
    if queue.is_empty() {
        return Ok(Vec::new());
    }

    let books = household_taste(library).await?;
    let listening = load_listening(library, &opts).await?;
    let profile =
        build_taste_profile(library, &books, &TasteWeights::new(), &listening, &opts).await?;
    let texts: Vec<String> = queue.iter().map(queue_entry_text).collect();
    let similarity = TasteSimilarity::load(library, &profile, &opts, &texts).await?;

    let mut ranks = Vec::with_capacity(queue.len());
    for (entry, text) in queue.into_iter().zip(&texts) {
        let owned = library_owns_work(library, &entry).await?;
        let (taste_score, reasons, _) = if owned {
            (0.0, Vec::new(), Vec::new())
        } else {
            score_work_against_taste(
                entry.authors.as_deref(),
                None,
                None,
                None,
                &profile,
                similarity.boost(text),
            )
        };
        ranks.push(bookclerk_library::QueueRank {
            work_id: entry.work_id,
            owned,
            taste_score,
            reasons,
        });
    }
    Ok(ranks)
}

/// Load optional listening rows for ranking (empty when disabled / no providers).
async fn load_listening(
    library: &LibraryStore,
    opts: &RecommendOptions,
) -> Result<Vec<ListeningSignal>> {
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
    ListeningSignal::load(library, rows).await
}

/// Full scored candidate pool (not truncated) used by flat list + shelves.
///
/// `books` is the library as the caller read it, so one call reads it once.
async fn recommend_all(
    library: &LibraryStore,
    registry: &SourceRegistry,
    books: &[TasteTitle],
    opts: &RecommendOptions,
) -> Result<Vec<Recommendation>> {
    let (owned_asins, owned_isbns, owned_product_keys) = owned_identity_sets(books);

    let listening = load_listening(library, opts).await?;
    let seeds = select_taste_seeds(books, &engagement_by_book(&listening));
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
        &reader_languages(books),
        build_taste_scores(books, &TasteWeights::new(), &listening),
        candidates,
        opts,
    )
    .await
}

/// Strongest listening engagement per book uuid (rows without a book are skipped).
pub(crate) fn engagement_by_book(listening: &[ListeningSignal]) -> HashMap<String, f64> {
    let mut by_uuid: HashMap<String, f64> = HashMap::new();
    for row in listening {
        let weight = listening_engagement(row);
        if weight <= 0.0 {
            continue;
        }
        if let Some(uuid) = &row.work_id {
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
    books: &[TasteTitle],
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
/// * `languages` - Normalized codes the reader listens in
///   ([`reader_languages`] of the taste books); candidates in another
///   language are dropped.
/// * `scores` - What the taste likes ([`build_taste_scores`] of the
///   subject's books, weights and listening).
/// * `candidates` - Unowned storefront candidates, already merged by identity;
///   those for a work the library holds are dropped here
///   ([`retain_unowned_works`]).
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
    languages: &HashSet<String>,
    scores: TasteScores,
    candidates: Vec<crate::candidates::StorefrontCandidate>,
    opts: &RecommendOptions,
) -> Result<Vec<Recommendation>> {
    let candidates = retain_reader_languages(candidates, languages);
    let candidates = retain_unowned_works(library, candidates).await?;
    let profile = TasteProfile::from_scores(library, scores, opts).await?;
    let queue = library.list_global_request_queue().await?;
    let candidate_texts: Vec<String> = candidates.iter().map(candidate_text).collect();
    let queue_texts: Vec<String> = queue.iter().map(queue_entry_text).collect();
    let all_texts: Vec<String> = candidate_texts
        .iter()
        .chain(&queue_texts)
        .cloned()
        .collect();
    let similarity = TasteSimilarity::load(library, &profile, opts, &all_texts).await?;
    let mut scored: HashMap<String, Recommendation> = HashMap::new();

    for (c, text) in candidates.into_iter().zip(&candidate_texts) {
        let (mut score, mut reasons, mut categories) = score_work_against_taste(
            c.authors.as_deref(),
            c.narrators.as_deref(),
            c.series.as_deref(),
            c.series_index.as_deref(),
            &profile,
            similarity.boost(text),
        );
        // Origin / category / deals are candidate-specific (not on wishlist rows).
        reasons.insert(0, c.origin.clone());
        let genres = genre_match(
            c.categories.as_deref(),
            c.seed_categories.as_deref(),
            &profile.liked_categories,
        );
        if !genres.reasons.is_empty() {
            score += genres.score;
            reasons.extend(genres.reasons);
            push_shelf_category(&mut categories, "genre");
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
            work_id: c.work_id,
            expression_id: c.expression_id,
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
            is_explicit: c.is_explicit,
            seed_title: c.seed_title,
            starter_genre: c.starter_genre,
            ..Default::default()
        };
        rec.work_key = recommendation_map_key(&rec);
        upsert_recommendation(&mut scored, rec);
    }

    // Global wishlist works: recommend taste + heavy multi-user wish boost.
    for (entry, text) in queue.into_iter().zip(&queue_texts) {
        if library_owns_work(library, &entry).await? {
            continue;
        }
        let (taste_score, mut reasons, mut categories) = score_work_against_taste(
            entry.authors.as_deref(),
            None,
            None,
            None,
            &profile,
            similarity.boost(text),
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
            work_id: Some(entry.work_id),
            expression_id: entry.expression_id,
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

/// What a taste likes, without the embedding centroid: the part of a
/// [`TasteProfile`] that is stored with a subject's taste
/// ([`crate::stored_taste::StoredTaste`]) so a ranking need not read the
/// library again.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct TasteScores {
    /// Lowercased author → finish/rating/listen weight.
    pub(crate) liked_authors: HashMap<String, f64>,
    /// Lowercased narrator → finish/rating weight.
    pub(crate) liked_narrators: HashMap<String, f64>,
    /// Lowercased category/subject → finish/rating weight.
    pub(crate) liked_categories: HashMap<String, f64>,
    /// Lowercased series name → ownership and listening affinity.
    pub(crate) series_affinity: HashMap<String, SeriesAffinity>,
    /// Works whose stored embeddings make up the seed centroid.
    pub(crate) seed_work_ids: std::collections::BTreeSet<String>,
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

impl TasteProfile {
    /// The profile of `scores`, with the centroid of their seed works'
    /// stored embeddings.
    async fn from_scores(
        library: &LibraryStore,
        scores: TasteScores,
        opts: &RecommendOptions,
    ) -> Result<Self> {
        let seed_centroid = seed_embedding_centroid(library, &scores.seed_work_ids, opts).await?;
        Ok(Self {
            liked_authors: scores.liked_authors,
            liked_narrators: scores.liked_narrators,
            liked_categories: scores.liked_categories,
            series_affinity: scores.series_affinity,
            seed_centroid,
        })
    }
}

/// Accumulates liked people/categories, series affinity, and a seed embedding centroid.
async fn build_taste_profile(
    library: &LibraryStore,
    books: &[TasteTitle],
    weights: &TasteWeights,
    listening: &[ListeningSignal],
    opts: &RecommendOptions,
) -> Result<TasteProfile> {
    TasteProfile::from_scores(library, build_taste_scores(books, weights, listening), opts).await
}

/// Accumulates liked people/categories, series affinity and the seed works
/// of one taste.
///
/// # Arguments
///
/// * `books` - Books whose finishes, ratings and series shape the taste.
/// * `weights` - How much each book counts (see [`TasteWeights`]).
/// * `listening` - Listening rows that add to the liked authors and series.
pub(crate) fn build_taste_scores(
    books: &[TasteTitle],
    weights: &TasteWeights,
    listening: &[ListeningSignal],
) -> TasteScores {
    let mut liked_authors: HashMap<String, f64> = HashMap::new();
    let mut liked_narrators: HashMap<String, f64> = HashMap::new();
    let mut liked_categories: HashMap<String, f64> = HashMap::new();
    let mut seed_work_ids: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
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
        if book.acquired {
            weight += 0.5;
        }
        weight *= taste_multiplier(weights, book);
        if weight <= 0.0 {
            continue;
        }
        seed_work_ids.insert(book.work_id.clone());
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

    for row in listening {
        let weight = listening_engagement(row) * listening_multiplier(weights, row);
        if weight <= 0.0 {
            continue;
        }
        if let Some(wid) = &row.work_id {
            seed_work_ids.insert(wid.clone());
        }
        if let Some(uuid) = &row.work_id {
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
    TasteScores {
        liked_authors,
        liked_narrators,
        liked_categories,
        series_affinity,
        seed_work_ids,
    }
}

/// Scores a candidate against liked people, series completion, and embedding similarity.
fn score_work_against_taste(
    authors: Option<&str>,
    narrators: Option<&str>,
    series: Option<&str>,
    series_index: Option<&str>,
    profile: &TasteProfile,
    similar: Option<SimilarityBoost>,
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

    if let Some(similar) = similar {
        score += similar.score;
        reasons.push(format!(
            "similar to titles you finish (sim {:.2})",
            similar.similarity
        ));
        push_shelf_category(&mut categories, "similar_taste");
    }

    (score, reasons, categories)
}

/// What a text's closeness to a taste's seed works adds to its score.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SimilarityBoost {
    /// Points added to the score.
    score: f64,
    /// Cosine similarity between the text's vector and the seed centroid.
    similarity: f32,
}

/// Least cosine similarity a hash vector needs to count as similar.
const STAND_IN_MIN_SIMILARITY: f32 = 0.15;

/// Points per unit of cosine similarity between hash vectors.
const STAND_IN_SIMILARITY_WEIGHT: f64 = 12.0;

/// Fewest texts in one ranking for a model's similarities to be judged
/// against each other; a smaller ranking gets no similarity score.
const MODEL_MIN_POOL: usize = 12;

/// How far above the ranking's mean similarity, in standard deviations, a
/// text must be to count as similar.
const MODEL_MIN_STANDING: f32 = 0.5;

/// Standing past which more similarity adds nothing.
const MODEL_MAX_STANDING: f32 = 3.0;

/// Points per standard deviation above the mean.
const MODEL_STANDING_WEIGHT: f64 = 2.5;

/// Longest one ranking spends embedding texts that have no stored vector;
/// the rest are ranked without similarity and embedded by a later ranking.
const TEXT_EMBED_BUDGET: std::time::Duration = std::time::Duration::from_secs(45);

/// How close each text of one ranking is to a taste's seed works.
///
/// A model's vectors all point roughly the same way, so their cosine
/// similarities sit in a narrow band well above zero and a fixed threshold
/// means nothing across models. The texts of one ranking are therefore
/// judged against each other: a text counts as similar when it stands
/// above the ranking's mean by [`MODEL_MIN_STANDING`] deviations. The
/// stand-in's hash vectors are near zero unless texts share words, and
/// keep their fixed threshold.
struct TasteSimilarity {
    /// Similarity to the seed centroid by [`text_hash`]; a text without a
    /// vector has none.
    similarities: HashMap<String, f32>,
    /// Mean and standard deviation of `similarities`, for a model's space
    /// with at least [`MODEL_MIN_POOL`] texts; `None` otherwise.
    spread: Option<(f32, f32)>,
    /// Whether the vectors are the stand-in's.
    stand_in: bool,
}

impl TasteSimilarity {
    /// No text is similar: similarity is off, or the taste has no centroid.
    fn none() -> Self {
        Self {
            similarities: HashMap::new(),
            spread: None,
            stand_in: true,
        }
    }

    /// The similarities of `texts` to `profile`'s seed centroid, in the
    /// space `opts` names. Texts with no stored vector are embedded and
    /// stored when `opts` carries an embedder of that space.
    ///
    /// # Errors
    ///
    /// Returns the library error.
    async fn load(
        library: &LibraryStore,
        profile: &TasteProfile,
        opts: &RecommendOptions,
        texts: &[String],
    ) -> Result<Self> {
        let Some(centroid) = profile
            .seed_centroid
            .as_deref()
            .filter(|_| opts.embeddings_enabled)
        else {
            return Ok(Self::none());
        };
        let space = &opts.vectors.space;
        let deadline = std::time::Instant::now() + TEXT_EMBED_BUDGET;
        let vectors = match &opts.vectors.embedder {
            Some(shared) => {
                let mut embedder = shared.lock().await;
                text_vectors(
                    library,
                    space,
                    Some(embedder.as_mut()),
                    texts,
                    Some(deadline),
                )
                .await?
            }
            None => text_vectors(library, space, None, texts, None).await?,
        };
        let similarities: HashMap<String, f32> = vectors
            .into_iter()
            .filter(|(_, vector)| vector.len() == centroid.len())
            .map(|(hash, vector)| (hash, cosine(centroid, &vector)))
            .collect();
        let stand_in = is_stand_in(space);
        let spread = (!stand_in && similarities.len() >= MODEL_MIN_POOL)
            .then(|| mean_and_deviation(similarities.values().copied()))
            .filter(|(_, deviation)| *deviation > 1e-6);
        Ok(Self {
            similarities,
            spread,
            stand_in,
        })
    }

    /// What `text`'s similarity adds to its score, when it counts as
    /// similar.
    fn boost(&self, text: &str) -> Option<SimilarityBoost> {
        let similarity = *self.similarities.get(&text_hash(text))?;
        if self.stand_in {
            return (similarity > STAND_IN_MIN_SIMILARITY).then(|| SimilarityBoost {
                score: f64::from(similarity) * STAND_IN_SIMILARITY_WEIGHT,
                similarity,
            });
        }
        let (mean, deviation) = self.spread?;
        let standing = (similarity - mean) / deviation;
        (standing > MODEL_MIN_STANDING).then(|| SimilarityBoost {
            score: f64::from(standing.min(MODEL_MAX_STANDING)) * MODEL_STANDING_WEIGHT,
            similarity,
        })
    }
}

/// Mean and (population) standard deviation of `values`; zeros when empty.
fn mean_and_deviation(values: impl Iterator<Item = f32> + Clone) -> (f32, f32) {
    let (sum, count) = values.clone().fold((0.0f64, 0u32), |(sum, count), v| {
        (sum + f64::from(v), count + 1)
    });
    if count == 0 {
        return (0.0, 0.0);
    }
    let mean = sum / f64::from(count);
    let variance = values.map(|v| (f64::from(v) - mean).powi(2)).sum::<f64>() / f64::from(count);
    // Narrowing a mean and a deviation of f32 values back to f32.
    #[allow(clippy::cast_possible_truncation)]
    (mean as f32, variance.sqrt() as f32)
}

/// The text embedded for a storefront candidate.
fn candidate_text(candidate: &crate::candidates::StorefrontCandidate) -> String {
    embed_text(&EmbedFields {
        title: &candidate.title,
        authors: candidate.authors.as_deref(),
        series: candidate.series.as_deref(),
        genres: candidate.categories.as_deref(),
        description: candidate.description.as_deref(),
    })
}

/// The text embedded for a wishlist entry.
fn queue_entry_text(entry: &bookclerk_library::GlobalQueueEntry) -> String {
    embed_text(&EmbedFields {
        title: &entry.title,
        authors: entry.authors.as_deref(),
        series: None,
        genres: None,
        description: entry.description.as_deref(),
    })
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

/// Whether the household library holds the work a wishlist entry is for:
/// whether a library title's listing reaches that work. Every wish names
/// its work, so no title or identifier is compared here.
///
/// # Errors
///
/// Returns the library read error.
async fn library_owns_work(library: &LibraryStore, entry: &GlobalQueueEntry) -> Result<bool> {
    Ok(!library
        .held_works(std::slice::from_ref(&entry.work_id))
        .await?
        .is_empty())
}

/// Drops storefront candidates for a work the household library holds.
///
/// The ASIN / ISBN / product sets catch the exact editions the library
/// holds; another edition of an owned book (a different narrator, store or
/// ASIN) has none of those ids. A candidate read from the shared listing
/// pool carries the work its listing resolved into; any other candidate is
/// placed by the library's resolver as it would a scanned listing (shared
/// identifiers, then the work's identifiers, then the work-level
/// title-and-author match). A candidate whose work has a held title is
/// dropped. A sequel that shares
/// its series' name is its own work and stays. Whether the works are held
/// is read for all of them at once ([`LibraryStore::held_works`]).
///
/// # Arguments
///
/// * `library` - Library store whose works and holdings decide.
/// * `candidates` - Storefront candidates, already free of owned ids.
///
/// # Returns
///
/// The candidates whose work the library does not hold, in their order,
/// each carrying the work it was placed in (`work_id`) when it has one.
///
/// # Errors
///
/// Returns the library read error.
pub(crate) async fn retain_unowned_works(
    library: &LibraryStore,
    candidates: Vec<crate::candidates::StorefrontCandidate>,
) -> Result<Vec<crate::candidates::StorefrontCandidate>> {
    if candidates.is_empty() {
        return Ok(candidates);
    }
    // Each candidate's work: a candidate read from the listing pool already
    // knows it; only a live store hit is resolved here, one at a time.
    let mut placed = Vec::with_capacity(candidates.len());
    for c in candidates {
        let work_id = match (&c.work_id, c.title.trim().is_empty()) {
            (Some(work_id), _) => Some(work_id.clone()),
            (None, true) => None,
            (None, false) => {
                let query = bookclerk_library::TitleQuery {
                    title: &c.title,
                    authors: c.authors.as_deref(),
                    asin: c.asin.as_deref(),
                    isbn: c.isbn.as_deref(),
                    series: c.series.as_deref(),
                    series_index: c.series_index.as_deref(),
                };
                library.work_for(&query).await?
            }
        };
        placed.push((c, work_id));
    }
    // Which of those works the library holds, in one chunked read rather
    // than one per work: a page of candidates is a few hundred works.
    let works: Vec<String> = placed
        .iter()
        .filter_map(|(_, work_id)| work_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let held = library.held_works(&works).await?;
    Ok(placed
        .into_iter()
        .filter(|(_, work_id)| work_id.as_ref().is_none_or(|w| !held.contains(w)))
        .map(|(mut c, work_id)| {
            // A live hit keeps the work it resolved to, so the ranking
            // makes one card per work without comparing titles.
            c.work_id = work_id;
            c
        })
        .collect())
}

/// The languages a taste listens in: the normalized language of each taste
/// book whose language is known, or the default preferred language
/// ([`bookclerk_source::default_preferred_language`]) when none is.
///
/// # Arguments
///
/// * `taste_books` - The books the ranking's taste comes from.
///
/// # Returns
///
/// Normalized language codes (`en`, `es`, …), never empty.
pub(crate) fn reader_languages(taste_books: &[TasteTitle]) -> HashSet<String> {
    let mut languages: HashSet<String> = taste_books
        .iter()
        .filter_map(|b| b.language.as_deref())
        .filter_map(bookclerk_source::normalize_language)
        .collect();
    if languages.is_empty() {
        languages.insert(bookclerk_source::default_preferred_language().to_owned());
    }
    languages
}

/// Drops candidates in a language the reader does not listen in.
///
/// A candidate whose language the store did not give (or gave in a form
/// [`bookclerk_source::normalize_language`] does not know) is kept, so a
/// store that leaves the language out loses no titles.
///
/// # Arguments
///
/// * `candidates` - Storefront candidates, in rank order.
/// * `languages` - Normalized codes from [`reader_languages`].
///
/// # Returns
///
/// The candidates in one of `languages` or of unknown language, in order.
pub(crate) fn retain_reader_languages(
    mut candidates: Vec<crate::candidates::StorefrontCandidate>,
    languages: &HashSet<String>,
) -> Vec<crate::candidates::StorefrontCandidate> {
    candidates.retain(|c| {
        c.language
            .as_deref()
            .and_then(bookclerk_source::normalize_language)
            .is_none_or(|code| languages.contains(&code))
    });
    candidates
}

/// How the library's data model relates two cards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CardRelation {
    /// Different works, or nothing ties them together.
    Unrelated,
    /// One work in two different recordings (a narration and a full-cast
    /// dramatisation, or two narrators): one card, but their prices are for
    /// different things, so only the owning card's are shown.
    SameWork,
    /// The same recording: one expression, or editions that share an ISBN
    /// or ASIN. Other stores' listings of it are more places to buy it.
    SameRecording,
}

/// How `a` and `b` relate, from the data model's work and expression and
/// from shared ISBNs and ASINs only. Titles and authors are never compared:
/// the library's resolver has already placed every pooled listing and every
/// wish in a work, and a store hit it could not place stays its own card.
///
/// # Arguments
///
/// * `a` - One card.
/// * `b` - The other card.
fn card_relation(a: &Recommendation, b: &Recommendation) -> CardRelation {
    let work = |r: &Recommendation| r.work_id.clone().filter(|w| !w.trim().is_empty());
    let expression = |r: &Recommendation| r.expression_id.clone().filter(|x| !x.trim().is_empty());
    let same_work = match (work(a), work(b)) {
        (Some(wa), Some(wb)) => Some(wa == wb),
        _ => None,
    };
    if same_work == Some(false) {
        return CardRelation::Unrelated;
    }
    let same_recording = match (expression(a), expression(b)) {
        (Some(xa), Some(xb)) => Some(xa == xb),
        _ => same_edition_identifiers(
            a.asin.as_deref(),
            a.isbn.as_deref(),
            b.asin.as_deref(),
            b.isbn.as_deref(),
        ),
    };
    match (same_recording, same_work) {
        (Some(true), _) => CardRelation::SameRecording,
        (_, Some(true)) => CardRelation::SameWork,
        _ => CardRelation::Unrelated,
    }
}

/// Whether `challenger` should own a card instead of `holder`: a wish
/// always owns its work's card (it names the edition someone asked for);
/// between two store cards, the higher score does.
fn takes_card_from(challenger: &Recommendation, holder: &Recommendation) -> bool {
    match (challenger.from_request, holder.from_request) {
        (true, false) => true,
        (false, true) => false,
        _ => challenger.score > holder.score,
    }
}

/// Folds a store listing of the same recording into a wish's card.
///
/// The wish describes the edition someone asked for, so every field it has
/// stays; the listing only fills blanks, and adds its store edition and
/// price as another place to buy the same recording.
///
/// # Arguments
///
/// * `wish` - The wish's card.
/// * `listing` - A store card for the same recording.
fn fold_recording_into_wish(wish: &mut Recommendation, listing: Recommendation) {
    for ed in listing.store_editions {
        push_edition(&mut wish.store_editions, ed);
    }
    if let (Some(s), Some(p)) = (
        listing.candidate_source.as_deref(),
        listing.candidate_product_id.as_deref(),
    ) {
        push_edition(&mut wish.store_editions, StoreEdition::new(s, p));
    }
    for hint in listing.purchase_hints {
        if !wish.purchase_hints.iter().any(|h| {
            h.source.eq_ignore_ascii_case(&hint.source)
                && h.product_id.eq_ignore_ascii_case(&hint.product_id)
        }) {
            wish.purchase_hints.push(hint);
        }
    }
    let fill = |slot: &mut Option<String>, value: Option<String>| {
        if slot.as_deref().is_none_or(|v| v.trim().is_empty()) {
            *slot = value.filter(|v| !v.trim().is_empty());
        }
    };
    fill(&mut wish.authors, listing.authors);
    fill(&mut wish.narrators, listing.narrators);
    fill(&mut wish.series, listing.series);
    fill(&mut wish.series_index, listing.series_index);
    fill(&mut wish.cover_url, listing.cover_url);
    fill(&mut wish.subtitle, listing.subtitle);
    fill(&mut wish.description, listing.description);
    fill(&mut wish.publisher, listing.publisher);
    fill(&mut wish.published_at, listing.published_at);
    fill(&mut wish.genres, listing.genres);
    fill(&mut wish.language, listing.language);
    if wish.length_minutes.is_none() {
        wish.length_minutes = listing.length_minutes;
    }
    if wish.expression_id.is_none() {
        wish.expression_id = listing.expression_id;
    }
    absorb_signals(
        wish,
        listing.score,
        listing.reasons,
        listing.categories,
        listing.seed_title,
        listing.seed_categories,
        listing.starter_genre,
    );
}

/// Folds a card for another recording of the same work into `owner`.
///
/// The owner keeps its edition, text and prices: the other recording's
/// cover, narrators, length and price describe something else. What the
/// other card says about the reader's taste (score, reasons, shelf tags,
/// seed) still counts toward the work.
///
/// # Arguments
///
/// * `owner` - The card that stays.
/// * `other` - A card for another recording of the owner's work.
fn fold_other_recording(owner: &mut Recommendation, other: Recommendation) {
    if other.from_request {
        owner.from_request = true;
        if owner.request_uuid.is_none() {
            owner.request_uuid = other.request_uuid;
        }
    }
    for wisher in other.wishers {
        bookclerk_library::push_queue_wisher(&mut owner.wishers, wisher);
    }
    owner.wish_count = owner.wish_count.max(other.wish_count);
    absorb_signals(
        owner,
        other.score,
        other.reasons,
        other.categories,
        other.seed_title,
        other.seed_categories,
        other.starter_genre,
    );
}

/// Adds another card's taste signals for the same work to `card`: the
/// better score, every reason and shelf tag once, and the seed and starter
/// genre when the card has none.
fn absorb_signals(
    card: &mut Recommendation,
    score: f64,
    reasons: Vec<String>,
    categories: Vec<String>,
    seed_title: Option<String>,
    seed_categories: Option<String>,
    starter_genre: Option<String>,
) {
    card.score = card.score.max(score);
    for reason in reasons {
        if !card.reasons.iter().any(|r| r == &reason) {
            card.reasons.push(reason);
        }
    }
    for cat in categories {
        push_shelf_category(&mut card.categories, &cat);
    }
    if card.seed_title.is_none() {
        card.seed_title = seed_title;
    }
    if card.seed_categories.is_none() {
        card.seed_categories = seed_categories;
    }
    if card.starter_genre.is_none() {
        card.starter_genre = starter_genre;
    }
}

/// Adds `rec` to the ranked cards, folding it into the card for its work
/// when there is one (see [`card_relation`]).
///
/// A wish owns its work's card and keeps the edition it names; listings of
/// the same recording add their prices to it, and listings of another
/// recording of the work add only their taste signals. Two store cards of
/// one recording merge as before ([`merge_recommendation`]); of one work in
/// different recordings, the higher-scoring card stays as it is.
fn upsert_recommendation(map: &mut HashMap<String, Recommendation>, rec: Recommendation) {
    let mut found: Option<(String, CardRelation)> = None;
    for (key, existing) in map.iter() {
        match card_relation(&rec, existing) {
            CardRelation::Unrelated => {}
            CardRelation::SameRecording => {
                found = Some((key.clone(), CardRelation::SameRecording));
                break;
            }
            CardRelation::SameWork => {
                if found.is_none() {
                    found = Some((key.clone(), CardRelation::SameWork));
                }
            }
        }
    }
    let Some((old_key, relation)) = found else {
        insert_card(map, rec);
        return;
    };
    let Some(existing) = map.remove(&old_key) else {
        return;
    };
    let (mut owner, other) = if takes_card_from(&rec, &existing) {
        (rec, existing)
    } else {
        (existing, rec)
    };
    match relation {
        CardRelation::SameRecording if owner.from_request && !other.from_request => {
            fold_recording_into_wish(&mut owner, other);
        }
        CardRelation::SameRecording => merge_recommendation(&mut owner, other),
        CardRelation::SameWork | CardRelation::Unrelated => fold_other_recording(&mut owner, other),
    }
    // A wish keeps the key its own entry carries, which clients match on.
    if !owner.from_request || owner.work_key.trim().is_empty() {
        owner.work_key = recommendation_map_key(&owner);
    }
    insert_card(map, owner);
}

/// Inserts `rec` under its map key, qualified with its store and product
/// when another card already holds that key: a key without an ISBN or ASIN
/// names only the title and authors, and two cards are never joined on that.
fn insert_card(map: &mut HashMap<String, Recommendation>, rec: Recommendation) {
    let mut key = recommendation_map_key(&rec);
    if map.contains_key(&key) {
        key = format!(
            "{key}|{}:{}",
            rec.candidate_source.as_deref().unwrap_or("wish"),
            rec.candidate_product_id
                .as_deref()
                .or(rec.work_id.as_deref())
                .unwrap_or(rec.title.as_str())
        );
    }
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
    books: &[TasteTitle],
    weights: &TasteWeights,
    listening: &[ListeningSignal],
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
    books: &[TasteTitle],
    listening: &[ListeningSignal],
    listening_engagement_by_uuid: &HashMap<String, f64>,
) -> HashMap<String, SeriesAffinity> {
    let mut by_series: HashMap<String, SeriesAffinity> = HashMap::new();
    let book_by_uuid: HashMap<&str, &TasteTitle> =
        books.iter().map(|b| (b.work_id.as_str(), b)).collect();

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
        if let Some(w) = listening_engagement_by_uuid.get(&book.work_id) {
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
            .work_id
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
pub fn listening_engagement(row: &ListeningSignal) -> f64 {
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

/// Averages the seed works' stored vectors in the space `opts` names;
/// `None` when similarity is off or none is stored there.
async fn seed_embedding_centroid(
    library: &LibraryStore,
    seed_work_ids: &std::collections::BTreeSet<String>,
    opts: &RecommendOptions,
) -> Result<Option<Vec<f32>>> {
    if !opts.embeddings_enabled || seed_work_ids.is_empty() {
        return Ok(None);
    }
    let ids: Vec<String> = seed_work_ids.iter().cloned().collect();
    let stored = library
        .vectors(
            bookclerk_library::VECTOR_KIND_WORK,
            &opts.vectors.space,
            &ids,
        )
        .await?;
    let mut sum: Option<Vec<f32>> = None;
    let mut count = 0u32;
    // In id order, so the same seeds always sum to the same centroid.
    for id in &ids {
        let Some(bytes) = stored.get(id) else {
            continue;
        };
        let vector = bytes_to_vector(bytes);
        match &mut sum {
            Some(acc) if acc.len() == vector.len() => {
                for (total, x) in acc.iter_mut().zip(&vector) {
                    *total += x;
                }
                count += 1;
            }
            Some(_) => {}
            None => {
                sum = Some(vector);
                count = 1;
            }
        }
    }
    Ok(sum.map(|mut centroid| {
        // A count of seed works, far below f32's exact integers.
        #[allow(clippy::cast_precision_loss)]
        let count = count as f32;
        for x in &mut centroid {
            *x /= count;
        }
        centroid
    }))
}

/// Weight of a liked genre the candidate itself lists.
const OWN_GENRE_WEIGHT: f64 = 0.45;

/// Weight of a liked genre only the seed lists, for a candidate whose store
/// gave no genres of its own. Lower than [`OWN_GENRE_WEIGHT`]: the store
/// found the candidate from the seed, but nothing says it shares the genre.
const SEED_GENRE_WEIGHT: f64 = 0.15;

/// How a candidate's genres move its score.
#[derive(Debug, Default, PartialEq)]
struct GenreMatch {
    /// Points added to the score.
    score: f64,
    /// One `matches liked category (…)` reason per liked genre matched.
    reasons: Vec<String>,
}

/// Scores a candidate's genres against the genres the person likes.
///
/// The candidate's own genres count when its store gave any. Only when it
/// gave none do the seed's genres count, at [`SEED_GENRE_WEIGHT`]: a
/// candidate the store found from a fantasy seed is not fantasy itself (a
/// title search for *The Great Hunt* found a hunting guide). Each liked genre
/// counts once, however often the genre ladders repeat it.
///
/// # Arguments
///
/// * `own` - Genres the candidate's store listed for it.
/// * `seed` - Genres of the taste seed the store was asked about.
/// * `liked` - Lowercased genre → how much the person likes it.
fn genre_match(own: Option<&str>, seed: Option<&str>, liked: &HashMap<String, f64>) -> GenreMatch {
    let own = own.map(split_tokens_display).unwrap_or_default();
    let (genres, weight) = if own.is_empty() {
        (
            seed.map(split_tokens_display).unwrap_or_default(),
            SEED_GENRE_WEIGHT,
        )
    } else {
        (own, OWN_GENRE_WEIGHT)
    };
    let mut seen = HashSet::new();
    let mut out = GenreMatch::default();
    for genre in genres {
        let key = genre.to_lowercase();
        let Some(w) = liked.get(&key) else {
            continue;
        };
        if seen.insert(key) {
            out.score += w * weight;
            out.reasons
                .push(format!("matches liked category ({genre})"));
        }
    }
    out
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

    /// A person who likes fantasy, and epic fantasy more.
    fn likes_fantasy() -> HashMap<String, f64> {
        HashMap::from([("fantasy".to_string(), 2.0), ("epic".to_string(), 1.0)])
    }

    #[test]
    fn a_candidate_is_scored_on_its_own_genres_not_the_seeds() {
        // Found from a fantasy seed, but the store files it under nature.
        let m = genre_match(
            Some("Children's Audiobooks; Animals & Nature"),
            Some("Science Fiction & Fantasy; Fantasy; Epic"),
            &likes_fantasy(),
        );
        assert_eq!(m, GenreMatch::default());
    }

    #[test]
    fn a_liked_genre_counts_once_however_often_the_ladders_repeat_it() {
        let m = genre_match(
            Some("Science Fiction & Fantasy; Fantasy; Epic; Fantasy"),
            None,
            &likes_fantasy(),
        );
        assert!((m.score - (2.0 + 1.0) * OWN_GENRE_WEIGHT).abs() < 1e-9);
        assert_eq!(
            m.reasons,
            vec![
                "matches liked category (Fantasy)".to_string(),
                "matches liked category (Epic)".to_string(),
            ]
        );
    }

    #[test]
    fn the_seeds_genres_count_less_when_the_store_gave_none() {
        let m = genre_match(None, Some("Fantasy"), &likes_fantasy());
        assert!((m.score - 2.0 * SEED_GENRE_WEIGHT).abs() < 1e-9);
        assert_eq!(m.reasons.len(), 1);
        const { assert!(SEED_GENRE_WEIGHT < OWN_GENRE_WEIGHT) };
    }

    /// A store card for one listing of *The Sunlit Man*, in work
    /// `w-sunlit`, recording `expression`.
    fn sunlit_listing(
        source: &str,
        product_id: &str,
        expression: &str,
        score: f64,
    ) -> Recommendation {
        let mut rec = Recommendation {
            work_id: Some("w-sunlit".into()),
            expression_id: Some(expression.into()),
            title: "The Sunlit Man: A Cosmere Novel (GraphicAudio)".into(),
            authors: Some("Brandon Sanderson".into()),
            narrators: Some("A full cast".into()),
            series: Some("Secret Projects".into()),
            series_index: Some("4".into()),
            score,
            cover_url: Some(format!(
                "https://covers.example/{product_id}-longer-cover.jpg"
            )),
            length_minutes: Some(671),
            candidate_source: Some(source.into()),
            candidate_product_id: Some(product_id.into()),
            purchase_hints: vec![PurchaseHint {
                source: source.into(),
                product_id: product_id.into(),
                price_cents: Some(1499),
                price_label: Some("$14.99".into()),
                ..Default::default()
            }],
            categories: vec!["from_store".into(), "author".into()],
            reasons: vec![format!("{source} author")],
            ..Default::default()
        };
        rec.work_key = recommendation_map_key(&rec);
        rec
    }

    /// The wish for the Audible edition of *The Sunlit Man*.
    fn sunlit_wish() -> Recommendation {
        Recommendation {
            work_id: Some("w-sunlit".into()),
            expression_id: Some("x-demeritt".into()),
            title: "The Sunlit Man".into(),
            authors: Some("Brandon Sanderson".into()),
            narrators: Some("William DeMeritt".into()),
            series: Some("The Cosmere".into()),
            series_index: Some("4".into()),
            asin: Some("B0D1KV49FL".into()),
            isbn: Some("9781938570452".into()),
            cover_url: Some("https://covers.example/wished.jpg".into()),
            length_minutes: Some(671),
            score: 40.0,
            from_request: true,
            work_key: "asin:B0D1KV49FL".into(),
            categories: vec!["requests".into()],
            store_editions: vec![StoreEdition::new("audible", "B0D1KV49FL")],
            ..Default::default()
        }
    }

    /// The only card in `map`.
    fn only_card(map: HashMap<String, Recommendation>) -> Recommendation {
        assert_eq!(map.len(), 1, "{:?}", map.keys());
        map.into_values().next().unwrap_or_default()
    }

    /// A store listing of another recording that was ranked first does not
    /// replace the wished edition: the card keeps the wish's title, cover,
    /// series and narrator, and takes neither the other recording's price
    /// nor its store edition.
    #[test]
    fn a_wish_keeps_its_edition_over_another_recording() {
        let mut map = HashMap::new();
        upsert_recommendation(
            &mut map,
            sunlit_listing("chirp", "ga-1", "x-full-cast", 3.0),
        );
        upsert_recommendation(&mut map, sunlit_wish());
        let card = only_card(map);
        assert_eq!(card.title, "The Sunlit Man");
        assert_eq!(card.series.as_deref(), Some("The Cosmere"));
        assert_eq!(card.narrators.as_deref(), Some("William DeMeritt"));
        assert_eq!(
            card.cover_url.as_deref(),
            Some("https://covers.example/wished.jpg")
        );
        assert_eq!(card.asin.as_deref(), Some("B0D1KV49FL"));
        assert_eq!(card.work_key, "asin:B0D1KV49FL");
        assert!(card.purchase_hints.is_empty(), "{:?}", card.purchase_hints);
        assert_eq!(
            card.store_editions,
            vec![StoreEdition::new("audible", "B0D1KV49FL")]
        );
        assert!(card.from_request);
        // The other listing's taste signals still count for the work.
        assert!(card.categories.iter().any(|c| c == "author"));
        assert!(card.reasons.iter().any(|r| r == "chirp author"));
    }

    /// Another store's listing of the wished recording adds its price and
    /// store edition, and fills only what the wish lacks.
    #[test]
    fn a_wish_takes_prices_from_its_own_recording() {
        let mut map = HashMap::new();
        let mut libro = sunlit_listing("libro", "li-1", "x-demeritt", 3.0);
        libro.publisher = Some("Dragonsteel".into());
        upsert_recommendation(&mut map, sunlit_wish());
        upsert_recommendation(&mut map, libro);
        let card = only_card(map);
        assert_eq!(card.title, "The Sunlit Man");
        assert_eq!(card.series.as_deref(), Some("The Cosmere"));
        assert_eq!(
            card.cover_url.as_deref(),
            Some("https://covers.example/wished.jpg")
        );
        assert_eq!(card.publisher.as_deref(), Some("Dragonsteel"));
        assert_eq!(card.purchase_hints.len(), 1);
        assert_eq!(card.purchase_hints[0].source, "libro");
        assert!(card
            .store_editions
            .contains(&StoreEdition::new("libro", "li-1")));
    }

    /// Two store cards for different recordings of one work are one card:
    /// the higher-scoring one, with only its own price.
    #[test]
    fn one_card_per_work_keeps_the_owning_recordings_price() {
        let mut map = HashMap::new();
        upsert_recommendation(
            &mut map,
            sunlit_listing("chirp", "ga-1", "x-full-cast", 2.0),
        );
        upsert_recommendation(
            &mut map,
            sunlit_listing("audible", "B0D1KV49FL", "x-demeritt", 5.0),
        );
        let card = only_card(map);
        assert_eq!(card.candidate_source.as_deref(), Some("audible"));
        assert_eq!(card.purchase_hints.len(), 1);
        assert_eq!(card.purchase_hints[0].source, "audible");
        assert!(!card.store_editions.iter().any(|e| e.source == "chirp"));
        assert!((card.score - 5.0).abs() < f64::EPSILON);
    }

    /// Cards the data model has not placed are never joined on their title
    /// and author.
    #[test]
    fn unplaced_cards_with_one_title_stay_apart() {
        let mut map = HashMap::new();
        for (source, product) in [("chirp", "ga-1"), ("libro", "li-1")] {
            let mut rec = sunlit_listing(source, product, "", 1.0);
            rec.work_id = None;
            rec.expression_id = None;
            upsert_recommendation(&mut map, rec);
        }
        assert_eq!(map.len(), 2, "{:?}", map.keys());
    }

    /// A ranked queue entry names the library work its wishes resolve to, so
    /// a client can match its own wish whatever key that wish was stored
    /// under.
    #[test]
    fn a_ranked_entry_names_its_work() {
        let entry = GlobalQueueEntry {
            edition_id: None,
            expression_id: None,
            work_key: "asin:B002V1OF70".into(),
            work_id: "work-dune".into(),
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
        let ranked = RankedQueueEntry::from_global(entry, 0.0, Vec::new());
        let json = serde_json::to_value(&ranked).unwrap();
        assert_eq!(json["work_id"], "work-dune");
        assert_eq!(json["work_key"], "asin:B002V1OF70");
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
                is_explicit: None,
                store_rank: None,
                work_id: None,
                expression_id: None,
                publisher_votes: Vec::new(),
            };
        let opts = RecommendOptions {
            include_purchase_hints: false,
            embeddings_enabled: false,
            ..RecommendOptions::default()
        };
        let recs = rank_candidates(
            &store,
            &SourceRegistry::new(),
            &reader_languages(&books),
            build_taste_scores(&books, &TasteWeights::new(), &[]),
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

    /// Candidates in a language the reader does not listen in leave the
    /// ranking; those in the reader's language, or of unknown language,
    /// stay. With no known language in the taste, English is assumed.
    #[tokio::test]
    async fn candidates_in_other_languages_are_dropped() {
        let (store, mut books) = rated_library().await;
        let candidate = |product_id: &str, language: Option<&str>| {
            crate::candidates::hit_to_candidate(
                "chirp",
                bookclerk_source::CatalogHit {
                    product_id: product_id.into(),
                    title: format!("Title {product_id}"),
                    language: language.map(str::to_owned),
                    ..Default::default()
                },
            )
        };
        let pool = || {
            vec![
                candidate("english", Some("English")),
                candidate("spanish", Some("Spanish")),
                candidate("unknown", None),
                candidate("unparsed", Some("Klingon dialect")),
            ]
        };
        let opts = RecommendOptions {
            include_purchase_hints: false,
            embeddings_enabled: false,
            ..RecommendOptions::default()
        };
        let ranked = |books: Vec<TasteTitle>| {
            let store = &store;
            let opts = &opts;
            async move {
                let mut ids: Vec<String> = rank_candidates(
                    store,
                    &SourceRegistry::new(),
                    &reader_languages(&books),
                    build_taste_scores(&books, &TasteWeights::new(), &[]),
                    pool(),
                    opts,
                )
                .await
                .unwrap()
                .into_iter()
                .filter_map(|r| r.candidate_product_id)
                .collect();
                ids.sort();
                ids
            }
        };
        assert_eq!(
            ranked(books.clone()).await,
            ["english", "unknown", "unparsed"],
            "no known language in the taste means English"
        );
        books[0].language = Some("es".into());
        assert_eq!(
            ranked(books.clone()).await,
            ["spanish", "unknown", "unparsed"]
        );
        let mut english = books[0].clone();
        english.language = Some("english".into());
        books.push(english);
        assert_eq!(
            ranked(books).await,
            ["english", "spanish", "unknown", "unparsed"]
        );
    }

    /// A store holding one unfinished book rated 5 by "Ada Author", and the
    /// household's titles.
    async fn rated_library() -> (LibraryStore, Vec<TasteTitle>) {
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
        let ada = store
            .create_user(bookclerk_library::UserRole::Owner, Some("Ada"), None)
            .await
            .unwrap();
        let work = store
            .work_of_holding(&book.holding_id)
            .await
            .unwrap()
            .unwrap();
        store
            .set_user_title_state(
                ada.id,
                &work,
                &bookclerk_library::UserBookFields {
                    rating_overall: Some(5.0),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let books = household_taste(&store).await.unwrap();
        (store, books)
    }

    /// Another store's edition of an owned book is dropped from the
    /// candidates by its work, though it shares no id with the library; a
    /// sequel and an unknown title stay, and so does everything for a
    /// library with no books.
    #[tokio::test]
    async fn another_edition_of_an_owned_book_is_not_a_candidate() {
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let candidate = |source: &str, product: &str, title: &str| {
            let mut c = crate::candidates::hit_to_candidate(
                source,
                bookclerk_source::CatalogHit {
                    product_id: product.into(),
                    title: title.into(),
                    work_kind: bookclerk_source::records::WorkKind::Book,
                    expression_kind: bookclerk_source::records::ExpressionKind::Narration,
                    media_kind: bookclerk_source::records::MediaKind::Audio,
                    ..bookclerk_source::CatalogHit::default()
                },
            );
            c.authors = Some("Frank Herbert".into());
            c
        };
        let mut chirp_dune = candidate("chirp", "chirp-dune", "Dune");
        chirp_dune.isbn = Some("9781427201447".into());
        let mut audible_dune = candidate("audible", "B0OTHER001", "Dune");
        audible_dune.asin = Some("B0OTHER001".into());
        let pool = || {
            vec![
                chirp_dune.clone(),
                candidate("libro", "messiah", "Dune Messiah"),
                audible_dune.clone(),
                candidate("libro", "unknown", "The Dosadi Experiment"),
            ]
        };
        let titles = |kept: &[crate::candidates::StorefrontCandidate]| {
            kept.iter()
                .map(|c| c.product_id.clone())
                .collect::<Vec<_>>()
        };

        let empty = retain_unowned_works(&store, pool()).await.unwrap();
        assert_eq!(
            titles(&empty).len(),
            4,
            "a new library keeps every candidate"
        );

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

        let kept = retain_unowned_works(&store, pool()).await.unwrap();
        assert_eq!(titles(&kept), ["messiah", "unknown"]);

        // A pooled candidate's own work decides, whatever its text says.
        let held_work = store
            .work_for(&bookclerk_library::TitleQuery {
                title: "Dune",
                authors: Some("Frank Herbert"),
                asin: Some("B002V1OF70"),
                isbn: None,
                series: None,
                series_index: None,
            })
            .await
            .unwrap()
            .unwrap();
        let mut renamed = candidate("libro", "renamed", "A Desert Planet Saga");
        renamed.work_id = Some(held_work);
        let mut other = candidate("libro", "other-work", "Dune");
        other.work_id = Some(String::from("work-not-held"));
        let kept = retain_unowned_works(&store, vec![renamed, other])
            .await
            .unwrap();
        assert_eq!(titles(&kept), ["other-work"]);

        // No candidates, or one with no title to place, pass through.
        assert!(retain_unowned_works(&store, Vec::new())
            .await
            .unwrap()
            .is_empty());
        let untitled = candidate("chirp", "untitled", "  ");
        let kept = retain_unowned_works(&store, vec![untitled]).await.unwrap();
        assert_eq!(titles(&kept), ["untitled"]);
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
        // Each wish names the work it resolves to: Dune's own, or a catalog
        // work for a book the library does not hold.
        let work = |title: &'static str, isbn: Option<&'static str>| {
            let store = &store;
            async move {
                store
                    .work_for_wish(&bookclerk_library::TitleQuery {
                        title,
                        authors: Some("Frank Herbert"),
                        asin: None,
                        isbn,
                        series: None,
                        series_index: None,
                    })
                    .await
                    .unwrap()
                    .0
            }
        };
        let entry = |title: &str, work_id: String| GlobalQueueEntry {
            edition_id: None,
            expression_id: None,
            work_key: String::new(),
            work_id,
            title: title.into(),
            authors: Some("Frank Herbert".into()),
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
            wish_count: 1,
            wishers: Vec::new(),
            sample_uuids: Vec::new(),
            first_requested_at: chrono::Utc::now(),
            last_requested_at: chrono::Utc::now(),
        };
        let dune = entry("Dune", work("Dune", None).await);
        assert!(library_owns_work(&store, &dune).await.unwrap());
        let messiah = entry(
            "Dune Messiah",
            work("Dune Messiah", Some("9781427202376")).await,
        );
        assert!(!library_owns_work(&store, &messiah).await.unwrap());
        let children = entry("Children of Dune", work("Children of Dune", None).await);
        assert!(!library_owns_work(&store, &children).await.unwrap());
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
        let row = |work_id: &str| crate::listening::ListeningSignal {
            identity_id: None,
            provider: String::from("abs"),
            external_user_id: String::from("u1"),
            work_id: Some(work_id.to_owned()),
            authors: Some(String::from("Ann Author")),
            progress: Some(0.5),
            current_time_seconds: Some(3_600.0),
            duration_seconds: Some(7_200.0),
            is_finished: false,
            last_listened_at: None,
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
        use crate::listening::ListeningSignal;

        fn row(
            progress: Option<f64>,
            current_time_seconds: Option<f64>,
            duration_seconds: Option<f64>,
            is_finished: bool,
        ) -> ListeningSignal {
            ListeningSignal {
                identity_id: None,
                provider: String::from("abs"),
                external_user_id: String::from("u1"),
                work_id: None,
                authors: None,
                progress,
                current_time_seconds,
                duration_seconds,
                is_finished,
                last_listened_at: None,
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
