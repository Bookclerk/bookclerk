//! Public bibliographic metadata for title detail dialogs.
//!
//! Prefer a metadata-provider record (the Audnexus plugin) when an ASIN is
//! known; otherwise try a confident Audible catalog match from
//! title/author/ISBN. When those miss or stay sparse (common for storefront
//! search hits), fill gaps from the stores' own catalog details: first the
//! stores that listed the title and declared `detailByProductId`, by the
//! product ids they gave it, then the stores that declared `detailByIsbn`, by
//! the title's ISBN. Used by Discover, Wishlist, and Library detail views
//! (commerce links stay on a separate purchase-hints path).
//!
//! Community ratings come from a separate Audible catalog `rating` request so
//! bulk Discover expand stays lean. Written reviews are loaded on demand via
//! [`resolve_title_reviews`] (paginated) so title-meta / shelf expand stay light.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bookclerk_enrich::{
    enrichment_for_asin, fetch_audible_catalog_rating, fetch_audible_catalog_reviews_page,
    lookup_by_metadata, public_http_client, CatalogReview, CatalogReviewsSort, Enrichment,
    MatchQuery, MetadataProviders, DEFAULT_ENRICH_MIN_CONFIDENCE,
};
use bookclerk_source::{CatalogHit, ContentSource, SourceRegistry};

use crate::error::Result;
use crate::identity::StoreEdition;
use crate::ttl_cache::{cache_key, TtlCache};

/// Process-wide TTL cache of resolved title-meta payloads (6 hours, 512 entries).
fn title_meta_cache() -> &'static TtlCache<Option<TitleMeta>> {
    static CACHE: OnceLock<TtlCache<Option<TitleMeta>>> = OnceLock::new();
    // Bump key prefix below when the payload shape changes.
    CACHE.get_or_init(|| TtlCache::new(Duration::from_secs(6 * 60 * 60), 512))
}

/// Process-wide TTL cache of paginated Audible review pages (30 minutes, 256 entries).
fn title_reviews_cache() -> &'static TtlCache<TitleReviewsPage> {
    static CACHE: OnceLock<TtlCache<TitleReviewsPage>> = OnceLock::new();
    CACHE.get_or_init(|| TtlCache::new(Duration::from_secs(30 * 60), 256))
}

/// Query for public title metadata (Audnexus / Audible catalog).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TitleMetaQuery {
    /// Display title as shown on the storefront or library card.
    #[serde(default)]
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
    /// Comma-separated narrator names when known.
    #[schema(example = "George Guidall")]
    pub narrators: Option<String>,
    /// Audiobook runtime in whole minutes when known.
    #[schema(example = 560)]
    pub length_minutes: Option<i64>,
    /// Marketplace / region code (`us`, `uk`, …) for catalog lookups.
    #[serde(default = "default_region")]
    #[schema(example = "us")]
    pub region: String,
    /// Store editions that listed this title (the search hit's or card's
    /// `store_editions`), at most 16. A sparse record is filled from each of
    /// these stores that declared `detailByProductId`, by the product id it
    /// gave the title, before the stores that declared `detailByIsbn` are
    /// asked by ISBN.
    #[serde(default)]
    pub store_editions: Vec<StoreEdition>,
}

/// Most store editions one title-meta query may name; the API rejects more
/// (keep [`TitleMetaQuery::store_editions`]'s doc in step).
pub const TITLE_META_MAX_STORE_EDITIONS: usize = 16;

/// Most storefront catalog-detail lookups one title-meta fill makes, so a
/// query naming many editions cannot fan out across every guest.
const STORE_FILL_MAX_LOOKUPS: usize = 3;

/// Most of [`STORE_FILL_MAX_LOOKUPS`] the listing stores' product lookups
/// may take, so the ISBN stores always keep a turn.
const STORE_FILL_MAX_PRODUCT_LOOKUPS: usize = 2;

/// Wall-clock budget for one title-meta fill's storefront lookups together,
/// so a slow guest cannot stall a dialog or a batch.
const STORE_FILL_TIMEOUT: Duration = Duration::from_millis(3500);

/// Marketplace default (`us`) when a title-meta or reviews query omits `region`.
fn default_region() -> String {
    String::from("us")
}

