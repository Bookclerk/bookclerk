//! Multi-storefront catalog search for Discover.
//!
//! Search is a **non-personalized catalog browser**: fan out one page per
//! storefront, identity-merge, apply server-side include/exclude filters (with
//! over-fetch), then return a page envelope + opaque cursor. Personalized
//! ranking stays on Discover shelves.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use std::time::Duration;

use bookclerk_source::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, CatalogSearchSort, CatalogSortDir,
    SourceRegistry,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::candidates::{hit_to_candidate, StorefrontCandidate};
use crate::error::Result;
use crate::identity::{
    hard_work_key, identities_match, merge_candidate_metadata, push_edition, work_map_key,
    StoreEdition, WorkIdentity,
};
use crate::ttl_cache::TtlCache;

/// Cap a single storefront so one slow guest cannot burn the whole search budget.
/// Keep this under the daemon `/api/discover/search` wall clock so parallel
/// stores finish before the handler times out.
const PER_SOURCE_SEARCH_TIMEOUT: Duration = Duration::from_millis(7_000);

/// After rank/truncate, enrich lean ISBN-bearing rows via Libro `catalog_detail`
/// (genres, narrators, abridged, …) without N+1 inside each store search.
const PAGE_ENRICH_TIMEOUT: Duration = Duration::from_millis(3_500);
const PAGE_ENRICH_CONCURRENCY: usize = 6;

/// Max over-fetch rounds when filters starve a page.
const MAX_OVERFETCH_ITERS: usize = 3;

/// Prefix for opaque server-side cursors (`s1:<uuid>`). Pending leftovers are
/// stored in-process so the wire token stays short — embedding full candidates
/// in a GET `?cursor=` hex blob overflows URI limits (HTTP 414) on broad queries.
const CURSOR_TOKEN_PREFIX: &str = "s1:";

/// How long a search cursor may sit idle between "load more" requests.
const CURSOR_TTL: Duration = Duration::from_secs(30 * 60);

/// Cap concurrent in-flight / paused search cursors.
const CURSOR_CACHE_CAP: usize = 256;

fn cursor_cache() -> &'static TtlCache<SearchCursorV1> {
    static CACHE: OnceLock<TtlCache<SearchCursorV1>> = OnceLock::new();
    CACHE.get_or_init(|| TtlCache::new(CURSOR_TTL, CURSOR_CACHE_CAP))
}

/// One autocomplete / results-page hit (possibly spanning multiple storefronts).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CatalogSearchHit {
    /// Stable map key for this work (`isbn:…`, `asin:…`, or soft title+author).
    pub work_key: String,
    /// Display title as shown on the storefront or library card.
    pub title: String,
    /// Comma-separated author names when the storefront provides them.
    pub authors: Option<String>,
    /// Comma-separated narrator names when known.
    pub narrators: Option<String>,
    /// Series name when the title belongs to a named series.
    pub series: Option<String>,
    /// Position within the series (e.g. `1`, `1.5`) when known.
    #[serde(default)]
    pub series_index: Option<String>,
    /// Audible / Amazon ASIN when this edition is sold on Audible.
    pub asin: Option<String>,
    /// Canonical ISBN-13 (or ISBN-10 normalized) when published.
    pub isbn: Option<String>,
    /// HTTPS URL for cover art when the catalog exposes one.
    #[serde(default)]
    pub cover_url: Option<String>,
    /// Per-storefront edition ids that collapsed into this work card.
    pub store_editions: Vec<StoreEdition>,
    /// Storefronts that matched (deduped source ids).
    pub sources: Vec<String>,
    /// Optional subtitle when the catalog distinguishes it from the title.
    #[serde(default)]
    pub subtitle: Option<String>,
    /// Publisher or storefront synopsis; may be truncated for embeddings.
    #[serde(default)]
    pub description: Option<String>,
    /// Publisher imprint as reported by the catalog.
    #[serde(default)]
    pub publisher: Option<String>,
    /// Audiobook runtime in whole minutes when known.
    #[serde(default)]
    pub length_minutes: Option<i64>,
    /// Publication date string from the catalog (ISO or storefront format).
    #[serde(default)]
    pub published_at: Option<String>,
    /// Genre labels from the catalog (comma-separated when multiple).
    #[serde(default)]
    pub genres: Option<String>,
    /// BCP-47 / storefront language code (`en`, `de`, …) when known.
    #[serde(default)]
    pub language: Option<String>,
    /// When `Some(true)`, the edition is marked abridged by the storefront.
    #[serde(default)]
    pub is_abridged: Option<bool>,
    /// Aggregate listener rating on the storefront's scale when known.
    #[serde(default)]
    pub rating_overall: Option<f64>,
    /// Number of ratings backing [`Self::rating_overall`] when known.
    #[serde(default)]
    pub rating_count: Option<i64>,
    /// Catalog list/deal price in cents when the storefront provided it.
    #[serde(default)]
    pub price_cents: Option<i64>,
    /// Buy / open-in-store links with optional member vs list pricing.
    #[serde(default)]
    pub purchase_hints: Vec<crate::purchase::PurchaseHint>,
}

/// Paged catalog-search response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogSearchPage {
    /// Ordered collection of child entries for this response.
    pub items: Vec<CatalogSearchHit>,
    /// Maximum number of items returned in this page.
    pub page_size: usize,
    /// When true, another page is available via [`Self::next_cursor`].
    pub has_more: bool,
    /// Opaque cursor for the next page; omit or `None` when exhausted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Sort field applied to the merged result set.
    pub sort: String,
    /// Ascending or descending order for [`Self::sort`].
    #[serde(default)]
    pub sort_dir: String,
}

