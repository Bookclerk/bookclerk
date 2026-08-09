//! Public bibliographic metadata for title detail dialogs.
//!
//! Prefer Audnexus when an ASIN is known; otherwise try a confident Audible
//! catalog match from title/author/ISBN. When those miss or stay sparse (common
//! for Libro.fm ISBN-only hits), fill gaps via source [`ContentSource::catalog_detail`]
//! (Libro explore details or product HTML JSON-LD). Used by Discover, Wishlist,
//! and Library detail views (commerce links stay on a separate purchase-hints path).
//!
//! Community ratings come from a separate Audible catalog `rating` request so
//! bulk Discover expand stays lean. Written reviews are loaded on demand via
//! [`resolve_title_reviews`] (paginated) so title-meta / shelf expand stay light.

use std::sync::OnceLock;
use std::time::Duration;

use bookclerk_enrich::{
    enrichment_for_asin, fetch_audible_catalog_rating, fetch_audible_catalog_reviews_page,
    lookup_by_metadata, public_http_client, CatalogReview, CatalogReviewsSort, Enrichment,
    MatchQuery, DEFAULT_ENRICH_MIN_CONFIDENCE,
};
use bookclerk_source::{CatalogHit, SourceRegistry};

use crate::error::Result;
use crate::ttl_cache::{cache_key, TtlCache};

fn title_meta_cache() -> &'static TtlCache<Option<TitleMeta>> {
    static CACHE: OnceLock<TtlCache<Option<TitleMeta>>> = OnceLock::new();
    // Bump key prefix below when the payload shape changes.
    CACHE.get_or_init(|| TtlCache::new(Duration::from_secs(6 * 60 * 60), 512))
}

fn title_reviews_cache() -> &'static TtlCache<TitleReviewsPage> {
    static CACHE: OnceLock<TtlCache<TitleReviewsPage>> = OnceLock::new();
    CACHE.get_or_init(|| TtlCache::new(Duration::from_secs(30 * 60), 256))
}

/// Query for public title metadata (Audnexus / Audible catalog).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitleMetaQuery {
    #[serde(default)]
    pub title: String,
    pub authors: Option<String>,
    pub asin: Option<String>,
    pub isbn: Option<String>,
    pub narrators: Option<String>,
    pub length_minutes: Option<i64>,
    #[serde(default = "default_region")]
    pub region: String,
}

fn default_region() -> String {
    String::from("us")
}

/// Bibliographic fields suitable for an Audible-style detail panel.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitleMeta {
    pub asin: Option<String>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub authors: Option<String>,
    pub narrators: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub isbn: Option<String>,
    pub cover_url: Option<String>,
    pub description: Option<String>,
    pub publisher: Option<String>,
    pub length_minutes: Option<i64>,
    pub published_at: Option<String>,
    pub categories: Option<String>,
    pub language: Option<String>,
    /// `Some(true)` abridged / `Some(false)` unabridged when the storefront said so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_abridged: Option<bool>,
    /// Audible community overall rating (0–5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_overall: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_performance: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_story: Option<f64>,
    /// Count of star ratings (Audible `num_ratings`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating_count: Option<i64>,
    /// Count of written reviews (Audible `num_reviews`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_count: Option<i64>,
    /// Deprecated: reviews load via `/api/discover/title-reviews`. Kept empty
    /// for older clients; prefer the paginated reviews endpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<TitleReview>,
}

/// Query for a page of Audible customer reviews.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitleReviewsQuery {
    pub asin: String,
    #[serde(default = "default_region")]
    pub region: String,
    /// 1-based page index.
    #[serde(default = "default_reviews_page")]
    pub page: u32,
    /// Page size (clamped server-side to 1..=20).
    #[serde(default = "default_reviews_page_size")]
    pub page_size: u32,
    /// `MostHelpful` (default) or `MostRecent`.
    #[serde(default = "default_reviews_sort")]
    pub sort_by: String,
}

fn default_reviews_page() -> u32 {
    1
}

fn default_reviews_page_size() -> u32 {
    5
}

fn default_reviews_sort() -> String {
    String::from("MostHelpful")
}

/// One page of title reviews for infinite scroll.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitleReviewsPage {
    pub asin: String,
    pub page: u32,
    pub page_size: u32,
    pub has_more: bool,
    pub sort_by: String,
    pub reviews: Vec<TitleReview>,
}