/// Bibliographic fields suitable for an Audible-style detail panel.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TitleMeta {
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    #[schema(example = "B002V0QK4C")]
    pub asin: Option<String>,
    /// Display title as shown on the storefront or library card.
    #[schema(example = "The Left Hand of Darkness")]
    pub title: Option<String>,
    /// Optional subtitle when the catalog distinguishes it from the title.
    #[schema(example = "A Hainish Novel")]
    pub subtitle: Option<String>,
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
    /// Publisher or storefront synopsis; may be truncated for embeddings.
    #[schema(
        example = "An envoy to the planet Gethen must win the trust of a people who have no fixed gender."
    )]
    pub description: Option<String>,
    /// Publisher imprint as reported by the catalog.
    #[schema(example = "Blackstone Publishing")]
    pub publisher: Option<String>,
    /// Audiobook runtime in whole minutes when known.
    #[schema(example = 560)]
    pub length_minutes: Option<i64>,
    /// Publication date string from the catalog (ISO or storefront format).
    #[schema(example = "2005-06-21")]
    pub published_at: Option<String>,
    /// Genre / subject tags as a single delimited string when known.
    #[schema(example = "Science Fiction & Fantasy; Science Fiction")]
    pub categories: Option<String>,
    /// BCP-47 / storefront language code (`en`, `de`, …) when known.
    #[schema(example = "en")]
    pub language: Option<String>,
    /// `Some(true)` abridged / `Some(false)` unabridged when the storefront said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = false)]
    pub is_abridged: Option<bool>,
    /// Audible community overall rating (0–5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 4.5)]
    pub rating_overall: Option<f64>,
    /// Narration / performance sub-score when the storefront splits ratings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 4.7)]
    pub rating_performance: Option<f64>,
    /// Story / content sub-score when the storefront splits ratings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 4.3)]
    pub rating_story: Option<f64>,
    /// Count of star ratings (Audible `num_ratings`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 12840)]
    pub rating_count: Option<i64>,
    /// Count of written reviews (Audible `num_reviews`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 1932)]
    pub review_count: Option<i64>,
    /// Deprecated: reviews load via `/api/discover/title-reviews`. Kept empty
    /// for older clients; prefer the paginated reviews endpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<TitleReview>,
}

/// Query for a page of Audible customer reviews.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TitleReviewsQuery {
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    #[schema(example = "B002V0QK4C")]
    pub asin: String,
    /// Marketplace / region code (`us`, `uk`, …) for catalog lookups.
    #[serde(default = "default_region")]
    #[schema(example = "us")]
    pub region: String,
    /// 1-based page index.
    #[serde(default = "default_reviews_page")]
    #[schema(example = 1)]
    pub page: u32,
    /// Page size (clamped server-side to 1..=20).
    #[serde(default = "default_reviews_page_size")]
    #[schema(example = 10)]
    pub page_size: u32,
    /// `MostHelpful` (default) or `MostRecent`.
    #[serde(default = "default_reviews_sort")]
    #[schema(example = "MostHelpful")]
    pub sort_by: String,
}

/// First reviews page (1-based) when the client omits `page`.
fn default_reviews_page() -> u32 {
    1
}

/// Default reviews page size (5); the handler still clamps to 1..=20.
fn default_reviews_page_size() -> u32 {
    5
}

/// Default Audible reviews sort (`MostHelpful`) when the client omits `sort_by`.
fn default_reviews_sort() -> String {
    String::from("MostHelpful")
}

/// One page of title reviews for infinite scroll.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TitleReviewsPage {
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    #[schema(example = "B002V0QK4C")]
    pub asin: String,
    /// 1-based page index for this reviews / list request.
    #[schema(example = 1)]
    pub page: u32,
    /// Maximum number of items returned in this page.
    #[schema(example = 10)]
    pub page_size: u32,
    /// When true, another page is available by incrementing [`Self::page`].
    #[schema(example = true)]
    pub has_more: bool,
    /// Remote sort key for reviews (`MostHelpful` or `MostRecent`, as applied).
    #[schema(example = "MostHelpful")]
    pub sort_by: String,
    /// Review rows on this page.
    pub reviews: Vec<TitleReview>,
}

/// One Audible customer review for the title detail panel.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct TitleReview {
    /// Stable identifier for this item.
    #[schema(example = "R3K2M9P1Q7X4Z")]
    pub id: Option<String>,
    /// Display title as shown on the storefront or library card.
    #[schema(example = "Still the best")]
    pub title: Option<String>,
    /// Full review text from the storefront.
    #[schema(example = "Guidall's narration carries the quiet of Winter; I listened twice.")]
    pub body: String,
    /// Display name of the review author on the storefront.
    #[schema(example = "Dana")]
    pub author_name: Option<String>,
    /// Overall star rating for this review when present.
    #[schema(example = 5)]
    pub overall_rating: Option<i64>,
    /// Narration sub-rating for this review when present.
    #[schema(example = 5)]
    pub performance_rating: Option<i64>,
    /// Story sub-rating for this review when present.
    #[schema(example = 4)]
    pub story_rating: Option<i64>,
    /// When the review was submitted (storefront timestamp string).
    #[schema(example = "2026-02-11")]
    pub submitted_at: Option<String>,
}

impl From<CatalogReview> for TitleReview {
    fn from(r: CatalogReview) -> Self {
        Self {
            id: r.id,
            title: r.title,
            body: r.body,
            author_name: r.author_name,
            overall_rating: r.overall_rating,
            performance_rating: r.performance_rating,
            story_rating: r.story_rating,
            submitted_at: r.submitted_at,
        }
    }
}