/// Include / exclude filters applied after identity merge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CatalogSearchFilters {
    /// Comma-separated author names when the storefront provides them.
    #[serde(default)]
    pub authors: Vec<String>,
    /// Comma-separated narrator names when known.
    #[serde(default)]
    pub narrators: Vec<String>,
    /// Series name when the title belongs to a named series.
    #[serde(default)]
    pub series: Vec<String>,
    /// Genre labels from the catalog (comma-separated when multiple).
    #[serde(default)]
    pub genres: Vec<String>,
    /// Deduped storefront ids that contributed editions to this hit.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Store ids to drop (any edition matching). Empty = no exclude.
    #[serde(default)]
    pub exclude_sources: Vec<String>,
    /// Normalized language codes (`en`, `zh`, …). When non-empty, only matching
    /// (or unknown/missing) languages pass.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Substring match on narrator strings (case-insensitive), e.g. `Virtual Voice`.
    #[serde(default)]
    pub exclude_narrators: Vec<String>,
    /// Keep hits with `rating_overall >= min` or missing rating.
    #[serde(default)]
    pub min_rating: Option<f64>,
    /// Inclusive lower bound on `length_minutes`; missing length passes.
    #[serde(default)]
    pub min_length_minutes: Option<i64>,
    /// Inclusive upper bound on `length_minutes`; missing length passes.
    #[serde(default)]
    pub max_length_minutes: Option<i64>,
}

impl CatalogSearchFilters {
    /// Returns true when no include/exclude filters are set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.authors.is_empty()
            && self.narrators.is_empty()
            && self.series.is_empty()
            && self.genres.is_empty()
            && self.sources.is_empty()
            && self.exclude_sources.is_empty()
            && self.languages.is_empty()
            && self.exclude_narrators.is_empty()
            && self.min_rating.is_none()
            && self.min_length_minutes.is_none()
            && self.max_length_minutes.is_none()
    }
}

/// Options for [`catalog_search_page`].
#[derive(Debug, Clone)]
pub struct CatalogSearchPageOpts<'a> {
    /// Free-text search string entered by the operator or SPA.
    pub query: &'a str,
    /// Marketplace / region code (`us`, `uk`, …) for catalog lookups.
    pub region: &'a str,
    /// Maximum number of items returned in this page.
    pub page_size: usize,
    /// Opaque pagination token from a previous page response.
    pub cursor: Option<&'a str>,
    /// Sort field applied to the merged result set.
    pub sort: CatalogSearchSort,
    /// Ascending or descending order for [`Self::sort`].
    pub sort_dir: CatalogSortDir,
    /// Optional catalog field scope (`title`, `author`, …) when searching one facet.
    pub field: Option<CatalogSearchField>,
    /// BCP-47 / storefront language code (`en`, `de`, …) when known.
    pub language: Option<&'a str>,
    /// When true, do not default hard language filter from [`Self::language`].
    pub all_languages: bool,
    /// Server-side include/exclude filters applied after identity merge.
    pub filters: CatalogSearchFilters,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SearchCursorV1 {
    v: u8,
    fp: String,
    /// Next 1-based page to fetch per source id.
    pages: HashMap<String, u32>,
    exhausted: HashSet<String>,
    /// Ranked leftovers from a prior merge that have not been returned yet.
    /// Load-more drains this before advancing storefront pages so over-fetched
    /// rows below the previous page cut are not dropped.
    #[serde(default)]
    pending: Vec<PendingSearchRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingSearchRow {
    work_key: String,
    candidate: StorefrontCandidate,
}

/// Search every configured storefront catalog and merge by work identity.
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `query` - Query vector or free-text search string.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `limit` - Maximum number of results to return.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogSearchHit>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn catalog_search(
    registry: &SourceRegistry,
    query: &str,
    region: &str,
    limit: usize,
) -> Result<Vec<CatalogSearchHit>> {
    catalog_search_with_field(registry, query, region, limit, None, None).await
}

/// Like [`catalog_search`], optionally scoping storefront queries to a facet
/// (author / narrator / series / genre). When `language` is set it is applied
/// as a hard include filter (unknown/missing language still passes).
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `query` - Query vector or free-text search string.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
/// * `limit` - Maximum number of results to return.
/// * `field` - Optional catalog search field scope.
/// * `language` - Optional preferred language code.
///
/// # Returns
///
/// On success, the inner `Vec<CatalogSearchHit>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn catalog_search_with_field(
    registry: &SourceRegistry,
    query: &str,
    region: &str,
    limit: usize,
    field: Option<CatalogSearchField>,
    language: Option<&str>,
) -> Result<Vec<CatalogSearchHit>> {
    let mut filters = CatalogSearchFilters::default();
    if let Some(code) = language.and_then(bookclerk_source::normalize_language) {
        filters.languages = vec![code];
    }
    let page = catalog_search_page(
        registry,
        CatalogSearchPageOpts {
            query,
            region,
            page_size: limit,
            cursor: None,
            sort: CatalogSearchSort::Relevance,
            sort_dir: CatalogSortDir::Desc,
            field,
            language,
            all_languages: false,
            filters,
        },
    )
    .await?;
    Ok(page.items)
}

