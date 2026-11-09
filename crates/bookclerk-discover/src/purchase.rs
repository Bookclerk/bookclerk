//! Suggest storefronts where a title might be purchased (with live pricing).

use std::collections::HashMap;
use std::time::Duration;

use bookclerk_enrich::normalize_region;
use bookclerk_source::{PurchaseHintOpts, SourcePurchaseHint, SourceRegistry};

use crate::error::Result;
use crate::identity::works_match;

/// A purchase / catalog availability hint (optionally priced at view time).
#[derive(
    Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq, utoipa::ToSchema,
)]
pub struct PurchaseHint {
    /// Canonical storefront / plugin id (`audible`, `libro`, `chirp`, …).
    #[schema(example = "libro")]
    pub source: String,
    /// Storefront-native product id (ASIN, ISBN, UUID, …).
    #[schema(example = "9780441478125")]
    pub product_id: String,
    /// Display title as shown on the storefront or library card.
    #[schema(example = "The Left Hand of Darkness")]
    pub title: Option<String>,
    /// Absolute HTTPS URL.
    #[schema(example = "https://libro.fm/audiobooks/9780441478125")]
    pub url: Option<String>,
    /// Primary / best known sell price in minor units (prefer member when dual).
    /// `0` = free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 1499)]
    pub price_cents: Option<i64>,
    /// ISO 4217 currency code for price fields (`USD`, `EUR`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "USD")]
    pub currency: Option<String>,
    /// Display string from the store (`$2.99`, `FREE`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "$14.99")]
    pub price_label: Option<String>,
    /// Non-member / list price in integer cents when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 2999)]
    pub list_price_cents: Option<i64>,
    /// Human-readable list price from the storefront.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "$29.99")]
    pub list_price_label: Option<String>,
    /// Member / credit price in integer cents when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = 1499)]
    pub member_price_cents: Option<i64>,
    /// Human-readable member price from the storefront.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "$14.99")]
    pub member_price_label: Option<String>,
}

impl PurchaseHint {
    /// URL-only catalog link (no price yet).
    ///
    /// # Arguments
    ///
    /// * `source` - Storefront id or filesystem source path, depending on call site.
    /// * `product_id` - Storefront-native product id.
    /// * `title` - Display title.
    /// * `url` - Absolute URL being checked or opened.
    ///
    /// # Returns
    ///
    /// Updated `Self` for chaining.
    #[must_use]
    pub fn link(
        source: impl Into<String>,
        product_id: impl Into<String>,
        title: Option<String>,
        url: Option<String>,
    ) -> Self {
        Self {
            source: source.into(),
            product_id: product_id.into(),
            title,
            url,
            price_cents: None,
            currency: None,
            price_label: None,
            list_price_cents: None,
            list_price_label: None,
            member_price_cents: None,
            member_price_label: None,
        }
    }

    #[cfg(test)]
    fn with_price(mut self, cents: i64, currency: &str, label: impl Into<String>) -> Self {
        self.price_cents = Some(cents.max(0));
        self.currency = Some(currency.to_string());
        self.price_label = Some(label.into());
        self
    }

    #[cfg(test)]
    fn with_dual_price(
        mut self,
        member_cents: i64,
        list_cents: i64,
        currency: &str,
        member_label: impl Into<String>,
        list_label: impl Into<String>,
    ) -> Self {
        let member_label = member_label.into();
        let list_label = list_label.into();
        self.currency = Some(currency.to_string());
        self.member_price_cents = Some(member_cents.max(0));
        self.member_price_label = Some(member_label.clone());
        self.list_price_cents = Some(list_cents.max(0));
        self.list_price_label = Some(list_label);
        // Primary / “best known” mirrors store dual-price helpers (member first).
        self.price_cents = Some(member_cents.max(0));
        self.price_label = Some(member_label);
        self
    }

    /// Maps a storefront [`SourcePurchaseHint`] onto a discover [`PurchaseHint`], decoding HTML entities.
    fn from_source_hint(source: &str, hint: SourcePurchaseHint) -> Self {
        let hint = hint.decode_html_entities();
        Self {
            source: source.to_string(),
            product_id: hint.product_id,
            title: hint.title,
            url: hint.url,
            price_cents: hint.price_cents,
            currency: hint.currency,
            price_label: hint.price_label,
            list_price_cents: hint.list_price_cents,
            list_price_label: hint.list_price_label,
            member_price_cents: hint.member_price_cents,
            member_price_label: hint.member_price_label,
        }
    }
}

/// Inputs for view-time catalog + pricing lookup.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PurchaseHintsQuery {
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
    /// Storefront id of the edition that produced this card.
    #[schema(example = "libro")]
    pub candidate_source: Option<String>,
    /// Storefront product id of the edition that produced this card.
    #[schema(example = "9780441478125")]
    pub candidate_product_id: Option<String>,
    /// Known storefront editions already on the recommendation card.
    #[serde(default)]
    pub store_editions: Vec<crate::identity::StoreEdition>,
    /// Marketplace / region code (`us`, `uk`, …) for catalog lookups.
    #[schema(example = "us")]
    pub region: Option<String>,
    /// Storefronts the caller has linked accounts for. Treated as **member**
    /// pricing when picking `best`; other stores are compared at list /
    /// non-member price. Every catalog match is still returned in `hints`.
    #[serde(default)]
    #[schema(example = json!(["libro"]))]
    pub preferred_sources: Vec<String>,
}