impl TitleMeta {
    /// Copies Audnexus / catalog enrichment into a detail-panel payload (ratings stay unset).
    fn from_enrichment(e: Enrichment) -> Self {
        Self {
            cover_path: None,
            asin: Some(e.asin).filter(|s| !s.is_empty()),
            title: Some(e.title).filter(|s| !s.is_empty()),
            subtitle: e.subtitle,
            authors: e.authors,
            narrators: e.narrators,
            series: e.series,
            series_index: e.series_index,
            isbn: e.isbn,
            cover_url: e.cover_url,
            // Keep store HTML; the UI sanitizes to a safe tag subset for render.
            description: e.description,
            publisher: e.publisher,
            length_minutes: e.length_minutes,
            published_at: e.published_at,
            categories: e.categories,
            language: e.language,
            is_abridged: None,
            rating_overall: None,
            rating_performance: None,
            rating_story: None,
            rating_count: None,
            review_count: None,
            reviews: Vec::new(),
        }
    }

    /// Overlays Audible community rating counts onto an already-built title-meta payload.
    fn with_ratings(mut self, rating: bookclerk_enrich::CatalogRating) -> Self {
        self.rating_overall = rating.overall;
        self.rating_performance = rating.performance;
        self.rating_story = rating.story;
        self.rating_count = rating.num_ratings;
        self.review_count = rating.num_reviews;
        self
    }

    /// Copies a storefront [`CatalogHit`] (Libro HTML/JSON-LD) into a detail-panel payload.
    fn from_catalog_hit(hit: CatalogHit) -> Self {
        let (authors, narrators) = (hit.authors(), hit.narrators());
        let (series, series_index) = (hit.series(), hit.series_index());
        let (asin, isbn) = (hit.asin(), hit.isbn());
        Self {
            cover_path: None,
            asin: asin.filter(|s| !s.is_empty()),
            title: Some(hit.title).filter(|s| !s.is_empty()),
            subtitle: hit.subtitle,
            authors,
            narrators,
            series,
            series_index,
            isbn,
            cover_url: hit.cover_url,
            description: hit.description,
            publisher: hit.publisher,
            length_minutes: hit.length_minutes,
            published_at: hit.published_at,
            categories: hit.categories,
            language: hit.language,
            is_abridged: hit.is_abridged,
            rating_overall: hit.rating_overall,
            rating_performance: None,
            rating_story: None,
            rating_count: hit.rating_count,
            review_count: None,
            reviews: Vec::new(),
        }
    }
}

/// True when the option holds a non-empty trimmed string.
fn non_empty(s: Option<&String>) -> bool {
    s.map(|v| !v.trim().is_empty()).unwrap_or(false)
}

/// True when Audible/Audnexus left gaps a storefront detail fetch can fill.
fn title_meta_needs_store_fill(meta: Option<&TitleMeta>) -> bool {
    match meta {
        None => true,
        Some(m) => {
            !non_empty(m.description.as_ref())
                || !non_empty(m.narrators.as_ref())
                || m.length_minutes.is_none()
                || !non_empty(m.categories.as_ref())
                || m.is_abridged.is_none()
        }
    }
}

/// Fills missing bibliographic fields from `fill` without overwriting set values.
fn merge_title_meta(base: Option<TitleMeta>, fill: TitleMeta) -> TitleMeta {
    let Some(mut b) = base else {
        return fill;
    };
    if !non_empty(b.title.as_ref()) {
        b.title = fill.title;
    }
    if !non_empty(b.subtitle.as_ref()) {
        b.subtitle = fill.subtitle;
    }
    if !non_empty(b.authors.as_ref()) {
        b.authors = fill.authors;
    }
    if !non_empty(b.narrators.as_ref()) {
        b.narrators = fill.narrators;
    }
    if !non_empty(b.series.as_ref()) {
        b.series = fill.series;
    }
    if !non_empty(b.series_index.as_ref()) {
        b.series_index = fill.series_index;
    }
    if !non_empty(b.isbn.as_ref()) {
        b.isbn = fill.isbn;
    }
    if !non_empty(b.cover_url.as_ref()) {
        b.cover_url = fill.cover_url;
    }
    if !non_empty(b.description.as_ref()) {
        b.description = fill.description;
    }
    if !non_empty(b.publisher.as_ref()) {
        b.publisher = fill.publisher;
    }
    if b.length_minutes.is_none() {
        b.length_minutes = fill.length_minutes;
    }
    if !non_empty(b.published_at.as_ref()) {
        b.published_at = fill.published_at;
    }
    if !non_empty(b.categories.as_ref()) {
        b.categories = fill.categories;
    }
    if !non_empty(b.language.as_ref()) {
        b.language = fill.language;
    }
    if b.is_abridged.is_none() {
        b.is_abridged = fill.is_abridged;
    }
    if b.rating_overall.is_none() {
        b.rating_overall = fill.rating_overall;
    }
    if b.rating_count.is_none() {
        b.rating_count = fill.rating_count;
    }
    b
}