/// Paged multi-store catalog browse (filters + sort + cursor).
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `opts` - Options struct for this operation.
///
/// # Returns
///
/// On success, the inner `CatalogSearchPage` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn catalog_search_page(
    registry: &SourceRegistry,
    opts: CatalogSearchPageOpts<'_>,
) -> Result<CatalogSearchPage> {
    let q = opts.query.trim();
    // API defaults to 24 (12–48); typeahead may pass a smaller limit (e.g. 10).
    let page_size = opts.page_size.clamp(1, 48);
    let sort_wire = opts.sort.as_wire().to_string();
    let sort_dir_wire = opts.sort_dir.as_wire().to_string();
    if q.is_empty() || page_size == 0 {
        return Ok(CatalogSearchPage {
            items: Vec::new(),
            page_size,
            has_more: false,
            next_cursor: None,
            sort: sort_wire,
            sort_dir: sort_dir_wire,
        });
    }

    let region = opts.region.trim().to_ascii_lowercase();
    let region = if region.is_empty() {
        String::from("us")
    } else {
        region
    };
    let preferred = opts
        .language
        .and_then(bookclerk_source::normalize_language)
        .unwrap_or_else(|| bookclerk_source::default_preferred_language().to_string());

    // Default hard language include from `lang` when the client did not pass
    // explicit `language=` filters (Discover UI always sends browser lang).
    let mut filters = opts.filters.clone();
    if !filters.languages.is_empty() {
        filters.languages = filters
            .languages
            .iter()
            .filter_map(|s| bookclerk_source::normalize_language(s))
            .collect();
        filters.languages.sort();
        filters.languages.dedup();
    } else if !opts.all_languages {
        if let Some(code) = opts.language.and_then(bookclerk_source::normalize_language) {
            filters.languages.push(code);
        }
    }

    let fp = cursor_fingerprint(
        q,
        &sort_wire,
        &sort_dir_wire,
        opts.field.map(|f| f.as_wire()),
        Some(preferred.as_str()),
        &filters,
    );

    let mut cursor = match opts.cursor {
        Some(raw) => match decode_cursor(raw) {
            Some(c) if c.v == 1 && c.fp == fp => c,
            _ => SearchCursorV1 {
                v: 1,
                fp: fp.clone(),
                pages: HashMap::new(),
                exhausted: HashSet::new(),
                pending: Vec::new(),
            },
        },
        None => SearchCursorV1 {
            v: 1,
            fp: fp.clone(),
            pages: HashMap::new(),
            exhausted: HashSet::new(),
            pending: Vec::new(),
        },
    };

    let source_ids: Vec<String> = registry.all().iter().map(|s| s.id().to_string()).collect();
    for id in &source_ids {
        cursor.pages.entry(id.clone()).or_insert(1);
    }

    // Over-fetch a bit so exclude filters (Virtual Voice) don't starve the page.
    let per_store = (page_size.saturating_add(8)).clamp(12, 50);
    let mut by_key: HashMap<String, StorefrontCandidate> = HashMap::new();
    // Resume with ranked leftovers so prior over-fetch is not discarded.
    for row in cursor.pending.drain(..) {
        by_key.insert(row.work_key, row.candidate);
    }
    let mut any_source_has_more = false;

    // Drain pending first; only hit storefronts when the buffer cannot fill a page.
    let need_fetch = by_key
        .values()
        .filter(|c| passes_filters(c, &filters))
        .count()
        < page_size;

    if need_fetch {
        for _iter in 0..MAX_OVERFETCH_ITERS {
            let active: Vec<(String, u32)> = source_ids
                .iter()
                .filter(|id| !cursor.exhausted.contains(id.as_str()))
                .map(|id| {
                    let page = *cursor.pages.get(id).unwrap_or(&1);
                    (id.clone(), page)
                })
                .collect();
            if active.is_empty() {
                break;
            }

            let mut set = JoinSet::new();
            for (id, page) in &active {
                let Some(source) = registry.get(id) else {
                    cursor.exhausted.insert(id.clone());
                    continue;
                };
                let search_opts = CatalogSearchOpts {
                    query: q.to_string(),
                    region: region.clone(),
                    limit: per_store,
                    page: (*page).max(1),
                    sort: opts.sort,
                    field: opts.field,
                    language: Some(preferred.clone()),
                };
                let id = id.clone();
                set.spawn(async move {
                    let outcome = timeout(
                        PER_SOURCE_SEARCH_TIMEOUT,
                        source.search_catalog(&search_opts),
                    )
                    .await;
                    (id, outcome)
                });
            }

            let mut fetched_any = false;
            while let Some(joined) = set.join_next().await {
                let (id, outcome) = match joined {
                    Ok(v) => v,
                    Err(err) => {
                        tracing::debug!(error = %err, "catalog search task join failed");
                        continue;
                    }
                };
                let hits = match outcome {
                    Ok(Ok(hits)) => hits,
                    Ok(Err(err)) => {
                        tracing::debug!(source = %id, error = %err, "catalog search store failed");
                        cursor.exhausted.insert(id);
                        continue;
                    }
                    Err(_) => {
                        tracing::debug!(
                            source = %id,
                            timeout_ms = PER_SOURCE_SEARCH_TIMEOUT.as_millis(),
                            "catalog search store timed out"
                        );
                        cursor.exhausted.insert(id);
                        continue;
                    }
                };
                fetched_any = true;
                let page_now = *cursor.pages.get(&id).unwrap_or(&1);
                let n = hits.len();
                if n == 0 || n < per_store {
                    cursor.exhausted.insert(id.clone());
                } else {
                    cursor.pages.insert(id.clone(), page_now.saturating_add(1));
                    any_source_has_more = true;
                }
                for (rank, hit) in hits.into_iter().enumerate() {
                    let hit = hit.decode_html_entities();
                    let audible_rank = if id.eq_ignore_ascii_case("audible") {
                        Some(
                            page_now
                                .saturating_sub(1)
                                .saturating_mul(per_store as u32)
                                .saturating_add(rank as u32),
                        )
                    } else {
                        None
                    };
                    upsert_hit(
                        &mut by_key,
                        StorefrontCandidate {
                            source: id.clone(),
                            product_id: hit.product_id.clone(),
                            title: hit.title.clone(),
                            authors: hit.authors.clone(),
                            narrators: hit.narrators.clone(),
                            series: hit.series.clone(),
                            series_index: hit.series_index.clone(),
                            asin: hit.asin.clone(),
                            isbn: hit.isbn.clone(),
                            cover_url: hit.cover_url.clone(),
                            seed_categories: None,
                            origin: String::from("catalog search"),
                            seed_title: None,
                            store_editions: Vec::new(),
                            subtitle: hit.subtitle.clone(),
                            description: hit.description.clone(),
                            publisher: hit.publisher.clone(),
                            length_minutes: hit.length_minutes,
                            published_at: hit.published_at.clone(),
                            categories: hit.categories.clone(),
                            language: hit.language.clone(),
                            price_cents: hit.price_cents,
                            currency: hit.currency.clone(),
                            price_label: hit.price_label.clone(),
                            rating_overall: hit.rating_overall,
                            rating_count: hit.rating_count,
                            is_abridged: hit.is_abridged,
                            audible_rank,
                        },
                    );
                }
            }

            if !fetched_any {
                break;
            }

            let filtered_count = by_key
                .values()
                .filter(|c| passes_filters(c, &filters))
                .count();
            if filtered_count >= page_size {
                break;
            }
            // Continue over-fetch only while some source still has more pages.
            let still = source_ids
                .iter()
                .any(|id| !cursor.exhausted.contains(id.as_str()));
            if !still {
                break;
            }
        }
    } // need_fetch

    let mut ranked: Vec<(String, StorefrontCandidate)> = by_key
        .into_iter()
        .filter(|(_, c)| passes_filters(c, &filters))
        .collect();
    rank_candidates(&mut ranked, opts.sort, opts.sort_dir, q, &preferred);
    let page: Vec<(String, StorefrontCandidate)> =
        ranked.drain(..ranked.len().min(page_size)).collect();
    cursor.pending = ranked
        .into_iter()
        .map(|(work_key, candidate)| PendingSearchRow {
            work_key,
            candidate,
        })
        .collect();
    let sources_remain = any_source_has_more
        || source_ids
            .iter()
            .any(|id| !cursor.exhausted.contains(id.as_str()));
    let has_more = !cursor.pending.is_empty() || sources_remain;
    let mut ranked = page;
    enrich_ranked_page(registry, &mut ranked).await;
    let out: Vec<CatalogSearchHit> = ranked
        .into_iter()
        .map(|(work_key, c)| candidate_to_hit(work_key, c, &region))
        .collect();

    let next_cursor = if has_more {
        Some(encode_cursor(&cursor))
    } else {
        None
    };

    Ok(CatalogSearchPage {
        items: out,
        page_size,
        has_more,
        next_cursor,
        sort: sort_wire,
        sort_dir: sort_dir_wire,
    })
}