/// Priced catalog matches for one title, sorted best-first.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PurchaseHintsResponse {
    /// Resolved purchase hints for this query.
    pub hints: Vec<PurchaseHint>,
    /// Lowest-priced hint (or first catalog hit when no prices resolved).
    pub best: Option<PurchaseHint>,
    /// True when the batch deadline cut this lookup short: the hints are the
    /// query's trusted seeds alone, so a client should not keep the answer
    /// and asks again later. Omitted (false) for a finished lookup.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schema(example = false)]
    pub incomplete: bool,
}

/// Look up purchase links via registered
/// [`bookclerk_source::ContentSource::purchase_hint`] (no prices).
///
/// Call [`resolve_purchase_hints`] for multi-store + live pricing.
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `title` - Display title.
/// * `author` - String `author` for this call.
/// * `asin` - Optional Audible / Amazon ASIN.
/// * `isbn` - Optional ISBN (any punctuation; normalized internally).
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// On success, the inner `Vec<PurchaseHint>` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn purchase_hints_for(
    registry: &SourceRegistry,
    title: &str,
    author: Option<&str>,
    asin: Option<&str>,
    isbn: Option<&str>,
    region: &str,
) -> Result<Vec<PurchaseHint>> {
    let mut hints = Vec::new();
    let region = normalize_region(region);

    let opts = PurchaseHintOpts {
        product_id: None,
        title: Some(title.to_string()).filter(|s| !s.trim().is_empty()),
        authors: author.map(str::to_string),
        asin: asin.map(str::to_string),
        isbn: isbn.map(str::to_string),
        region,
        with_price: false,
    };
    append_registry_hints(registry, &mut hints, &opts, None).await;

    Ok(hints)
}

/// Seed a deterministic storefront URL from a known candidate (no remote I/O).
///
/// # Arguments
///
/// * `source` - Storefront id or filesystem source path, depending on call site.
/// * `product_id` - Storefront-native product id.
/// * `title` - Display title.
/// * `region` - Marketplace / region code (`us`, `uk`, …).
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
#[must_use]
pub fn seed_purchase_hint(
    source: &str,
    product_id: &str,
    title: Option<String>,
    region: &str,
) -> Option<PurchaseHint> {
    let pid = product_id.trim();
    if pid.is_empty() {
        return None;
    }
    let region = normalize_region(region);
    match source.trim().to_ascii_lowercase().as_str() {
        "audible" => Some(audible_hint(pid, title, &region)),
        "libro" => Some(libro_hint(pid, title)),
        "chirp" => Some(PurchaseHint::link(
            "chirp",
            pid,
            title,
            Some(format!("https://www.chirpbooks.com/audiobooks/{pid}")),
        )),
        "graphicaudio" => Some(PurchaseHint::link(
            "graphicaudio",
            pid,
            title,
            Some(format!(
                "https://www.graphicaudio.net/catalog/product/view/id/{pid}"
            )),
        )),
        _ => None,
    }
}

/// Process-wide 10-minute TTL cache (256 entries) for priced purchase-hint responses.
fn purchase_hints_cache() -> &'static crate::ttl_cache::TtlCache<PurchaseHintsResponse> {
    use std::sync::OnceLock;
    use std::time::Duration;
    static CACHE: OnceLock<crate::ttl_cache::TtlCache<PurchaseHintsResponse>> = OnceLock::new();
    CACHE.get_or_init(|| crate::ttl_cache::TtlCache::new(Duration::from_secs(10 * 60), 256))
}

/// Cache key from region, title, identifiers, known editions, and preferred storefronts.
fn purchase_hints_cache_key(query: &PurchaseHintsQuery, region: &str) -> String {
    let mut editions: Vec<_> = query
        .store_editions
        .iter()
        .map(|e| format!("{}:{}", e.source.to_ascii_lowercase(), e.product_id))
        .collect();
    editions.sort();
    let preferred: Vec<_> = query
        .preferred_sources
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();
    crate::ttl_cache::cache_key(&[
        "purchase-hints",
        region,
        query.title.as_str(),
        query.authors.as_deref().unwrap_or(""),
        query.asin.as_deref().unwrap_or(""),
        query.isbn.as_deref().unwrap_or(""),
        query.candidate_source.as_deref().unwrap_or(""),
        query.candidate_product_id.as_deref().unwrap_or(""),
        &editions.join(","),
        &preferred.join(","),
    ])
}

/// Resolve every catalog match and attach live prices (view-time).
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `query` - Query vector or free-text search string.
///
/// # Returns
///
/// On success, the inner `PurchaseHintsResponse` value.
///
/// # Errors
///
/// Returns an error when the underlying I/O, parse, network, or store operation fails.
pub async fn resolve_purchase_hints(
    registry: &SourceRegistry,
    query: &PurchaseHintsQuery,
) -> Result<PurchaseHintsResponse> {
    let region = normalize_region(query.region.as_deref().unwrap_or("us"));
    let cache_key = purchase_hints_cache_key(query, &region);
    if let Some(cached) = purchase_hints_cache().get(&cache_key) {
        return Ok(cached);
    }
    let response = resolve_purchase_hints_uncached(registry, query, &region).await?;
    purchase_hints_cache().insert(cache_key, response.clone());
    Ok(response)
}