/// True for compact ISBN-10 (digits plus optional trailing X) or ISBN-13 digit strings.
fn isbn_like(raw: &str) -> bool {
    let compact: String = raw.chars().filter(|c| *c != '-').collect();
    let b = compact.as_bytes();
    match b.len() {
        13 => b.iter().all(|c| c.is_ascii_digit()),
        10 => {
            b[..9].iter().all(|c| c.is_ascii_digit())
                && (b[9].is_ascii_digit() || b[9].eq_ignore_ascii_case(&b'X'))
        }
        _ => false,
    }
}

/// One storefront catalog-detail lookup a title-meta fill can make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FillLookup {
    /// The store's own product id for an edition it listed
    /// (`detailByProductId`).
    Product(String),
    /// The title's ISBN (`detailByIsbn`).
    Isbn(String),
}

impl FillLookup {
    /// What kind of lookup this is (`product` or `isbn`), for logs that
    /// must not name the title.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Product(_) => "product",
            Self::Isbn(_) => "isbn",
        }
    }

    /// The id this lookup asks the store for (a product id or an ISBN).
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Product(id) | Self::Isbn(id) => id,
        }
    }
}

/// What a store declared about catalog-detail lookups, as a title-meta fill
/// plan reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillStoreLookups {
    /// The store's plugin key; two lookups for the same key and id are one.
    pub plugin_key: String,
    /// The store declared `detailByProductId`.
    pub by_product_id: bool,
    /// The store declared `detailByIsbn`.
    pub by_isbn: bool,
}

/// Why a title-meta query was refused before any lookup ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TitleMetaQueryProblem {
    /// The query named more than [`TITLE_META_MAX_STORE_EDITIONS`] store
    /// editions.
    TooManyStoreEditions,
    /// The query's title, ASIN and ISBN are all blank.
    NothingToLookUp,
}

impl std::fmt::Display for TitleMetaQueryProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyStoreEditions => write!(
                f,
                "at most {TITLE_META_MAX_STORE_EDITIONS} store editions per title-meta query"
            ),
            Self::NothingToLookUp => f.write_str("title, asin, or isbn is required"),
        }
    }
}

/// Checks a title-meta query from a client before it is resolved.
///
/// # Errors
///
/// Returns [`TitleMetaQueryProblem::TooManyStoreEditions`] when the query
/// names more than [`TITLE_META_MAX_STORE_EDITIONS`] editions, and
/// [`TitleMetaQueryProblem::NothingToLookUp`] when its title, ASIN and ISBN
/// are all blank.
pub fn check_title_meta_query(
    query: &TitleMetaQuery,
) -> std::result::Result<(), TitleMetaQueryProblem> {
    if query.store_editions.len() > TITLE_META_MAX_STORE_EDITIONS {
        return Err(TitleMetaQueryProblem::TooManyStoreEditions);
    }
    let blank = |v: Option<&str>| v.unwrap_or("").trim().is_empty();
    if query.title.trim().is_empty() && blank(query.asin.as_deref()) && blank(query.isbn.as_deref())
    {
        return Err(TitleMetaQueryProblem::NothingToLookUp);
    }
    Ok(())
}

/// Plans the catalog-detail lookups that can fill `query`, in the order to
/// try them.
///
/// Each listing store that declared `detailByProductId` comes first, by its
/// own product id (in the query's edition order, at most two), then each
/// store in `isbn_stores` that declared `detailByIsbn`, by the title's ISBN
/// (or an ISBN-shaped ASIN). A store already asked for the same id is
/// skipped, and the plan holds at most three lookups. Reads only declared
/// capabilities, so no guest is woken to plan.
///
/// # Arguments
///
/// * `query` - The title-meta query, already checked.
/// * `resolve` - Finds the store an edition names by its source id.
/// * `isbn_stores` - The stores that may be asked by ISBN, in order.
/// * `lookups` - What a store declared.
///
/// # Returns
///
/// The stores to ask, each with the lookup to make.
pub fn plan_title_meta_fill<S>(
    query: &TitleMetaQuery,
    resolve: impl Fn(&str) -> Option<S>,
    isbn_stores: impl IntoIterator<Item = S>,
    lookups: impl Fn(&S) -> FillStoreLookups,
) -> Vec<(S, FillLookup)> {
    let mut plan: Vec<(S, FillLookup)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for edition in &query.store_editions {
        let product_id = edition.product_id.trim();
        if product_id.is_empty() {
            continue;
        }
        let Some(store) = resolve(edition.source.trim()) else {
            continue;
        };
        let declared = lookups(&store);
        if !declared.by_product_id || !seen.insert((declared.plugin_key, product_id.to_string())) {
            continue;
        }
        plan.push((store, FillLookup::Product(product_id.to_string())));
        if plan.len() == STORE_FILL_MAX_PRODUCT_LOOKUPS {
            break;
        }
    }
    let isbn = query
        .isbn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            query
                .asin
                .as_deref()
                .map(str::trim)
                .filter(|s| isbn_like(s))
        });
    if let Some(isbn) = isbn {
        for store in isbn_stores {
            if plan.len() >= STORE_FILL_MAX_LOOKUPS {
                break;
            }
            let declared = lookups(&store);
            // A store whose product ids are ISBNs was just asked this one.
            if declared.by_isbn && seen.insert((declared.plugin_key, isbn.to_string())) {
                plan.push((store, FillLookup::Isbn(isbn.to_string())));
            }
        }
    }
    plan.truncate(STORE_FILL_MAX_LOOKUPS);
    plan
}