fn isbn_key_for_enrich(c: &StorefrontCandidate) -> Option<String> {
    if let Some(isbn) = c.isbn.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return Some(isbn.to_string());
    }
    for ed in &c.store_editions {
        if ed.source.eq_ignore_ascii_case("libro") {
            let pid = ed.product_id.trim();
            if !pid.is_empty() {
                return Some(pid.to_string());
            }
        }
    }
    if c.source.eq_ignore_ascii_case("libro") {
        let pid = c.product_id.trim();
        if !pid.is_empty() {
            return Some(pid.to_string());
        }
    }
    None
}

fn candidate_needs_page_enrich(c: &StorefrontCandidate) -> bool {
    let lean = c
        .description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
        || c.narrators
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        || c.length_minutes.is_none()
        || c.categories
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        || c.is_abridged.is_none();
    lean && isbn_key_for_enrich(c).is_some()
}

/// Fill bibliographic gaps on the final page via Libro product detail.
///
/// Bounded by [`PAGE_ENRICH_TIMEOUT`] so a slow guest cannot exceed the
/// daemon search budget. Chirp-only rows without ISBN are skipped (title-meta
/// / purchase-hints still fill those in the detail modal).
async fn enrich_ranked_page(
    registry: &SourceRegistry,
    ranked: &mut [(String, StorefrontCandidate)],
) {
    let Some(libro) = registry.get("libro") else {
        return;
    };
    let pending: Vec<(usize, String)> = ranked
        .iter()
        .enumerate()
        .filter_map(|(i, (_, c))| {
            if !candidate_needs_page_enrich(c) {
                return None;
            }
            isbn_key_for_enrich(c).map(|k| (i, k))
        })
        .collect();
    if pending.is_empty() {
        return;
    }

    let enrich = async {
        for chunk in pending.chunks(PAGE_ENRICH_CONCURRENCY) {
            let mut set = JoinSet::new();
            for &(i, ref key) in chunk {
                let libro = libro.clone();
                let key = key.clone();
                set.spawn(async move {
                    let hit = match libro.catalog_detail(&key).await {
                        Ok(h) => h,
                        Err(err) => {
                            tracing::debug!(
                                key = %key,
                                error = %err,
                                "catalog search page enrich failed"
                            );
                            None
                        }
                    };
                    (i, hit)
                });
            }
            while let Some(joined) = set.join_next().await {
                if let Ok((i, Some(hit))) = joined {
                    if let Some((_, c)) = ranked.get_mut(i) {
                        apply_catalog_detail_hit(c, hit);
                    }
                }
            }
        }
    };

    if timeout(PAGE_ENRICH_TIMEOUT, enrich).await.is_err() {
        tracing::debug!(
            pending = pending.len(),
            timeout_ms = PAGE_ENRICH_TIMEOUT.as_millis(),
            "catalog search page enrich timed out"
        );
    }
}

fn apply_catalog_detail_hit(c: &mut StorefrontCandidate, hit: CatalogHit) {
    let from = hit_to_candidate("libro", hit);
    merge_candidate_metadata(c, &from);
}