/// Live catalog + pricing lookup: seed trusted ids, then merge title-matched storefront hints.
async fn resolve_purchase_hints_uncached(
    registry: &SourceRegistry,
    query: &PurchaseHintsQuery,
    region: &str,
) -> Result<PurchaseHintsResponse> {
    let region = region.to_string();
    let title = query.title.trim();
    let authors = query
        .authors
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let asin = query
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let isbn = query
        .isbn
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());

    let mut hints: Vec<PurchaseHint> = Vec::new();

    // Only seed Audible from ASIN/product id. Libro ISBN≠catalog membership
    // (Audible exclusives often carry a bibliographic ISBN that 404s on
    // libro.fm) — Libro rows come only from a live purchase_hint that verified
    // the product page. Chirp/GA Magento ids stay soft (title-matched).
    for ed in &query.store_editions {
        if !seed_source_is_trusted(&ed.source) {
            continue;
        }
        if let Some(seed) =
            seed_purchase_hint(&ed.source, &ed.product_id, Some(title.to_string()), &region)
        {
            push_dedupe(&mut hints, seed);
        }
    }

    if let (Some(source), Some(pid)) = (
        query.candidate_source.as_deref(),
        query.candidate_product_id.as_deref(),
    ) {
        if seed_source_is_trusted(source) {
            if let Some(seed) = seed_purchase_hint(source, pid, Some(title.to_string()), &region) {
                push_dedupe(&mut hints, seed);
            }
        }
        // Candidate ASIN may differ from product id.
        if source.eq_ignore_ascii_case("audible") {
            // already seeded
        } else if let Some(a) = asin {
            push_dedupe(
                &mut hints,
                audible_hint(a, Some(title.to_string()), &region),
            );
        }
    } else if let Some(a) = asin {
        push_dedupe(
            &mut hints,
            audible_hint(a, Some(title.to_string()), &region),
        );
    }

    // One parallel priced pass across storefronts (replaces the old URL-only
    // expand + sequential priced expand, which doubled wall time).
    let priced_opts = PurchaseHintOpts {
        product_id: query.candidate_product_id.clone(),
        title: Some(title.to_string()).filter(|s| !s.is_empty()),
        authors: authors.map(str::to_string),
        asin: asin.map(str::to_string),
        isbn: isbn.map(str::to_string),
        region: region.clone(),
        with_price: true,
    };
    append_registry_hints(
        registry,
        &mut hints,
        &priced_opts,
        query.candidate_source.as_deref(),
    )
    .await;

    // Price known editions that the registry pass did not already cover.
    let mut edition_set = tokio::task::JoinSet::new();
    for ed in &query.store_editions {
        let source_id = ed.source.trim().to_ascii_lowercase();
        let product_id = ed.product_id.trim().to_string();
        if product_id.is_empty() {
            continue;
        }
        if hints.iter().any(|h| {
            h.source.eq_ignore_ascii_case(&source_id)
                && h.product_id == product_id
                && h.price_cents.is_some()
        }) {
            continue;
        }
        let Some(source) = registry.get(&ed.source) else {
            continue;
        };
        let opts = PurchaseHintOpts {
            product_id: Some(product_id),
            title: Some(title.to_string()).filter(|s| !s.is_empty()),
            authors: authors.map(str::to_string),
            asin: asin.map(str::to_string),
            isbn: isbn.map(str::to_string),
            region: region.clone(),
            with_price: true,
        };
        let trusted = seed_source_is_trusted(&ed.source);
        let q_title = title.to_string();
        let q_authors = authors.map(str::to_string);
        edition_set.spawn(async move {
            let mapped = match source.purchase_hint(&opts).await {
                Ok(Some(hint)) => PurchaseHint::from_source_hint(source.id(), hint),
                Ok(None) => return None,
                Err(err) => {
                    tracing::debug!(source = %source.id(), error = %err, "purchase_hint failed");
                    return None;
                }
            };
            if trusted || catalog_hint_matches_query(&q_title, q_authors.as_deref(), &mapped) {
                Some(mapped)
            } else {
                tracing::debug!(
                    source = %mapped.source,
                    product_id = %mapped.product_id,
                    hint_title = ?mapped.title,
                    query_title = %q_title,
                    "dropping stored edition purchase hint that failed title match"
                );
                None
            }
        });
    }
    while let Some(joined) = edition_set.join_next().await {
        if let Ok(Some(mapped)) = joined {
            merge_or_push(&mut hints, mapped);
        }
    }

    let preferred = preferred_source_set(&query.preferred_sources);
    sort_hints_for_display(&mut hints, &preferred);
    let best = best_purchase_hint_preferring(&hints, &preferred)
        .map(|h| best_hint_for_caller(h, &preferred));
    Ok(PurchaseHintsResponse {
        hints,
        best,
        incomplete: false,
    })
}

/// Pick the lowest-priced hint (ties keep earlier order). Unpriced sort after priced.
///
/// # Arguments
///
/// * `hints` - Purchase hints to pick from.
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
#[must_use]
pub fn best_purchase_hint(hints: &[PurchaseHint]) -> Option<&PurchaseHint> {
    hints.iter().min_by(|a, b| cmp_hint_price(a, b))
}