/// The title-meta fill plan over the registry's stores (see
/// [`plan_title_meta_fill`]).
fn store_fill_plan(
    query: &TitleMetaQuery,
    sources: &SourceRegistry,
) -> Vec<(Arc<dyn ContentSource>, FillLookup)> {
    plan_title_meta_fill(
        query,
        |id| sources.get(id),
        sources.routed(),
        |store| FillStoreLookups {
            plugin_key: store.plugin_key().to_string(),
            by_product_id: store.answers_catalog_detail_by_product_id(),
            by_isbn: store.answers_catalog_detail_by_isbn(),
        },
    )
}

/// Gap-fills a sparse result from the storefronts' catalog details, in the
/// order [`store_fill_plan`] gives; the first record found wins, a store
/// that fails is skipped, and the lookups together stop after
/// [`STORE_FILL_TIMEOUT`].
async fn fill_from_source_catalog(
    query: &TitleMetaQuery,
    sources: Option<&SourceRegistry>,
) -> Option<TitleMeta> {
    let plan = store_fill_plan(query, sources?);
    let planned: Vec<String> = plan
        .iter()
        .map(|(store, lookup)| format!("{}:{}", store.id(), lookup.kind()))
        .collect();
    // Which lookup of the plan was running when time ran out, so a timeout
    // names the slow store; the title itself is redacted from uploads.
    let step = std::sync::atomic::AtomicUsize::new(0);
    match tokio::time::timeout(STORE_FILL_TIMEOUT, fill_in_order(plan, &step)).await {
        Ok(found) => found,
        Err(_) => {
            let at = step.load(std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(
                title = %query.title,
                waiting_on = planned.get(at).map_or("", String::as_str),
                plan = %planned.join(","),
                "title-meta store fill timed out"
            );
            None
        }
    }
}

/// Runs a fill plan in order and returns the first record found.
async fn fill_in_order(
    plan: Vec<(Arc<dyn ContentSource>, FillLookup)>,
    step: &std::sync::atomic::AtomicUsize,
) -> Option<TitleMeta> {
    for (index, (store, lookup)) in plan.into_iter().enumerate() {
        step.store(index, std::sync::atomic::Ordering::Relaxed);
        let (result, key) = match &lookup {
            FillLookup::Product(product_id) => {
                (store.catalog_detail(product_id).await, product_id.as_str())
            }
            FillLookup::Isbn(isbn) => (store.catalog_detail_by_isbn(isbn).await, isbn.as_str()),
        };
        match result {
            Ok(Some(hit)) => return Some(TitleMeta::from_catalog_hit(hit)),
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(
                    source = %store.id(),
                    key,
                    error = %err,
                    "title-meta catalog_detail failed"
                );
            }
        }
    }
    None
}

/// Resolve public metadata for a title detail dialog.
///
/// When `sources` is provided, sparse Audible/Audnexus results are gap-filled
/// from storefront [`bookclerk_source::ContentSource::catalog_detail`]: the
/// listing stores that declared `detailByProductId`, then the stores that
/// declared `detailByIsbn`.
///
/// # Arguments
///
/// * `query` - Query vector or free-text search string.
/// * `providers` - Deployed metadata providers asked for records.
/// * `sources` - String `sources` for this call.
///
/// # Returns
///
/// On success, the inner `Option<TitleMeta>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn resolve_title_meta(
    query: &TitleMetaQuery,
    providers: Arc<dyn MetadataProviders>,
    sources: Option<&SourceRegistry>,
) -> Result<Option<TitleMeta>> {
    let region = query.region.trim();
    let region = if region.is_empty() { "us" } else { region };
    let editions: Vec<String> = query
        .store_editions
        .iter()
        .map(|ed| format!("{}:{}", ed.source.trim(), ed.product_id.trim()))
        .collect();
    let editions = editions.join(",");
    let key = cache_key(&[
        "title-meta-v10",
        region,
        query.asin.as_deref().unwrap_or(""),
        query.isbn.as_deref().unwrap_or(""),
        query.title.as_str(),
        query.authors.as_deref().unwrap_or(""),
        editions.as_str(),
    ]);
    if let Some(cached) = title_meta_cache().get(&key) {
        return Ok(cached);
    }

    let resolved =
        match resolve_title_meta_uncached(query, providers.as_ref(), region, sources).await {
            Ok(meta) => meta,
            Err(err) => {
                // Treat upstream provider / catalog errors as a miss so batch callers
                // still get partial results instead of a hard failure.
                tracing::debug!(error = %err, title = %query.title, "title-meta resolve failed");
                None
            }
        };
    title_meta_cache().insert(key, resolved.clone());
    Ok(resolved)
}