fn candidate_to_hit(work_key: String, c: StorefrontCandidate, region: &str) -> CatalogSearchHit {
    let mut sources: Vec<String> = c.store_editions.iter().map(|e| e.source.clone()).collect();
    sources.sort();
    sources.dedup();
    let mut purchase_hints = Vec::new();
    // Align with resolve_purchase_hints: Audible-only URL seeds on cards.
    if crate::purchase::seed_source_is_trusted(&c.source) {
        if let Some(label) = c.price_label.clone().filter(|s| !s.is_empty()) {
            if let Some(hint) = crate::purchase::seed_purchase_hint(
                &c.source,
                &c.product_id,
                Some(c.title.clone()),
                region,
            ) {
                purchase_hints.push(crate::purchase::PurchaseHint {
                    price_cents: c.price_cents,
                    currency: c.currency.clone(),
                    price_label: Some(label),
                    ..hint
                });
            }
        }
    }
    CatalogSearchHit {
        work_key,
        title: c.title,
        authors: c.authors,
        narrators: c.narrators,
        series: c.series,
        series_index: c.series_index,
        asin: c.asin,
        isbn: c.isbn,
        cover_url: c.cover_url,
        store_editions: c.store_editions,
        sources,
        subtitle: c.subtitle,
        description: c.description,
        publisher: c.publisher,
        length_minutes: c.length_minutes,
        published_at: c.published_at,
        genres: c.categories,
        language: c.language,
        is_abridged: c.is_abridged,
        rating_overall: c.rating_overall,
        rating_count: c.rating_count,
        price_cents: c.price_cents,
        purchase_hints,
    }
}

/// Prior mean for Bayesian rating sort (typical audiobook catalog average).
const RATING_SORT_PRIOR_MEAN: f64 = 4.0;
/// Virtual rating count — raw averages need this many votes to dominate.
const RATING_SORT_PRIOR_STRENGTH: f64 = 100.0;

/// Deterministic Bayesian average: `(v/(v+m))*R + (m/(v+m))*C`.
///
/// Missing counts use `v = 0` so a 5.0 with few/no votes cannot outrank a
/// slightly lower score with a large sample.
#[must_use]
fn bayesian_rating_score(rating: f64, count: Option<i64>) -> f64 {
    let v = count.unwrap_or(0).max(0) as f64;
    let m = RATING_SORT_PRIOR_STRENGTH;
    let c = RATING_SORT_PRIOR_MEAN;
    (v / (v + m)) * rating + (m / (v + m)) * c
}

fn apply_sort_dir(ord: Ordering, dir: CatalogSortDir) -> Ordering {
    match dir {
        CatalogSortDir::Asc => ord,
        CatalogSortDir::Desc => ord.reverse(),
    }
}

/// Compare optional numeric keys; missing values always sort last.
fn cmp_opt_i64(a: Option<i64>, b: Option<i64>, dir: CatalogSortDir) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(av), Some(bv)) => apply_sort_dir(av.cmp(&bv), dir),
    }
}

fn cmp_opt_f64(a: Option<f64>, b: Option<f64>, dir: CatalogSortDir) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(av), Some(bv)) => apply_sort_dir(av.partial_cmp(&bv).unwrap_or(Ordering::Equal), dir),
    }
}

fn rank_candidates(
    out: &mut [(String, StorefrontCandidate)],
    sort: CatalogSearchSort,
    dir: CatalogSortDir,
    q: &str,
    preferred: &str,
) {
    let q_lower = q.to_ascii_lowercase();
    // Relevance keeps Audible/page order regardless of sort_dir (dir is stored
    // for prefs when the user switches to a directional sort).
    let effective_dir = if matches!(sort, CatalogSearchSort::Relevance) {
        CatalogSortDir::Asc
    } else {
        dir
    };
    out.sort_by(|(_, a), (_, b)| {
        let primary = match sort {
            CatalogSearchSort::Title => apply_sort_dir(
                a.title
                    .to_ascii_lowercase()
                    .cmp(&b.title.to_ascii_lowercase()),
                effective_dir,
            ),
            CatalogSearchSort::Author => apply_sort_dir(
                a.authors
                    .as_deref()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .cmp(&b.authors.as_deref().unwrap_or("").to_ascii_lowercase()),
                effective_dir,
            ),
            CatalogSearchSort::Price => cmp_opt_i64(a.price_cents, b.price_cents, effective_dir),
            CatalogSearchSort::Length => {
                cmp_opt_i64(a.length_minutes, b.length_minutes, effective_dir)
            }
            CatalogSearchSort::Rating => {
                let a_score = a
                    .rating_overall
                    .map(|r| bayesian_rating_score(r, a.rating_count));
                let b_score = b
                    .rating_overall
                    .map(|r| bayesian_rating_score(r, b.rating_count));
                cmp_opt_f64(a_score, b_score, effective_dir)
            }
            CatalogSearchSort::Relevance => {
                let a_rank = a.audible_rank.unwrap_or(u32::MAX);
                let b_rank = b.audible_rank.unwrap_or(u32::MAX);
                // Storefront page order: rank 0 first (ignore sort_dir).
                a_rank.cmp(&b_rank)
            }
            CatalogSearchSort::Popularity => {
                let a_rank = a.audible_rank.unwrap_or(u32::MAX);
                let b_rank = b.audible_rank.unwrap_or(u32::MAX);
                // audible_rank 0 = most popular. Invert so Asc = least popular
                // first and Desc = most popular first (matches rating/price).
                apply_sort_dir(b_rank.cmp(&a_rank), effective_dir)
            }
        };
        primary
            .then_with(|| {
                if matches!(
                    sort,
                    CatalogSearchSort::Popularity | CatalogSearchSort::Relevance
                ) {
                    Ordering::Equal
                } else if matches!(sort, CatalogSearchSort::Rating) {
                    let a_rank = a.audible_rank.unwrap_or(u32::MAX);
                    let b_rank = b.audible_rank.unwrap_or(u32::MAX);
                    a_rank.cmp(&b_rank)
                } else {
                    Ordering::Equal
                }
            })
            .then_with(|| b.store_editions.len().cmp(&a.store_editions.len()))
            .then_with(|| {
                bookclerk_source::language_rank(a.language.as_deref(), preferred).cmp(
                    &bookclerk_source::language_rank(b.language.as_deref(), preferred),
                )
            })
            .then_with(|| {
                let a_match = a.title.to_ascii_lowercase().starts_with(&q_lower) as u8;
                let b_match = b.title.to_ascii_lowercase().starts_with(&q_lower) as u8;
                b_match.cmp(&a_match)
            })
            .then_with(|| a.title.len().cmp(&b.title.len()))
            .then_with(|| a.title.cmp(&b.title))
            .then_with(|| a.product_id.cmp(&b.product_id))
    });
}