/// Pick the offer the caller would actually pay least for.
///
/// Linked storefronts (`preferred`) are compared at **member** price; every
/// other store is compared at **list / non-member** price. That way a Libro-only
/// member still sees Audible on the shelf when Audible’s non-member price beats
/// Libro’s member price. When nothing is linked, all stores are compared as
/// non-members. Hints with no usable price for that role sort last.
///
/// # Arguments
///
/// * `hints` - Purchase hints to pick from.
/// * `preferred` - String `preferred` for this call.
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
#[must_use]
pub fn best_purchase_hint_preferring<'a>(
    hints: &'a [PurchaseHint],
    preferred: &std::collections::HashSet<String>,
) -> Option<&'a PurchaseHint> {
    hints.iter().min_by(|a, b| {
        let a_pref = source_is_preferred(&a.source, preferred);
        let b_pref = source_is_preferred(&b.source, preferred);
        match (
            effective_price_cents(a, a_pref),
            effective_price_cents(b, b_pref),
        ) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    })
}

/// True when the caller has a linked account for this storefront (member pricing).
fn source_is_preferred(source: &str, preferred: &std::collections::HashSet<String>) -> bool {
    !preferred.is_empty() && preferred.contains(&source.to_ascii_lowercase())
}

/// Price used when ranking `best` for a caller.
///
/// - Linked (member): member → primary → list
/// - Unlinked / no accounts: list → primary (when not a dual member-only primary) → member
pub(crate) fn effective_price_cents(hint: &PurchaseHint, as_member: bool) -> Option<i64> {
    if as_member {
        return hint
            .member_price_cents
            .or(hint.price_cents)
            .or(hint.list_price_cents)
            .map(|c| c.max(0));
    }
    if let Some(list) = hint.list_price_cents {
        return Some(list.max(0));
    }
    // Single-price stores (Chirp, …): `price_cents` is what anyone pays.
    // Dual-price rows always set list when member is present (see store helpers).
    hint.price_cents
        .or(hint.member_price_cents)
        .map(|c| c.max(0))
}

/// Shape `best` so shelf/UI primary fields match what the caller would pay.
fn best_hint_for_caller(
    hint: &PurchaseHint,
    preferred: &std::collections::HashSet<String>,
) -> PurchaseHint {
    let as_member = source_is_preferred(&hint.source, preferred);
    let mut out = hint.clone();
    if as_member {
        if let Some(member) = out.member_price_cents {
            out.price_cents = Some(member);
            if let Some(label) = out.member_price_label.clone() {
                out.price_label = Some(label);
            }
        }
        return out;
    }
    // Non-member: surface list as primary and drop member so shelf formatters
    // do not prefer a coupon the caller cannot use.
    if let Some(list) = out.list_price_cents {
        out.price_cents = Some(list);
        out.price_label = out
            .list_price_label
            .clone()
            .or_else(|| out.price_label.clone());
        out.member_price_cents = None;
        out.member_price_label = None;
    }
    out
}

/// Longest a batch waits for its lookups before answering with what has
/// resolved. Under the daemon's 25 s route budget, so a slow store never
/// turns a page of cards into one failed request.
pub const BATCH_DEADLINE: Duration = Duration::from_secs(10);

/// Resolve many purchase-hint queries at once (order preserved).
///
/// Every query starts together, at most `max_concurrent` at a time. The
/// whole batch answers within [`BATCH_DEADLINE`]: a query still waiting
/// then answers with its trusted seeds alone (no prices) and `incomplete`
/// set, so the page shows a link rather than nothing, keeps no copy, and
/// asks again later.
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `queries` - Batch of purchase-hint queries.
/// * `max_concurrent` - Lookups in flight at once (clamped to 1–8).
///
/// # Returns
///
/// One result per query, in request order.
pub async fn resolve_purchase_hints_batch(
    registry: &SourceRegistry,
    queries: &[PurchaseHintsQuery],
    max_concurrent: usize,
) -> Vec<Result<PurchaseHintsResponse>> {
    let limit = max_concurrent.clamp(1, 8);
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(limit));
    let deadline = tokio::time::Instant::now() + BATCH_DEADLINE;
    let mut set = tokio::task::JoinSet::new();
    for (offset, q) in queries.iter().enumerate() {
        let q = q.clone();
        let registry = registry.clone();
        let gate = std::sync::Arc::clone(&gate);
        set.spawn(async move {
            let resolved = async {
                let _permit = gate.acquire().await.ok()?;
                Some(resolve_purchase_hints(&registry, &q).await)
            };
            let outcome = match tokio::time::timeout_at(deadline, resolved).await {
                Ok(Some(result)) => result,
                // Out of time (or the gate closed): links only, priced later.
                Ok(None) | Err(_) => Ok(seeded_response(&q)),
            };
            (offset, outcome)
        });
    }
    let mut slot: Vec<Option<Result<PurchaseHintsResponse>>> =
        (0..queries.len()).map(|_| None).collect();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((offset, result)) => slot[offset] = Some(result),
            Err(err) => tracing::debug!(error = %err, "purchase-hints batch task failed"),
        }
    }
    slot.into_iter()
        .map(|result| {
            result.unwrap_or_else(|| {
                Err(crate::error::DiscoverError::message(
                    "purchase-hints batch task cancelled",
                ))
            })
        })
        .collect()
}