/// Resolves title metadata without the TTL cache (provider record by ASIN, then storefront fill).
async fn resolve_title_meta_uncached(
    query: &TitleMetaQuery,
    providers: &dyn MetadataProviders,
    region: &str,
    sources: Option<&SourceRegistry>,
) -> Result<Option<TitleMeta>> {
    // Prefer a single provider lookup by ASIN. Title/author catalog matching is
    // multiple public HTTP round-trips and is easy to stack under the API budget
    // when many detail modals open — only fall back when no ASIN is known.
    let mut meta = if let Some(asin) = query
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !isbn_like(s))
    {
        enrichment_for_asin(providers, asin, region)
            .await
            .map(TitleMeta::from_enrichment)
    } else {
        None
    };

    if meta.is_none() {
        let title = query.title.trim();
        let authors = query
            .authors
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let isbn = query
            .isbn
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                // Wishlist/library rows sometimes store ISBN-10 in the asin column.
                query
                    .asin
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| isbn_like(s))
            });
        if !title.is_empty() || isbn.is_some() {
            let mq = MatchQuery {
                title: if title.is_empty() {
                    isbn.unwrap_or("")
                } else {
                    title
                },
                subtitle: None,
                author: authors,
                narrator: query
                    .narrators
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                isbn,
                duration_minutes: query.length_minutes.map(|n| n as f64),
            };
            let min = f64::from(DEFAULT_ENRICH_MIN_CONFIDENCE) / 100.0;
            meta = match lookup_by_metadata(providers, &mq, region, min).await? {
                Some(scored) => Some(TitleMeta::from_enrichment(scored.enrichment)),
                None => None,
            };
        }
    }

    if title_meta_needs_store_fill(meta.as_ref()) {
        if let Some(fill) = fill_from_source_catalog(query, sources).await {
            meta = Some(merge_title_meta(meta, fill));
        }
    }

    let Some(mut meta) = meta else {
        return Ok(None);
    };

    if let Some(asin) = meta
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !isbn_like(s))
        .map(str::to_string)
    {
        match public_http_client() {
            Ok(http) => match fetch_audible_catalog_rating(&http, &asin, region).await {
                Ok(Some(rating)) => meta = meta.with_ratings(rating),
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(
                        asin = %asin,
                        error = %err,
                        "title-meta rating fetch failed"
                    )
                }
            },
            Err(err) => tracing::debug!(error = %err, "title-meta rating client failed"),
        }
    }

    Ok(Some(meta))
}

/// Resolve one page of Audible customer reviews for infinite scroll.
///
/// # Arguments
///
/// * `query` - Query vector or free-text search string.
///
/// # Returns
///
/// On success, the inner `TitleReviewsPage` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn resolve_title_reviews(query: &TitleReviewsQuery) -> Result<TitleReviewsPage> {
    let asin = query.asin.trim().to_ascii_uppercase();
    let region = query.region.trim();
    let region = if region.is_empty() { "us" } else { region };
    let page = query.page.max(1);
    let page_size = query.page_size.clamp(1, 20) as usize;
    let sort = CatalogReviewsSort::parse(&query.sort_by);
    let sort_by = sort.as_str().to_string();
    let empty = TitleReviewsPage {
        asin: asin.clone(),
        page,
        page_size: page_size as u32,
        has_more: false,
        sort_by: sort_by.clone(),
        reviews: Vec::new(),
    };
    if asin.is_empty() {
        return Ok(empty);
    }

    let key = cache_key(&[
        "title-reviews-v3",
        region,
        &asin,
        &sort_by,
        &page.to_string(),
        &page_size.to_string(),
    ]);
    if let Some(cached) = title_reviews_cache().get(&key) {
        return Ok(cached);
    }

    let http = match public_http_client() {
        Ok(http) => http,
        Err(err) => {
            tracing::debug!(error = %err, "title-reviews client failed");
            return Ok(empty);
        }
    };

    let page_res =
        match fetch_audible_catalog_reviews_page(&http, &asin, region, page, page_size, sort).await
        {
            Ok(page) => page,
            Err(err) => {
                tracing::debug!(asin = %asin, error = %err, "title-reviews fetch failed");
                return Ok(empty);
            }
        };

    let out = TitleReviewsPage {
        asin,
        page: page_res.page,
        page_size: page_res.page_size as u32,
        has_more: page_res.has_more,
        sort_by,
        reviews: page_res
            .reviews
            .into_iter()
            .map(TitleReview::from)
            .collect(),
    };
    title_reviews_cache().insert(key, out.clone());
    Ok(out)
}

/// Longest a title-meta batch waits before answering with what resolved.
///
/// A query still working then answers as failed (the route shows no
/// metadata for it, and the browser asks again later), and keeps running in
/// the background so its answer is cached for that next ask.
pub const TITLE_META_BATCH_DEADLINE: Duration = Duration::from_secs(6);