fn passes_filters(c: &StorefrontCandidate, f: &CatalogSearchFilters) -> bool {
    if f.is_empty() {
        return true;
    }
    if !f.exclude_narrators.is_empty() {
        let narr = c.narrators.as_deref().unwrap_or("").to_ascii_lowercase();
        for ex in &f.exclude_narrators {
            let needle = ex.trim().to_ascii_lowercase();
            if !needle.is_empty() && narr.contains(&needle) {
                return false;
            }
        }
    }
    if !f.languages.is_empty() && !language_passes(c.language.as_deref(), &f.languages) {
        return false;
    }
    if let Some(min) = f.min_rating {
        if let Some(r) = c.rating_overall {
            if r < min {
                return false;
            }
        }
    }
    if let Some(min_len) = f.min_length_minutes {
        if let Some(len) = c.length_minutes {
            if len < min_len {
                return false;
            }
        }
    }
    if let Some(max_len) = f.max_length_minutes {
        if let Some(len) = c.length_minutes {
            if len > max_len {
                return false;
            }
        }
    }
    if !f.authors.is_empty() && !list_matches_any(c.authors.as_deref(), &f.authors) {
        return false;
    }
    if !f.narrators.is_empty() && !list_matches_any(c.narrators.as_deref(), &f.narrators) {
        return false;
    }
    if !f.series.is_empty() {
        let series = c.series.as_deref().unwrap_or("").trim();
        let ok = f.series.iter().any(|want| {
            let w = want.trim();
            if w.is_empty() {
                return false;
            }
            series.eq_ignore_ascii_case(w)
        });
        if !ok {
            return false;
        }
    }
    if !f.genres.is_empty() && !list_matches_any(c.categories.as_deref(), &f.genres) {
        return false;
    }
    let hit_sources: HashSet<String> = c
        .store_editions
        .iter()
        .map(|e| e.source.to_ascii_lowercase())
        .chain(std::iter::once(c.source.to_ascii_lowercase()))
        .collect();
    if !f.exclude_sources.is_empty() {
        let excluded: HashSet<String> = f
            .exclude_sources
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();
        // Drop when every edition is on an excluded store (multi-store works
        // still pass if any included store remains).
        if !hit_sources.is_empty() && hit_sources.iter().all(|s| excluded.contains(s)) {
            return false;
        }
    }
    if !f.sources.is_empty() {
        let ok = f.sources.iter().any(|want| {
            let w = want.trim().to_ascii_lowercase();
            !w.is_empty() && hit_sources.contains(&w)
        });
        if !ok {
            return false;
        }
    }
    true
}

/// Hard language include: known other languages fail; unknown/missing passes
/// (GraphicAudio and other sparse sources may still omit language).
fn language_passes(hit_language: Option<&str>, allowed: &[String]) -> bool {
    let allowed: HashSet<String> = allowed
        .iter()
        .filter_map(|s| bookclerk_source::normalize_language(s))
        .collect();
    if allowed.is_empty() {
        return true;
    }
    match hit_language.and_then(bookclerk_source::normalize_language) {
        None => true,
        Some(code) => allowed.contains(&code),
    }
}