/// The hints one query can answer without asking any store: its trusted
/// store seeds, unpriced, marked `incomplete` so the client asks again.
fn seeded_response(query: &PurchaseHintsQuery) -> PurchaseHintsResponse {
    let region = normalize_region(query.region.as_deref().unwrap_or("us"));
    let title = query.title.trim();
    let mut hints = Vec::new();
    for ed in &query.store_editions {
        if !seed_source_is_trusted(&ed.source) {
            continue;
        }
        if let Some(seed) =
            seed_purchase_hint(&ed.source, &ed.product_id, Some(title.to_string()), &region)
        {
            push_dedupe(&mut hints, seed);
        }
    }
    if let (Some(source), Some(pid)) = (
        query.candidate_source.as_deref(),
        query.candidate_product_id.as_deref(),
    ) {
        if seed_source_is_trusted(source) {
            if let Some(seed) = seed_purchase_hint(source, pid, Some(title.to_string()), &region) {
                push_dedupe(&mut hints, seed);
            }
        }
    }
    if let Some(asin) = query
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
    {
        push_dedupe(
            &mut hints,
            audible_hint(asin, Some(title.to_string()), &region),
        );
    }
    let preferred = preferred_source_set(&query.preferred_sources);
    sort_hints_for_display(&mut hints, &preferred);
    PurchaseHintsResponse {
        hints,
        best: None,
        incomplete: true,
    }
}

/// Timeouts in a row after which a store's price lookups are skipped.
pub const BREAKER_TRIP_TIMEOUTS: u32 = 3;

/// How long a tripped store is skipped before one lookup tries it again.
pub const BREAKER_OPEN_FOR: Duration = Duration::from_secs(5 * 60);

/// One store's recent price-lookup timeouts.
#[derive(Debug, Clone, Copy, Default)]
struct StoreBreaker {
    /// Timeouts since the store last answered.
    timeouts: u32,
    /// When the breaker tripped, while it is open.
    tripped_at: Option<std::time::Instant>,
}

impl StoreBreaker {
    /// True when the store should be skipped right now.
    fn is_open(&self, now: std::time::Instant) -> bool {
        self.tripped_at
            .is_some_and(|at| now.duration_since(at) < BREAKER_OPEN_FOR)
    }

    /// Records a timeout; trips after [`BREAKER_TRIP_TIMEOUTS`] in a row.
    fn timed_out(&mut self, now: std::time::Instant) {
        self.timeouts = self.timeouts.saturating_add(1);
        if self.timeouts >= BREAKER_TRIP_TIMEOUTS {
            self.tripped_at = Some(now);
            self.timeouts = 0;
        }
    }

    /// Records an answer (or a failure that was not a timeout): the store
    /// is reachable, so the breaker closes.
    fn answered(&mut self) {
        *self = Self::default();
    }
}

/// Process-wide breaker per store id.
fn store_breakers() -> &'static std::sync::Mutex<HashMap<String, StoreBreaker>> {
    use std::sync::OnceLock;
    static BREAKERS: OnceLock<std::sync::Mutex<HashMap<String, StoreBreaker>>> = OnceLock::new();
    BREAKERS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// True when `store`'s price lookups are skipped because it kept timing out.
fn store_breaker_open(store: &str, now: std::time::Instant) -> bool {
    store_breakers()
        .lock()
        .map(|map| map.get(store).is_some_and(|b| b.is_open(now)))
        .unwrap_or(false)
}

/// Records how `store`'s lookup ended.
fn note_store_outcome(store: &str, timed_out: bool, now: std::time::Instant) {
    if let Ok(mut map) = store_breakers().lock() {
        let breaker = map.entry(store.to_string()).or_default();
        if timed_out {
            breaker.timed_out(now);
            if breaker.is_open(now) {
                tracing::info!(%store, "price lookups paused after repeated timeouts");
            }
        } else {
            breaker.answered();
        }
    }
}

/// Queries every registered source (8s timeout) and merges title-matched or trusted-id hints.
async fn append_registry_hints(
    registry: &SourceRegistry,
    hints: &mut Vec<PurchaseHint>,
    opts: &PurchaseHintOpts,
    prefer_source: Option<&str>,
) {
    const PER_SOURCE_HINT_TIMEOUT: Duration = Duration::from_secs(8);
    let prefer = prefer_source.map(str::to_string);
    let now = std::time::Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for source in registry.all() {
        let mut call_opts = opts.clone();
        let id = source.id().to_string();
        // A store that kept timing out is left alone for a while.
        if store_breaker_open(&id, now) {
            tracing::debug!(source = %id, "purchase_hint skipped: store paused");
            continue;
        }
        // When the candidate is for this source, prefer its product id.
        if prefer
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case(&id))
        {
            // product_id already set from query
        } else if prefer.is_some() {
            // Candidate belongs to another store — still search by title.
            call_opts.product_id = None;
        }
        set.spawn(async move {
            let outcome =
                tokio::time::timeout(PER_SOURCE_HINT_TIMEOUT, source.purchase_hint(&call_opts))
                    .await;
            (id, call_opts, outcome)
        });
    }

    while let Some(joined) = set.join_next().await {
        let (id, call_opts, outcome) = match joined {
            Ok(v) => v,
            Err(err) => {
                tracing::debug!(error = %err, "purchase_hint task join failed");
                continue;
            }
        };
        note_store_outcome(&id, outcome.is_err(), std::time::Instant::now());
        let hint = match outcome {
            Ok(Ok(Some(hint))) => hint,
            Ok(Ok(None)) => continue,
            Ok(Err(err)) => {
                tracing::debug!(source = %id, error = %err, "purchase_hint failed");
                continue;
            }
            Err(_) => {
                tracing::debug!(source = %id, "purchase_hint timed out");
                continue;
            }
        };
        let mapped = PurchaseHint::from_source_hint(&id, hint);
        // Audible ASINs are trusted. Libro and soft Magento/Chirp ids need a
        // title match — ISBN alone is not proof of Libro membership.
        let trusted_product = seed_source_is_trusted(&id)
            && call_opts
                .product_id
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .is_some();
        if trusted_product
            || catalog_hint_matches_query(
                call_opts.title.as_deref().unwrap_or(""),
                call_opts.authors.as_deref(),
                &mapped,
            )
        {
            merge_or_push(hints, mapped);
        } else {
            tracing::debug!(
                source = %id,
                product_id = %mapped.product_id,
                hint_title = ?mapped.title,
                "dropping registry purchase hint that failed title match"
            );
        }
    }
}