/// Resolve many title-meta queries with bounded concurrency (order preserved).
///
/// At most `max_concurrent` queries run at once, and each finished query
/// frees its slot for the next one at once (no query waits for the slowest
/// of a group). The batch answers within [`TITLE_META_BATCH_DEADLINE`].
///
/// # Arguments
///
/// * `queries` - Batch of title-meta queries.
/// * `max_concurrent` - Most queries resolving at once (clamped to 1..=8).
/// * `providers` - Deployed metadata providers asked for records.
/// * `sources` - The deployed storefronts, for gap-filling from their
///   catalogs.
///
/// # Returns
///
/// One result per query, in order. A query that failed, was cancelled or
/// did not finish by the deadline is an error.
pub async fn resolve_title_meta_batch(
    queries: &[TitleMetaQuery],
    max_concurrent: usize,
    providers: Arc<dyn MetadataProviders>,
    sources: Option<&SourceRegistry>,
) -> Vec<Result<Option<TitleMeta>>> {
    let deadline = tokio::time::Instant::now() + TITLE_META_BATCH_DEADLINE;
    let slots = Arc::new(tokio::sync::Semaphore::new(max_concurrent.clamp(1, 8)));
    let sources = sources.cloned();
    let mut set = tokio::task::JoinSet::new();
    for (index, q) in queries.iter().enumerate() {
        let q = q.clone();
        let sources = sources.clone();
        let providers = Arc::clone(&providers);
        let slots = Arc::clone(&slots);
        set.spawn(async move {
            // The semaphore is never closed, so a slot always comes.
            let _slot = slots.acquire_owned().await.ok();
            (
                index,
                resolve_title_meta(&q, providers, sources.as_ref()).await,
            )
        });
    }
    let mut slot: Vec<Option<Result<Option<TitleMeta>>>> =
        (0..queries.len()).map(|_| None).collect();
    let collect = async {
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((index, result)) => slot[index] = Some(result),
                Err(err) => tracing::debug!(error = %err, "title-meta batch task failed"),
            }
        }
    };
    if tokio::time::timeout_at(deadline, collect).await.is_err() {
        let late = set.len();
        // The late queries finish on their own and fill the caches.
        set.detach_all();
        tracing::debug!(
            queries = queries.len(),
            late,
            "title-meta batch answered at its deadline"
        );
    }
    slot.into_iter()
        .map(|result| {
            result.unwrap_or_else(|| {
                Err(crate::error::DiscoverError::message(
                    "title-meta query did not finish before the batch deadline",
                ))
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog_search::tests::DetailStore;

    /// A detail store with the given declarations, its canned record, and
    /// its call log.
    fn store(
        id: &'static str,
        by_isbn: bool,
        by_product_id: bool,
        description: &str,
    ) -> (Arc<dyn ContentSource>, Arc<std::sync::Mutex<Vec<String>>>) {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let store = DetailStore {
            id,
            by_isbn,
            by_product_id,
            answer: Some(CatalogHit {
                product_id: format!("{id}-1"),
                title: "Mistborn".into(),
                description: Some(description.into()),
                ..CatalogHit::default()
            }),
            fails: false,
            calls: calls.clone(),
            listed: Vec::new(),
        };
        (Arc::new(store), calls)
    }

    /// Providers whose lookup of `B0SLOW0001` never answers in time and
    /// that answer everything else at once with nothing.
    struct SlowAsin;

    #[async_trait::async_trait]
    impl MetadataProviders for SlowAsin {
        async fn lookup(
            &self,
            params: &bookclerk_plugin_sdk::MetadataLookupParams,
        ) -> bookclerk_enrich::MetadataAnswer {
            if params.identifiers.iter().any(|id| id.value == "B0SLOW0001") {
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
            bookclerk_enrich::MetadataAnswer::default()
        }

        async fn search(
            &self,
            _params: &bookclerk_plugin_sdk::MetadataSearchParams,
        ) -> bookclerk_enrich::MetadataAnswer {
            bookclerk_enrich::MetadataAnswer::default()
        }

        async fn chapters(
            &self,
            _params: &bookclerk_plugin_sdk::MetadataChaptersParams,
        ) -> bookclerk_enrich::ChaptersAnswer {
            bookclerk_enrich::ChaptersAnswer::default()
        }
    }

    /// A batch answers at its deadline: the query still waiting on a slow
    /// provider is an error (not a cached "no metadata"), and the quick one
    /// keeps its answer.
    #[tokio::test(start_paused = true)]
    async fn a_batch_answers_at_its_deadline() {
        let mut slow = query(&[], None);
        slow.title = "Deadline Slow".into();
        slow.asin = Some("B0SLOW0001".into());
        let mut quick = query(&[], None);
        // An ASIN-shaped title is looked up at the providers directly, so
        // the quick query makes no catalog search over the network.
        quick.title = "B0QUICK001".into();
        let started = tokio::time::Instant::now();
        let results = resolve_title_meta_batch(&[slow, quick], 4, Arc::new(SlowAsin), None).await;
        assert!(started.elapsed() <= TITLE_META_BATCH_DEADLINE + Duration::from_millis(50));
        assert!(results[0].is_err(), "the slow query timed out");
        assert!(
            matches!(results[1], Ok(None)),
            "the quick query answered: {:?}",
            results[1]
        );
    }

    /// A query for "Mistborn" naming `editions` and carrying `isbn`.
    fn query(editions: &[(&str, &str)], isbn: Option<&str>) -> TitleMetaQuery {
        TitleMetaQuery {
            title: "Mistborn".into(),
            authors: None,
            asin: None,
            isbn: isbn.map(str::to_string),
            narrators: None,
            length_minutes: None,
            region: "us".into(),
            store_editions: editions
                .iter()
                .map(|(source, pid)| StoreEdition::new(*source, *pid))
                .collect(),
        }
    }

    /// The listing store answers by its own product id before any ISBN
    /// store is asked, whichever plugin id it has.
    #[tokio::test]
    async fn the_listing_store_fills_by_its_own_product_id_first() {
        let (listing, listing_calls) = store("graphicaudio", false, true, "From GA.");
        let (isbn_store, isbn_calls) = store("libro", true, false, "From Libro.");
        let mut registry = SourceRegistry::new();
        registry.register(listing);
        registry.register(isbn_store);

        let meta = fill_from_source_catalog(
            &query(&[("graphicaudio", "1716")], Some("9780000000001")),
            Some(&registry),
        )
        .await
        .expect("filled");
        assert_eq!(meta.description.as_deref(), Some("From GA."));
        assert_eq!(*listing_calls.lock().unwrap(), vec!["product:1716"]);
        assert!(isbn_calls.lock().unwrap().is_empty());
    }

    /// A listing store that declared no product lookup is never asked; the
    /// ISBN store answers instead.
    #[tokio::test]
    async fn an_undeclared_listing_store_is_skipped_for_the_isbn_store() {
        let (chirp, chirp_calls) = store("chirp", false, false, "From Chirp.");
        let (isbn_store, isbn_calls) = store("libro", true, false, "From Libro.");
        let mut registry = SourceRegistry::new();
        registry.register(chirp);
        registry.register(isbn_store);

        let meta = fill_from_source_catalog(
            &query(&[("chirp", "c-1")], Some("9780000000001")),
            Some(&registry),
        )
        .await
        .expect("filled");
        assert_eq!(meta.description.as_deref(), Some("From Libro."));
        assert!(chirp_calls.lock().unwrap().is_empty());
        assert_eq!(*isbn_calls.lock().unwrap(), vec!["isbn:9780000000001"]);

        // Without an ISBN or a declared listing store nothing is asked.
        assert!(
            fill_from_source_catalog(&query(&[("chirp", "c-1")], None), Some(&registry))
                .await
                .is_none()
        );
        assert_eq!(isbn_calls.lock().unwrap().len(), 1);
    }

    /// Many editions, repeats and unknown stores still make at most
    /// [`STORE_FILL_MAX_LOOKUPS`] lookups.
    #[test]
    fn the_fill_plan_is_deduplicated_and_capped() {
        let (listing, _) = store("graphicaudio", false, true, "From GA.");
        let mut registry = SourceRegistry::new();
        registry.register(listing);
        let editions = [
            ("graphicaudio", "1"),
            ("graphicaudio", " 1 "),
            ("unknown", "x"),
            ("graphicaudio", ""),
            ("graphicaudio", "2"),
            ("graphicaudio", "3"),
            ("graphicaudio", "4"),
        ];
        let plan = store_fill_plan(&query(&editions, None), &registry);
        assert_eq!(plan_ids(&plan), vec!["1", "2"]);
    }

    /// The lookups a plan makes, as `<store>:<product>` or `<store>:isbn:<v>`.
    fn plan_ids(plan: &[(Arc<dyn ContentSource>, FillLookup)]) -> Vec<String> {
        plan.iter()
            .map(|(store, lookup)| match lookup {
                FillLookup::Product(pid) if store.id() == "graphicaudio" => pid.clone(),
                FillLookup::Product(pid) => format!("{}:{pid}", store.id()),
                FillLookup::Isbn(isbn) => format!("{}:isbn:{isbn}", store.id()),
            })
            .collect()
    }

    /// Listing editions never take every slot from the ISBN stores, and a
    /// store whose product id is the ISBN is not asked for it twice.
    #[test]
    fn the_isbn_stores_keep_a_turn_and_are_not_asked_twice() {
        let (listing, _) = store("graphicaudio", false, true, "From GA.");
        let (libro, _) = store("libro", true, true, "From Libro.");
        let mut registry = SourceRegistry::new();
        registry.register(listing);
        registry.register(libro);

        let many = query(
            &[
                ("graphicaudio", "1"),
                ("graphicaudio", "2"),
                ("graphicaudio", "3"),
            ],
            Some("9780000000001"),
        );
        assert_eq!(
            plan_ids(&store_fill_plan(&many, &registry)),
            vec!["1", "2", "libro:isbn:9780000000001"]
        );

        let same = query(&[("libro", "9780000000001")], Some("9780000000001"));
        assert_eq!(
            plan_ids(&store_fill_plan(&same, &registry)),
            vec!["libro:9780000000001"]
        );
    }
}