fn list_matches_any(hay: Option<&str>, needles: &[String]) -> bool {
    let parts: Vec<String> = hay
        .unwrap_or("")
        .split([',', ';', '&', '/'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    if parts.is_empty() {
        return false;
    }
    needles.iter().any(|n| {
        let n = n.trim().to_ascii_lowercase();
        !n.is_empty() && parts.iter().any(|p| p == &n || p.contains(&n))
    })
}

fn cursor_fingerprint(
    q: &str,
    sort: &str,
    sort_dir: &str,
    field: Option<&str>,
    lang: Option<&str>,
    filters: &CatalogSearchFilters,
) -> String {
    let payload = serde_json::json!({
        "q": q,
        "sort": sort,
        "sort_dir": sort_dir,
        "field": field,
        "lang": lang,
        "authors": sorted_norm(&filters.authors),
        "narrators": sorted_norm(&filters.narrators),
        "series": sorted_norm(&filters.series),
        "genres": sorted_norm(&filters.genres),
        "sources": sorted_norm(&filters.sources),
        "exclude_sources": sorted_norm(&filters.exclude_sources),
        "languages": sorted_norm(&filters.languages),
        "exclude_narrators": sorted_norm(&filters.exclude_narrators),
        "min_rating": filters.min_rating,
        "min_length_minutes": filters.min_length_minutes,
        "max_length_minutes": filters.max_length_minutes,
    });
    hex::encode(serde_json::to_vec(&payload).unwrap_or_default())
}

fn sorted_norm(v: &[String]) -> Vec<String> {
    let mut out: Vec<String> = v
        .iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect();
    out.sort();
    out.dedup();
    out
}

fn encode_cursor(c: &SearchCursorV1) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    cursor_cache().insert(id.clone(), c.clone());
    format!("{CURSOR_TOKEN_PREFIX}{id}")
}

fn decode_cursor(raw: &str) -> Option<SearchCursorV1> {
    let raw = raw.trim();
    if let Some(id) = raw.strip_prefix(CURSOR_TOKEN_PREFIX) {
        return cursor_cache().get(id);
    }
    // Legacy clients may still send hex-encoded cursors from before the
    // server-side token change; accept them when they decode cleanly.
    let bytes = hex::decode(raw).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn upsert_hit(map: &mut HashMap<String, StorefrontCandidate>, mut hit: StorefrontCandidate) {
    push_edition(
        &mut hit.store_editions,
        StoreEdition::new(&hit.source, &hit.product_id),
    );
    if let Some(isbn) = hit.isbn.as_mut() {
        let n = bookclerk_enrich::canonicalize_isbn(isbn);
        if !n.is_empty() {
            *isbn = n;
        }
    }

    // Fast path: exact hard bibliographic key already in the map.
    if let Some(hard) = hard_work_key(hit.asin.as_deref(), hit.isbn.as_deref()) {
        if let Some(mut existing) = map.remove(&hard) {
            merge_candidate_metadata(&mut existing, &hit);
            let new_key = work_map_key(
                existing.asin.as_deref(),
                existing.isbn.as_deref(),
                &existing.title,
                existing.authors.as_deref(),
                Some(existing.source.as_str()),
                Some(existing.product_id.as_str()),
            );
            map.insert(new_key, existing);
            return;
        }
    }

    let match_key = map.iter().find_map(|(key, existing)| {
        if identities_match(
            WorkIdentity::new(
                hit.asin.as_deref(),
                hit.isbn.as_deref(),
                &hit.title,
                hit.authors.as_deref(),
            )
            .with_series(hit.series.as_deref())
            .with_series_index(hit.series_index.as_deref()),
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
        merge_candidate_metadata(&mut existing, &hit);
        let new_key = work_map_key(
            existing.asin.as_deref(),
            existing.isbn.as_deref(),
            &existing.title,
            existing.authors.as_deref(),
            Some(existing.source.as_str()),
            Some(existing.product_id.as_str()),
        );
        map.insert(new_key, existing);
        return;
    }

    let key = work_map_key(
        hit.asin.as_deref(),
        hit.isbn.as_deref(),
        &hit.title,
        hit.authors.as_deref(),
        Some(hit.source.as_str()),
        Some(hit.product_id.as_str()),
    );
    map.insert(key, hit);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(narrators: Option<&str>, authors: Option<&str>) -> StorefrontCandidate {
        StorefrontCandidate {
            source: "audible".into(),
            product_id: "B00TEST".into(),
            title: "Adventure".into(),
            authors: authors.map(str::to_string),
            narrators: narrators.map(str::to_string),
            series: None,
            series_index: None,
            asin: Some("B00TEST".into()),
            isbn: None,
            cover_url: None,
            seed_categories: None,
            origin: "test".into(),
            seed_title: None,
            store_editions: vec![StoreEdition::new("audible", "B00TEST")],
            subtitle: None,
            description: None,
            publisher: None,
            length_minutes: None,
            published_at: None,
            categories: Some("Fantasy; Adventure".into()),
            language: Some("english".into()),
            price_cents: None,
            currency: None,
            price_label: None,
            rating_overall: None,
            rating_count: None,
            is_abridged: None,
            audible_rank: Some(0),
        }
    }

    #[test]
    fn exclude_virtual_voice_substring() {
        let f = CatalogSearchFilters {
            exclude_narrators: vec![String::from("Virtual Voice")],
            ..Default::default()
        };
        assert!(!passes_filters(
            &cand(Some("Virtual Voice"), Some("Author")),
            &f
        ));
        assert!(passes_filters(
            &cand(Some("Ray Porter"), Some("Author")),
            &f
        ));
    }

    #[test]
    fn include_genre_and_author() {
        let f = CatalogSearchFilters {
            authors: vec![String::from("Andy Weir")],
            genres: vec![String::from("Adventure")],
            ..Default::default()
        };
        assert!(passes_filters(&cand(None, Some("Andy Weir")), &f));
        assert!(!passes_filters(&cand(None, Some("Someone Else")), &f));
    }

    #[test]
    fn language_include_drops_other_keeps_unknown() {
        let f = CatalogSearchFilters {
            languages: vec![String::from("en")],
            ..Default::default()
        };
        let mut en = cand(None, Some("Author"));
        en.language = Some("english".into());
        let mut zh = cand(None, Some("Author"));
        zh.language = Some("chinese".into());
        let mut es = cand(None, Some("Author"));
        es.language = Some("Spanish".into());
        let mut unknown = cand(None, Some("Author"));
        unknown.language = None;
        assert!(passes_filters(&en, &f));
        assert!(!passes_filters(&zh, &f));
        assert!(!passes_filters(&es, &f));
        assert!(passes_filters(&unknown, &f));
    }

    #[test]
    fn min_rating_drops_low_keeps_unknown() {
        let f = CatalogSearchFilters {
            min_rating: Some(4.0),
            ..Default::default()
        };
        let mut high = cand(None, Some("Author"));
        high.rating_overall = Some(4.5);
        let mut low = cand(None, Some("Author"));
        low.rating_overall = Some(3.2);
        let mut unknown = cand(None, Some("Author"));
        unknown.rating_overall = None;
        assert!(passes_filters(&high, &f));
        assert!(!passes_filters(&low, &f));
        assert!(passes_filters(&unknown, &f));
    }

    #[test]
    fn length_bounds_keep_unknown() {
        let f = CatalogSearchFilters {
            min_length_minutes: Some(360),
            max_length_minutes: Some(720),
            ..Default::default()
        };
        let mut ok = cand(None, Some("Author"));
        ok.length_minutes = Some(480);
        let mut short = cand(None, Some("Author"));
        short.length_minutes = Some(120);
        let mut long = cand(None, Some("Author"));
        long.length_minutes = Some(900);
        let mut unknown = cand(None, Some("Author"));
        unknown.length_minutes = None;
        assert!(passes_filters(&ok, &f));
        assert!(!passes_filters(&short, &f));
        assert!(!passes_filters(&long, &f));
        assert!(passes_filters(&unknown, &f));
    }

    #[test]
    fn exclude_sources_drops_when_all_editions_excluded() {
        let f = CatalogSearchFilters {
            exclude_sources: vec![String::from("audible")],
            ..Default::default()
        };
        assert!(!passes_filters(&cand(None, Some("Author")), &f));
        let mut multi = cand(None, Some("Author"));
        multi.store_editions = vec![
            StoreEdition::new("audible", "B00A"),
            StoreEdition::new("chirp", "chirp-1"),
        ];
        assert!(passes_filters(&multi, &f));
    }

    #[test]
    fn rank_price_missing_last_asc() {
        let mut rows = vec![
            ("a".into(), {
                let mut c = cand(None, Some("A"));
                c.product_id = "a".into();
                c.price_cents = Some(999);
                c
            }),
            ("b".into(), {
                let mut c = cand(None, Some("B"));
                c.product_id = "b".into();
                c.price_cents = None;
                c
            }),
            ("c".into(), {
                let mut c = cand(None, Some("C"));
                c.product_id = "c".into();
                c.price_cents = Some(499);
                c
            }),
        ];
        rank_candidates(
            &mut rows,
            CatalogSearchSort::Price,
            CatalogSortDir::Asc,
            "q",
            "en",
        );
        assert_eq!(rows[0].0, "c");
        assert_eq!(rows[1].0, "a");
        assert_eq!(rows[2].0, "b");
    }

    #[test]
    fn bayesian_rating_prefers_high_volume_over_tiny_perfect() {
        let tiny = bayesian_rating_score(5.0, Some(16));
        let popular = bayesian_rating_score(4.8, Some(149_000));
        assert!(
            popular > tiny,
            "expected 4.8/149k ({popular}) > 5.0/16 ({tiny})"
        );
    }

    #[test]
    fn rank_rating_desc_prefers_volume_backed_score() {
        let mut rows = vec![
            ("tiny".into(), {
                let mut c = cand(None, Some("A"));
                c.product_id = "tiny".into();
                c.title = "Tiny Perfect".into();
                c.rating_overall = Some(5.0);
                c.rating_count = Some(16);
                c
            }),
            ("popular".into(), {
                let mut c = cand(None, Some("B"));
                c.product_id = "popular".into();
                c.title = "Popular Near Perfect".into();
                c.rating_overall = Some(4.8);
                c.rating_count = Some(149_000);
                c
            }),
        ];
        rank_candidates(
            &mut rows,
            CatalogSearchSort::Rating,
            CatalogSortDir::Desc,
            "q",
            "en",
        );
        assert_eq!(rows[0].0, "popular");
        assert_eq!(rows[1].0, "tiny");
    }

    #[test]
    fn rank_popularity_asc_least_first_desc_most_first() {
        let make = |id: &str, rank: u32| -> (String, StorefrontCandidate) {
            let mut c = cand(None, Some(id));
            c.product_id = id.into();
            c.title = id.into();
            c.audible_rank = Some(rank);
            (id.into(), c)
        };
        let mut asc = vec![make("top", 0), make("mid", 1), make("low", 5)];
        rank_candidates(
            &mut asc,
            CatalogSearchSort::Popularity,
            CatalogSortDir::Asc,
            "q",
            "en",
        );
        assert_eq!(
            asc.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["low", "mid", "top"]
        );

        let mut desc = vec![make("top", 0), make("mid", 1), make("low", 5)];
        rank_candidates(
            &mut desc,
            CatalogSearchSort::Popularity,
            CatalogSortDir::Desc,
            "q",
            "en",
        );
        assert_eq!(
            desc.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            vec!["top", "mid", "low"]
        );
    }

    #[test]
    fn cursor_roundtrip() {
        let c = SearchCursorV1 {
            v: 1,
            fp: String::from("abc"),
            pages: HashMap::from([(String::from("audible"), 2), (String::from("chirp"), 1)]),
            exhausted: HashSet::from([String::from("graphicaudio")]),
            pending: vec![PendingSearchRow {
                work_key: String::from("asin:B00TEST"),
                candidate: StorefrontCandidate {
                    source: String::from("audible"),
                    product_id: String::from("B00TEST"),
                    title: String::from("Pending Title"),
                    authors: None,
                    narrators: None,
                    series: None,
                    series_index: None,
                    asin: Some(String::from("B00TEST")),
                    isbn: None,
                    cover_url: None,
                    seed_categories: None,
                    origin: String::from("test"),
                    seed_title: None,
                    store_editions: Vec::new(),
                    subtitle: None,
                    description: None,
                    publisher: None,
                    length_minutes: None,
                    published_at: None,
                    categories: None,
                    language: None,
                    price_cents: None,
                    currency: None,
                    price_label: None,
                    rating_overall: None,
                    rating_count: None,
                    is_abridged: None,
                    audible_rank: None,
                },
            }],
        };
        let enc = encode_cursor(&c);
        assert!(
            enc.starts_with(CURSOR_TOKEN_PREFIX),
            "wire cursor must be a short opaque token, got len={}",
            enc.len()
        );
        assert!(
            enc.len() < 80,
            "opaque cursor must stay well under URI limits, got {}",
            enc.len()
        );
        let dec = decode_cursor(&enc).expect("decode");
        assert_eq!(dec.v, 1);
        assert_eq!(dec.fp, "abc");
        assert_eq!(dec.pages.get("audible"), Some(&2));
        assert!(dec.exhausted.contains("graphicaudio"));
        assert_eq!(dec.pending.len(), 1);
        assert_eq!(dec.pending[0].work_key, "asin:B00TEST");

        // Legacy hex cursors still decode for in-flight sessions.
        let legacy = hex::encode(serde_json::to_vec(&c).unwrap());
        let legacy_dec = decode_cursor(&legacy).expect("legacy hex");
        assert_eq!(legacy_dec.pending[0].work_key, "asin:B00TEST");
    }
}