/// Storefronts that may be URL-seeded from product id alone (no live check).
///
/// Libro is intentionally excluded: `libro.fm/audiobooks/{isbn}` 404s for many
/// ISBNs that appear on Audible-only titles.
///
/// # Arguments
///
/// * `source` - Storefront id or filesystem source path, depending on call site.
///
/// # Returns
///
/// `true` when the predicate holds.
pub(crate) fn seed_source_is_trusted(source: &str) -> bool {
    matches!(source.trim().to_ascii_lowercase().as_str(), "audible")
}

/// Whether a title-searched purchase hint is bibliographic enough to keep.
fn catalog_hint_matches_query(
    query_title: &str,
    query_authors: Option<&str>,
    hint: &PurchaseHint,
) -> bool {
    let query_title = query_title.trim();
    if query_title.is_empty() {
        return false;
    }
    let Some(hint_title) = hint
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    else {
        // No title on the hit — cannot validate Magento/Chirp first-rank noise.
        return false;
    };
    works_match(query_title, query_authors, hint_title, None)
}

/// Merges price/url/title into an existing source+id row, or appends via [`push_dedupe`].
fn merge_or_push(hints: &mut Vec<PurchaseHint>, hint: PurchaseHint) {
    let key = (
        hint.source.to_ascii_lowercase(),
        hint.product_id.to_ascii_lowercase(),
    );
    if let Some(existing) = hints.iter_mut().find(|h| {
        h.source.eq_ignore_ascii_case(&key.0) && h.product_id.eq_ignore_ascii_case(&key.1)
    }) {
        if existing.price_cents.is_none() && hint.price_cents.is_some() {
            existing.price_cents = hint.price_cents;
            existing.currency = hint.currency.clone();
            existing.price_label = hint.price_label.clone();
        }
        if existing.list_price_cents.is_none() && hint.list_price_cents.is_some() {
            existing.list_price_cents = hint.list_price_cents;
            existing.list_price_label = hint.list_price_label.clone();
        }
        if existing.member_price_cents.is_none() && hint.member_price_cents.is_some() {
            existing.member_price_cents = hint.member_price_cents;
            existing.member_price_label = hint.member_price_label.clone();
        }
        if existing.url.is_none() {
            existing.url = hint.url;
        }
        if existing.title.is_none() {
            existing.title = hint.title;
        }
        return;
    }
    push_dedupe(hints, hint);
}

/// Lowercased, non-empty storefront ids the caller has linked accounts for.
fn preferred_source_set(raw: &[String]) -> std::collections::HashSet<String> {
    raw.iter()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Orders hints by primary `price_cents`; priced rows sort before unpriced ones.
fn cmp_hint_price(a: &PurchaseHint, b: &PurchaseHint) -> std::cmp::Ordering {
    match (a.price_cents, b.price_cents) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Sorts linked storefronts first, then by primary price.
fn sort_hints_for_display(
    hints: &mut [PurchaseHint],
    preferred: &std::collections::HashSet<String>,
) {
    hints.sort_by(|a, b| {
        let a_pref = preferred.contains(&a.source.to_ascii_lowercase());
        let b_pref = preferred.contains(&b.source.to_ascii_lowercase());
        match (a_pref, b_pref) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => cmp_hint_price(a, b),
        }
    });
}

/// Appends a hint unless the same source+id or another row for that source already exists.
fn push_dedupe(hints: &mut Vec<PurchaseHint>, hint: PurchaseHint) {
    let key = (
        hint.source.to_ascii_lowercase(),
        hint.product_id.to_ascii_lowercase(),
    );
    if hints
        .iter()
        .any(|h| h.source.eq_ignore_ascii_case(&key.0) && h.product_id.eq_ignore_ascii_case(&key.1))
    {
        return;
    }
    // One row per source: keep first (usually the proposing / known id).
    if hints
        .iter()
        .any(|h| h.source.eq_ignore_ascii_case(&hint.source))
    {
        return;
    }
    hints.push(hint);
}

/// URL-only Audible catalog link for an uppercase ASIN on the regional `audible.*` host.
fn audible_hint(asin: &str, title: Option<String>, region: &str) -> PurchaseHint {
    let asin = asin.to_ascii_uppercase();
    PurchaseHint::link(
        "audible",
        asin.clone(),
        title,
        Some(format!(
            "https://www.audible{}/pd/{}",
            region_host_suffix(region),
            asin
        )),
    )
}

/// URL-only Libro.fm catalog link; ISBN alone is not proof of catalog membership.
fn libro_hint(isbn_or_slug: &str, title: Option<String>) -> PurchaseHint {
    PurchaseHint::link(
        "libro",
        isbn_or_slug,
        title,
        Some(format!("https://libro.fm/audiobooks/{isbn_or_slug}")),
    )
}

/// Audible hostname suffix for a marketplace code (`us` → `.com`, `uk` → `.co.uk`).
fn region_host_suffix(region: &str) -> &'static str {
    match region {
        "uk" => ".co.uk",
        "ca" => ".ca",
        "au" => ".com.au",
        "fr" => ".fr",
        "de" => ".de",
        "jp" => ".co.jp",
        "it" => ".it",
        "in" => ".in",
        "es" => ".es",
        _ => ".com",
    }
}