/// One Audible customer review for the title detail panel.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TitleReview {
    pub id: Option<String>,
    pub title: Option<String>,
    pub body: String,
    pub author_name: Option<String>,
    pub overall_rating: Option<i64>,
    pub performance_rating: Option<i64>,
    pub story_rating: Option<i64>,
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
    fn from_enrichment(e: Enrichment) -> Self {
        Self {
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

    fn with_ratings(mut self, rating: bookclerk_enrich::CatalogRating) -> Self {
        self.rating_overall = rating.overall;
        self.rating_performance = rating.performance;
        self.rating_story = rating.story;
        self.rating_count = rating.num_ratings;
        self.review_count = rating.num_reviews;
        self
    }

    fn from_catalog_hit(hit: CatalogHit) -> Self {
        Self {
            asin: hit.asin.filter(|s| !s.is_empty()),
            title: Some(hit.title).filter(|s| !s.is_empty()),
            subtitle: hit.subtitle,
            authors: hit.authors,
            narrators: hit.narrators,
            series: hit.series,
            series_index: hit.series_index,
            isbn: hit.isbn,
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

async fn fill_from_source_catalog(
    query: &TitleMetaQuery,
    sources: Option<&SourceRegistry>,
) -> Option<TitleMeta> {
    let sources = sources?;
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
        })?;
    // Libro.fm is the ISBN-keyed storefront; other sources use different ids.
    let libro = sources.get("libro")?;
    match libro.catalog_detail(isbn).await {
        Ok(Some(hit)) => Some(TitleMeta::from_catalog_hit(hit)),
        Ok(None) => None,
        Err(err) => {
            tracing::debug!(isbn, error = %err, "title-meta libro catalog_detail failed");
            None
        }
    }
}

/// Resolve public metadata for a title detail dialog.
///
/// When `sources` is provided, sparse Audible/Audnexus results are gap-filled
/// from storefront [`ContentSource::catalog_detail`] (Libro ISBN details).
pub async fn resolve_title_meta(
    query: &TitleMetaQuery,
    sources: Option<&SourceRegistry>,
) -> Result<Option<TitleMeta>> {
    let region = query.region.trim();
    let region = if region.is_empty() { "us" } else { region };
    let key = cache_key(&[
        "title-meta-v8",
        region,
        query.asin.as_deref().unwrap_or(""),
        query.isbn.as_deref().unwrap_or(""),
        query.title.as_str(),
        query.authors.as_deref().unwrap_or(""),
    ]);
    if let Some(cached) = title_meta_cache().get(&key) {
        return Ok(cached);
    }

    let resolved = match resolve_title_meta_uncached(query, region, sources).await {
        Ok(meta) => meta,
        Err(err) => {
            // Treat upstream Audnexus / catalog errors as a miss so batch callers
            // still get partial results instead of a hard failure.
            tracing::debug!(error = %err, title = %query.title, "title-meta resolve failed");
            None
        }
    };
    title_meta_cache().insert(key, resolved.clone());
    Ok(resolved)
}

async fn resolve_title_meta_uncached(
    query: &TitleMetaQuery,
    region: &str,
    sources: Option<&SourceRegistry>,
) -> Result<Option<TitleMeta>> {
    // Prefer a single Audnexus fetch by ASIN. Title/author catalog matching is
    // multiple public HTTP round-trips and is easy to stack under the API budget
    // when many detail modals open — only fall back when no ASIN is known.
    let mut meta = if let Some(asin) = query
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| !isbn_like(s))
    {
        enrichment_for_asin(asin, region)
            .await?
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
            meta = match lookup_by_metadata(&mq, region, min).await? {
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

/// Resolve many title-meta queries with bounded concurrency (order preserved).
pub async fn resolve_title_meta_batch(
    queries: &[TitleMetaQuery],
    max_concurrent: usize,
    sources: Option<&SourceRegistry>,
) -> Vec<Result<Option<TitleMeta>>> {
    let limit = max_concurrent.clamp(1, 8);
    let sources = sources.cloned();
    let mut out = Vec::with_capacity(queries.len());
    for chunk in queries.chunks(limit) {
        let mut set = tokio::task::JoinSet::new();
        for (offset, q) in chunk.iter().enumerate() {
            let q = q.clone();
            let sources = sources.clone();
            set.spawn(async move { (offset, resolve_title_meta(&q, sources.as_ref()).await) });
        }
        let mut slot: Vec<Option<Result<Option<TitleMeta>>>> =
            (0..chunk.len()).map(|_| None).collect();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((offset, result)) => slot[offset] = Some(result),
                Err(err) => tracing::debug!(error = %err, "title-meta batch task failed"),
            }
        }
        for result in slot {
            out.push(result.unwrap_or_else(|| {
                Err(crate::error::DiscoverError::message(
                    "title-meta batch task cancelled",
                ))
            }));
        }
    }
    out
}