/// Parse `$12.34` / `12.34` / `FREE` into cents.
///
/// # Arguments
///
/// * `raw` - String `raw` for this call.
///
/// # Returns
///
/// `Some(...)` when found / applicable; otherwise `None`.
#[must_use]
pub fn parse_money_label_to_cents(raw: &str) -> Option<i64> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if s.eq_ignore_ascii_case("free") || s.eq_ignore_ascii_case("free!") {
        return Some(0);
    }
    let mut num = String::new();
    let mut seen_dot = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            num.push(c);
        } else if c == '.' && !seen_dot {
            num.push('.');
            seen_dot = true;
        } else if c == ',' {
            continue;
        } else if !num.is_empty() {
            break;
        }
    }
    if num.is_empty() {
        return None;
    }
    let amount: f64 = num.parse().ok()?;
    Some((amount * 100.0).round() as i64)
}

/// Formats integer cents into a storefront-style money label for `currency`.
///
/// # Arguments
///
/// * `cents` - Integer cents to format.
/// * `currency` - ISO 4217 currency code.
///
/// # Returns
///
/// String result for this operation.
#[must_use]
pub fn format_money_label(cents: i64, currency: &str) -> String {
    if cents <= 0 {
        return String::from("FREE");
    }
    let major = cents / 100;
    let minor = (cents % 100).unsigned_abs();
    match currency.to_ascii_uppercase().as_str() {
        "USD" | "" => format!("${major}.{minor:02}"),
        "GBP" => format!("£{major}.{minor:02}"),
        "EUR" => format!("€{major}.{minor:02}"),
        other => format!("{major}.{minor:02} {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_store_breaker_trips_after_repeated_timeouts_and_closes_on_an_answer() {
        let start = std::time::Instant::now();
        let mut breaker = StoreBreaker::default();
        for _ in 1..BREAKER_TRIP_TIMEOUTS {
            breaker.timed_out(start);
            assert!(!breaker.is_open(start));
        }
        breaker.timed_out(start);
        assert!(breaker.is_open(start));
        assert!(breaker.is_open(start + BREAKER_OPEN_FOR / 2));
        // Past the pause the store is tried again.
        assert!(!breaker.is_open(start + BREAKER_OPEN_FOR));
        // Any answer, even an error, closes it.
        breaker.timed_out(start);
        breaker.answered();
        assert!(!breaker.is_open(start));
        assert_eq!(breaker.timeouts, 0);

        note_store_outcome("test-breaker-store", true, start);
        assert!(!store_breaker_open("test-breaker-store", start));
        for _ in 0..BREAKER_TRIP_TIMEOUTS {
            note_store_outcome("test-breaker-store", true, start);
        }
        assert!(store_breaker_open("test-breaker-store", start));
        note_store_outcome("test-breaker-store", false, start);
        assert!(!store_breaker_open("test-breaker-store", start));
    }

    #[test]
    fn a_timed_out_query_answers_with_its_trusted_links_only() {
        let query = PurchaseHintsQuery {
            title: "Dune".into(),
            authors: Some("Frank Herbert".into()),
            asin: Some("B002V1OF70".into()),
            isbn: Some("9780441172719".into()),
            candidate_source: Some("libro".into()),
            candidate_product_id: Some("9780441172719".into()),
            store_editions: vec![
                crate::identity::StoreEdition::new("audible", "B002V1OF70"),
                crate::identity::StoreEdition::new("chirp", "12345"),
            ],
            region: None,
            preferred_sources: Vec::new(),
        };
        let response = seeded_response(&query);
        // Only Audible is trusted from an id; Libro and Chirp need a live
        // title match, and nothing was priced.
        let sources: Vec<&str> = response.hints.iter().map(|h| h.source.as_str()).collect();
        assert_eq!(sources, vec!["audible"]);
        assert!(response.hints[0].price_cents.is_none());
        assert!(response.best.is_none());
        assert!(response.incomplete, "the client must not keep this answer");
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["incomplete"], serde_json::Value::Bool(true));
    }

    #[test]
    fn audible_links_use_the_marketplace_host() {
        for (region, host) in [
            ("us", "audible.com"),
            ("uk", "audible.co.uk"),
            ("ca", "audible.ca"),
            ("au", "audible.com.au"),
            ("fr", "audible.fr"),
            ("de", "audible.de"),
            ("jp", "audible.co.jp"),
            ("it", "audible.it"),
            ("in", "audible.in"),
            ("es", "audible.es"),
        ] {
            assert_eq!(
                audible_hint("b0abc", None, region).url.as_deref(),
                Some(format!("https://www.{host}/pd/B0ABC").as_str()),
                "{region}"
            );
        }
    }

    #[test]
    fn merge_or_push_fills_an_existing_row_and_appends_new_sources() {
        let mut hints = vec![PurchaseHint::link("audible", "B0ABC", None, None)];
        merge_or_push(
            &mut hints,
            PurchaseHint::link(
                "Audible",
                "b0abc",
                Some("Title".into()),
                Some("https://www.audible.com/pd/B0ABC".into()),
            )
            .with_price(1999, "USD", "$19.99"),
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].price_cents, Some(1999));
        assert_eq!(hints[0].currency.as_deref(), Some("USD"));
        assert_eq!(hints[0].title.as_deref(), Some("Title"));
        assert_eq!(
            hints[0].url.as_deref(),
            Some("https://www.audible.com/pd/B0ABC")
        );
        merge_or_push(&mut hints, PurchaseHint::link("libro", "978", None, None));
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[1].source, "libro");
    }

    #[test]
    fn money_label_parsing() {
        assert_eq!(parse_money_label_to_cents("$2.99"), Some(299));
        assert_eq!(parse_money_label_to_cents("FREE"), Some(0));
        assert_eq!(parse_money_label_to_cents("12.5"), Some(1250));
        assert_eq!(format_money_label(299, "USD"), "$2.99");
        assert_eq!(format_money_label(0, "USD"), "FREE");
    }

    #[test]
    fn best_hint_prefers_lowest_price() {
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None).with_price(1999, "USD", "$19.99"),
            PurchaseHint::link("chirp", "C", None, None).with_price(299, "USD", "$2.99"),
            PurchaseHint::link("libro", "L", None, None),
        ];
        let best = best_purchase_hint(&hints).unwrap();
        assert_eq!(best.source, "chirp");
        assert_eq!(best.price_cents, Some(299));
    }

    #[test]
    fn best_hint_uses_member_price_on_linked_store() {
        // Libro linked at $14.99 member; Audible unlinked list $31.62 (member $5.99).
        // Non-member Audible is worse than Libro member → Libro wins.
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None)
                .with_dual_price(599, 3162, "USD", "$5.99", "$31.62"),
            PurchaseHint::link("libro", "L", None, None)
                .with_dual_price(1499, 3254, "USD", "$14.99", "$32.54"),
        ];
        let preferred = std::collections::HashSet::from([String::from("libro")]);
        let best = best_purchase_hint_preferring(&hints, &preferred).unwrap();
        assert_eq!(best.source, "libro");
    }

    #[test]
    fn cheaper_nonmember_beats_linked_member() {
        // Libro linked at $14.99 member; Audible unlinked list $9.99 → Audible.
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None)
                .with_dual_price(599, 999, "USD", "$5.99", "$9.99"),
            PurchaseHint::link("libro", "L", None, None)
                .with_dual_price(1499, 3254, "USD", "$14.99", "$32.54"),
        ];
        let preferred = std::collections::HashSet::from([String::from("libro")]);
        let best = best_purchase_hint_preferring(&hints, &preferred).unwrap();
        assert_eq!(best.source, "audible");
        let display = best_hint_for_caller(best, &preferred);
        assert_eq!(display.price_cents, Some(999));
        assert_eq!(display.price_label.as_deref(), Some("$9.99"));
        assert!(display.member_price_cents.is_none());
    }

    #[test]
    fn linked_member_beats_other_linked_list() {
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None)
                .with_dual_price(599, 3162, "USD", "$5.99", "$31.62"),
            PurchaseHint::link("libro", "L", None, None)
                .with_dual_price(1499, 3254, "USD", "$14.99", "$32.54"),
        ];
        let preferred =
            std::collections::HashSet::from([String::from("audible"), String::from("libro")]);
        let best = best_purchase_hint_preferring(&hints, &preferred).unwrap();
        assert_eq!(best.source, "audible");
    }

    #[test]
    fn unpriced_linked_does_not_beat_cheaper_foreign() {
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None),
            PurchaseHint::link("chirp", "C", None, None).with_price(299, "USD", "$2.99"),
        ];
        let preferred = std::collections::HashSet::from([String::from("audible")]);
        let best = best_purchase_hint_preferring(&hints, &preferred).unwrap();
        assert_eq!(best.source, "chirp");
    }

    #[test]
    fn no_preferred_compares_as_nonmember() {
        let hints = vec![
            PurchaseHint::link("audible", "A", None, None)
                .with_dual_price(599, 3162, "USD", "$5.99", "$31.62"),
            PurchaseHint::link("chirp", "C", None, None).with_price(2899, "USD", "$28.99"),
        ];
        let preferred = std::collections::HashSet::new();
        let best = best_purchase_hint_preferring(&hints, &preferred).unwrap();
        // Chirp $28.99 < Audible list $31.62
        assert_eq!(best.source, "chirp");
    }

    #[test]
    fn soft_storefronts_are_not_trusted_seeds() {
        assert!(seed_source_is_trusted("audible"));
        assert!(!seed_source_is_trusted("libro"));
        assert!(!seed_source_is_trusted("graphicaudio"));
        assert!(!seed_source_is_trusted("chirp"));
    }

    #[test]
    fn catalog_hint_rejects_unrelated_ga_title() {
        let hint = PurchaseHint::link(
            "graphicaudio",
            "123",
            Some(String::from("Red Rising Saga 1: Red Rising 1 of 2")),
            Some(String::from(
                "https://www.graphicaudio.net/catalog/product/view/id/123",
            )),
        );
        assert!(!catalog_hint_matches_query(
            "Ashes of Man",
            Some("Christopher Ruocchio"),
            &hint
        ));
    }
}
