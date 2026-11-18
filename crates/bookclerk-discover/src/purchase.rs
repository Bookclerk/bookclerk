//! Suggest storefronts where a title might be purchased (with live pricing).

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use bookclerk_enrich::normalize_region;
use bookclerk_source::{
    PriceLookup, PricedAnswer, PurchaseHintOpts, SourcePurchaseHint, SourceRegistry,
};

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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct PurchaseHintsResponse {
    /// Resolved purchase hints for this query.
    pub hints: Vec<PurchaseHint>,
    /// Lowest-priced hint (or first catalog hit when no prices resolved).
    pub best: Option<PurchaseHint>,
    /// True when the batch deadline cut this lookup short (the hints are the
    /// query's trusted seeds alone), or when the hints could not be checked
    /// against the card's recording in time: a client should not keep the
    /// answer and asks again later. Omitted (false) for a finished lookup.
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
        lookup: PriceLookup::Unspecified,
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
/// One query through [`resolve_purchase_hints_batch`], so a single card
/// is priced the same way as a page of them: the same lookups, the same
/// evaluation of title-search records, and the same cache.
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `library` - The library whose listing pool places title-search
///   records; `None` keeps only records that carry the card's ISBN or ASIN.
/// * `query` - The card to price.
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
    library: Option<&bookclerk_library::LibraryStore>,
    query: &PurchaseHintsQuery,
) -> Result<PurchaseHintsResponse> {
    resolve_purchase_hints_batch(registry, library, std::slice::from_ref(query), 1)
        .await
        .into_iter()
        .next()
        .unwrap_or_else(|| Ok(PurchaseHintsResponse::default()))
}

/// Drops live purchase hints that sell another recording than the card's.
///
/// A store answers a price lookup by searching its catalog, and the host can
/// only check the answer's title, so a store whose first hit is another
/// recording of the book (a full-cast dramatisation for a narration) would
/// price the card with it. The shared listing pool knows which expression
/// (recording) each listed product is: when it knows the card's own
/// editions, a hint whose product the pool placed only in other expressions
/// is dropped, and `best` is picked again. A hint for a product the pool
/// never listed stays, as does every hint when the pool knows none of the
/// card's editions.
///
/// # Arguments
///
/// * `library` - The library whose listing pool is read.
/// * `registry` - Storefronts, to name each hint's store by its PluginKey.
/// * `query` - The card's lookup: its candidate edition and store editions.
/// * `response` - The resolved hints, filtered in place.
///
/// # Errors
///
/// Returns the library read error.
pub async fn retain_card_recording(
    library: &bookclerk_library::LibraryStore,
    registry: &SourceRegistry,
    query: &PurchaseHintsQuery,
    response: &mut PurchaseHintsResponse,
) -> Result<()> {
    let key_of = |source: &str| -> Option<String> {
        registry.get(source).map(|s| s.plugin_key().to_string())
    };
    let card: Vec<(String, String)> = card_editions(query)
        .into_iter()
        .filter_map(|(source, product)| Some((key_of(&source)?, product)))
        .collect();
    if card.is_empty() {
        return Ok(());
    }
    let hints: Vec<Option<(String, String)>> = response
        .hints
        .iter()
        .map(|h| Some((key_of(&h.source)?, h.product_id.clone())))
        .collect();
    let mut pairs = card.clone();
    pairs.extend(hints.iter().flatten().cloned());
    let expressions = library.listing_expressions(&pairs).await?;
    let card_expressions: std::collections::BTreeSet<&String> = card
        .iter()
        .filter_map(|pair| expressions.get(pair))
        .flatten()
        .collect();
    if card_expressions.is_empty() {
        return Ok(());
    }
    let keep: Vec<bool> = hints
        .iter()
        .map(|pair| {
            pair.as_ref()
                .and_then(|pair| expressions.get(pair))
                .is_none_or(|own| own.iter().any(|x| card_expressions.contains(x)))
        })
        .collect();
    if keep.iter().all(|k| *k) {
        return Ok(());
    }
    let mut kept = keep.into_iter();
    response.hints.retain(|hint| {
        let keep = kept.next().unwrap_or(true);
        if !keep {
            tracing::debug!(
                source = %hint.source,
                product_id = %hint.product_id,
                "dropping purchase hint for another recording than the card's"
            );
        }
        keep
    });
    let preferred = preferred_source_set(&query.preferred_sources);
    response.best = best_purchase_hint_preferring(&response.hints, &preferred)
        .map(|h| best_hint_for_caller(h, &preferred));
    Ok(())
}

/// The card's own store editions, as `(source, product_id)`: its candidate
/// edition first, then its other store editions, without repeats. A card
/// that names no store edition but an ASIN is that Audible product.
fn card_editions(query: &PurchaseHintsQuery) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut push = |source: &str, product: &str| {
        let (source, product) = (source.trim(), product.trim());
        if source.is_empty() || product.is_empty() {
            return;
        }
        if !out
            .iter()
            .any(|(s, p)| s.eq_ignore_ascii_case(source) && p == product)
        {
            out.push((source.to_string(), product.to_string()));
        }
    };
    if let (Some(source), Some(product)) = (
        query.candidate_source.as_deref(),
        query.candidate_product_id.as_deref(),
    ) {
        push(source, product);
    }
    for edition in &query.store_editions {
        push(&edition.source, &edition.product_id);
    }
    // A card named only by its ASIN is that Audible product.
    let named = query.candidate_product_id.is_some() || !query.store_editions.is_empty();
    if !named {
        if let Some(asin) = query.asin.as_deref() {
            push("audible", asin);
        }
    }
    out
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
/// Queries answered from the cache are done at once. For the rest, every
/// store is asked **once**, through [`bookclerk_source::ContentSource::purchase_hints`],
/// for all of them together, so a store whose catalog prices many products
/// per request makes one request for a page of cards. The whole batch
/// answers within [`BATCH_DEADLINE`]: a store still working then counts as
/// timed out. A query no store answered with a hint (timed out, paused, or
/// never tried within a guest's own budget) carries its trusted seeds alone
/// (no prices) and `incomplete` set: it is not cached, so the page shows a
/// link rather than nothing, keeps no copy, and asks again later. A store
/// that asks to slow down
/// ([`bookclerk_source::SourceError::RateLimited`]) is left alone for the
/// time it said (see [`RATE_LIMIT_FIRST_PAUSE`]).
///
/// Each store is asked the strongest way it supports: by the product it already sells, by the card's exact ISBN or ASIN when it
/// declared that lookup, else by title. A title search's records are
/// stored in the listing pool and kept only for the card's recording.
///
/// # Arguments
///
/// * `registry` - Configured content-source or integration registry.
/// * `library` - The library whose listing pool places title-search
///   records, keeping those of the card's recording; `None` keeps only
///   records that carry the card's ISBN or ASIN.
/// * `queries` - Batch of purchase-hint queries.
/// * `max_concurrent` - Unused since each store is asked once; kept for
///   the route's call shape.
///
/// # Returns
///
/// One result per query, in request order.
pub async fn resolve_purchase_hints_batch(
    registry: &SourceRegistry,
    library: Option<&bookclerk_library::LibraryStore>,
    queries: &[PurchaseHintsQuery],
    max_concurrent: usize,
) -> Vec<Result<PurchaseHintsResponse>> {
    let _ = max_concurrent;
    let deadline = tokio::time::Instant::now() + BATCH_DEADLINE;
    let regions: Vec<String> = queries
        .iter()
        .map(|q| normalize_region(q.region.as_deref().unwrap_or("us")))
        .collect();
    let mut out: Vec<Option<PurchaseHintsResponse>> = queries
        .iter()
        .zip(&regions)
        .map(|(q, region)| purchase_hints_cache().get(&purchase_hints_cache_key(q, region)))
        .collect();
    let pending: Vec<usize> = (0..queries.len()).filter(|i| out[*i].is_none()).collect();
    if pending.is_empty() {
        return out.into_iter().map(|r| Ok(r.unwrap_or_default())).collect();
    }

    // Each pending query starts from its trusted seeds.
    let mut hints: Vec<Vec<PurchaseHint>> = queries
        .iter()
        .zip(&regions)
        .map(|(q, region)| seeded_hints(q, region))
        .collect();
    // Queries at least one store answered with a hint, so caching them
    // hides no price. The rest answer `incomplete` and are not cached.
    let mut answered: Vec<bool> = vec![false; queries.len()];

    let now = std::time::Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for source in registry.all() {
        let id = source.id().to_string();
        if store_breaker_open(&id, now) {
            tracing::debug!(source = %id, "purchase_hints skipped: store paused");
            continue;
        }
        let opts: Vec<PurchaseHintOpts> = pending
            .iter()
            .map(|&i| store_query_opts(&queries[i], &regions[i], source.as_ref()))
            .collect();
        set.spawn(async move {
            let outcome =
                tokio::time::timeout_at(deadline, source.purchase_hint_answers(&opts)).await;
            (id, source.plugin_key().to_string(), opts, outcome)
        });
    }
    let mut stores: Vec<StoreAnswers> = Vec::new();
    while let Some(joined) = set.join_next().await {
        let (id, plugin_key, opts, outcome) = match joined {
            Ok(v) => v,
            Err(err) => {
                tracing::debug!(error = %err, "purchase_hints task join failed");
                continue;
            }
        };
        let now = std::time::Instant::now();
        let answers = match outcome {
            Ok(Ok(answers)) => {
                note_store_outcome(&id, false, now);
                answers
            }
            Ok(Err(bookclerk_source::SourceError::RateLimited(wait))) => {
                note_store_rate_limit(&id, wait, now);
                continue;
            }
            Ok(Err(err)) => {
                note_store_outcome(&id, false, now);
                tracing::debug!(source = %id, error = %err, "purchase_hints failed");
                continue;
            }
            Err(_) => {
                note_store_outcome(&id, true, now);
                tracing::debug!(source = %id, "purchase_hints timed out");
                continue;
            }
        };
        stores.push(StoreAnswers {
            id,
            plugin_key,
            opts,
            answers,
        });
    }

    // Title-search records go to the listing pool, which says which
    // recording each one is, within what is left of the batch's time.
    let placed = match library {
        Some(library) => {
            let place = place_records(library, registry, queries, &pending, &regions, &stores);
            match tokio::time::timeout_at(deadline, place).await {
                Ok(Ok(placed)) => Some(placed),
                Ok(Err(err)) => {
                    tracing::debug!(error = %err, "purchase-hint records not placed");
                    None
                }
                Err(_) => {
                    tracing::debug!("purchase-hint records not placed before the deadline");
                    None
                }
            }
        }
        None => None,
    };

    for store in &stores {
        for ((&i, query_opts), answer) in pending.iter().zip(&store.opts).zip(&store.answers) {
            // `None` is no proof the store lacks the title: a guest that
            // ran out of its own time budget answers `None` for every title
            // it never tried. Only a hint or a record counts as an answer.
            if answer.hint.is_none() && answer.records.is_empty() {
                continue;
            }
            answered[i] = true;
            let pool = placed.as_ref().map(|p| p.view(i, &store.plugin_key));
            for hint in evaluate_answer(&store.id, &queries[i], query_opts, answer, pool) {
                merge_or_push(&mut hints[i], hint);
            }
        }
    }

    for &i in &pending {
        let query = &queries[i];
        let preferred = preferred_source_set(&query.preferred_sources);
        let mut found = std::mem::take(&mut hints[i]);
        sort_hints_for_display(&mut found, &preferred);
        let best = best_purchase_hint_preferring(&found, &preferred)
            .map(|h| best_hint_for_caller(h, &preferred));
        let response = PurchaseHintsResponse {
            hints: found,
            best,
            incomplete: !answered[i],
        };
        if answered[i] {
            purchase_hints_cache().insert(
                purchase_hints_cache_key(query, &regions[i]),
                response.clone(),
            );
        }
        out[i] = Some(response);
    }
    out.into_iter().map(|r| Ok(r.unwrap_or_default())).collect()
}

/// What one store answered for a batch: its queries and answers, in order.
struct StoreAnswers {
    /// Store (plugin) id the hints are shown under.
    id: String,
    /// The store's PluginKey, which names its listings in the pool.
    plugin_key: String,
    /// What the store was asked, one per pending query.
    opts: Vec<PurchaseHintOpts>,
    /// Its answers, one per query in order.
    answers: Vec<PricedAnswer>,
}

/// What the listing pool says about a batch's cards and records.
#[derive(Debug, Default)]
struct Placement {
    /// The recordings (expression ids) of each card's own editions, by
    /// query index; a card the pool knows nothing of is left out.
    cards: HashMap<usize, BTreeSet<String>>,
    /// The recordings the pool placed each `(plugin_key, product_id)` in.
    expressions: HashMap<(String, String), BTreeSet<String>>,
}

/// One card's view of a [`Placement`], for one store's answer.
#[derive(Clone, Copy)]
struct PoolView<'a> {
    /// The card's recordings; `None` when the pool knows none of its editions.
    card: Option<&'a BTreeSet<String>>,
    /// Every placed product's recordings.
    expressions: &'a HashMap<(String, String), BTreeSet<String>>,
    /// The answering store's PluginKey.
    plugin_key: &'a str,
}

impl Placement {
    /// Query `index`'s view, for an answer from the store `plugin_key`.
    ///
    /// # Arguments
    ///
    /// * `index` - The query's position in the batch.
    /// * `plugin_key` - The answering store's PluginKey.
    fn view<'a>(&'a self, index: usize, plugin_key: &'a str) -> PoolView<'a> {
        PoolView {
            card: self.cards.get(&index),
            expressions: &self.expressions,
            plugin_key,
        }
    }
}

impl PoolView<'_> {
    /// Whether the pool placed `product_id` (from this view's store) in
    /// the card's recording. `false` when the pool knows the card's
    /// recording or the product not.
    ///
    /// # Arguments
    ///
    /// * `product_id` - The store's product id for the record.
    fn same_recording(&self, product_id: &str) -> bool {
        let Some(card) = self.card else {
            return false;
        };
        self.expressions
            .get(&(self.plugin_key.to_string(), product_id.to_string()))
            .is_some_and(|own| own.iter().any(|x| card.contains(x)))
    }
}

/// Stores every title-search record of a batch in the listing pool and
/// reads which recording each record and each card's own editions are.
///
/// # Arguments
///
/// * `library` - The library holding the pool.
/// * `registry` - Maps each card edition's store to its PluginKey.
/// * `queries` - The batch's queries.
/// * `pending` - The indexes of the queries the stores were asked about.
/// * `regions` - Each query's marketplace, by index.
/// * `stores` - What each store answered.
///
/// # Errors
///
/// Returns the library error from storing the records or reading the pool.
async fn place_records(
    library: &bookclerk_library::LibraryStore,
    registry: &SourceRegistry,
    queries: &[PurchaseHintsQuery],
    pending: &[usize],
    regions: &[String],
    stores: &[StoreAnswers],
) -> Result<Placement> {
    let now = chrono::Utc::now();
    let mut listings = Vec::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for store in stores {
        for (&i, answer) in pending.iter().zip(&store.answers) {
            for priced in &answer.records {
                let region = regions.get(i).map_or("us", String::as_str);
                listings.push(crate::pool::catalog_listing(
                    &store.plugin_key,
                    region,
                    &priced.record,
                    now,
                ));
                pairs.push((store.plugin_key.clone(), priced.record.product_id.clone()));
            }
        }
    }
    if listings.is_empty() {
        return Ok(Placement::default());
    }
    library.record_catalog_listings(&listings).await?;
    let key_of = |source: &str| registry.get(source).map(|s| s.plugin_key().to_string());
    let card_pairs: Vec<(usize, Vec<(String, String)>)> = pending
        .iter()
        .filter_map(|&i| queries.get(i).map(|q| (i, q)))
        .map(|(i, query)| {
            let own = card_editions(query)
                .into_iter()
                .filter_map(|(source, product)| Some((key_of(&source)?, product)))
                .collect();
            (i, own)
        })
        .collect();
    pairs.extend(card_pairs.iter().flat_map(|(_, own)| own.iter().cloned()));
    pairs.sort();
    pairs.dedup();
    let expressions = library.listing_expressions(&pairs).await?;
    let cards = card_pairs
        .into_iter()
        .filter_map(|(i, own)| {
            let recordings: BTreeSet<String> = own
                .iter()
                .filter_map(|pair| expressions.get(pair))
                .flatten()
                .cloned()
                .collect();
            (!recordings.is_empty()).then_some((i, recordings))
        })
        .collect();
    Ok(Placement { cards, expressions })
}

/// Whether a record carries the card's own ISBN or ASIN.
///
/// # Arguments
///
/// * `record` - A listing a store's title search returned.
/// * `query` - The card being priced.
fn record_names_card(record: &bookclerk_source::CatalogHit, query: &PurchaseHintsQuery) -> bool {
    let isbn = query
        .isbn
        .as_deref()
        .map(bookclerk_enrich::canonicalize_isbn)
        .filter(|s| !s.is_empty());
    let asin = query
        .asin
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_uppercase);
    record.identifiers.iter().any(|id| match id.scheme {
        bookclerk_source::records::IdentifierScheme::Isbn => isbn
            .as_deref()
            .is_some_and(|isbn| bookclerk_enrich::canonicalize_isbn(&id.value) == isbn),
        bookclerk_source::records::IdentifierScheme::Asin => asin
            .as_deref()
            .is_some_and(|asin| id.value.trim().eq_ignore_ascii_case(asin)),
        // A card names no Open Library id to compare.
        bookclerk_source::records::IdentifierScheme::OpenLibraryEdition
        | bookclerk_source::records::IdentifierScheme::OpenLibraryWork => false,
    })
}

/// The hints one store's answer adds to a card.
///
/// - Records (a title search's listings): each one that carries the card's
///   ISBN or ASIN, or that the pool placed in the card's recording. The
///   store's own pick is not used, and titles are never compared.
/// - An exact ISBN or ASIN lookup the store declared: its hint.
/// - A bare hint for the product the card already names at a trusted
///   store: that hint.
/// - Any other bare hint (a store that returns no records): kept when its
///   title and authors match the card's, as before records existed.
///
/// # Arguments
///
/// * `store` - The store's id, which the hints are shown under.
/// * `query` - The card being priced.
/// * `opts` - What the store was asked.
/// * `answer` - What it answered.
/// * `pool` - Where the listing pool placed the card and the records;
///   `None` when there is no pool or it could not be read in time.
///
/// # Returns
///
/// The hints to merge into the card's response.
fn evaluate_answer(
    store: &str,
    query: &PurchaseHintsQuery,
    opts: &PurchaseHintOpts,
    answer: &PricedAnswer,
    pool: Option<PoolView<'_>>,
) -> Vec<PurchaseHint> {
    if !answer.records.is_empty() {
        let kept: Vec<PurchaseHint> = answer
            .records
            .iter()
            .filter(|priced| {
                record_names_card(&priced.record, query)
                    || pool.is_some_and(|pool| pool.same_recording(&priced.record.product_id))
            })
            .map(|priced| PurchaseHint::from_source_hint(store, priced.hint.clone()))
            .collect();
        if kept.len() < answer.records.len() {
            tracing::debug!(
                source = %store,
                kept = kept.len(),
                returned = answer.records.len(),
                "dropping title-search records for another recording than the card's"
            );
        }
        return kept;
    }
    let Some(hint) = answer.hint.clone() else {
        return Vec::new();
    };
    let mapped = PurchaseHint::from_source_hint(store, hint);
    let exact = matches!(opts.lookup, PriceLookup::Isbn | PriceLookup::Asin);
    let trusted_product = seed_source_is_trusted(store)
        && opts
            .product_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|s| !s.is_empty());
    if exact
        || trusted_product
        || catalog_hint_matches_query(
            opts.title.as_deref().unwrap_or(""),
            opts.authors.as_deref(),
            &mapped,
        )
    {
        vec![mapped]
    } else {
        tracing::debug!(
            source = %store,
            product_id = %mapped.product_id,
            hint_title = ?mapped.title,
            "dropping batch purchase hint that failed title match"
        );
        Vec::new()
    }
}

/// What one store is asked about one query in a batch, the strongest way
/// the store supports.
///
/// 1. The product the query already knows for that store (its own card,
///    or a listed edition): `lookup = product`, so the store needs no search.
/// 2. The card's ISBN, when the store declared exact ISBN lookups.
/// 3. The card's ASIN, when the store declared exact ASIN lookups.
/// 4. Otherwise a title search (`lookup = title`), whose records the host
///    evaluates (see [`evaluate_answer`]).
///
/// Every field the query has is still sent, for guests that predate
/// `lookup`.
///
/// # Arguments
///
/// * `query` - The card.
/// * `region` - The card's marketplace.
/// * `store` - The store being asked.
fn store_query_opts(
    query: &PurchaseHintsQuery,
    region: &str,
    store: &dyn bookclerk_source::ContentSource,
) -> PurchaseHintOpts {
    let id = store.id();
    let own = query
        .candidate_source
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case(id))
        .then(|| query.candidate_product_id.clone())
        .flatten();
    let edition = query
        .store_editions
        .iter()
        .find(|e| e.source.eq_ignore_ascii_case(id))
        .map(|e| e.product_id.clone());
    let clean = |v: Option<&str>| {
        v.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let product_id = clean(own.as_deref()).or_else(|| clean(edition.as_deref()));
    let isbn = clean(query.isbn.as_deref());
    let asin = clean(query.asin.as_deref());
    let title = clean(Some(&query.title));
    let lookup = if product_id.is_some() {
        PriceLookup::Product
    } else if isbn.is_some() && store.answers_price_by_isbn() {
        PriceLookup::Isbn
    } else if asin.is_some() && store.answers_price_by_asin() {
        PriceLookup::Asin
    } else if title.is_some() {
        PriceLookup::Title
    } else {
        PriceLookup::Unspecified
    };
    PurchaseHintOpts {
        product_id,
        title,
        authors: clean(query.authors.as_deref()),
        asin,
        isbn,
        region: region.to_string(),
        with_price: true,
        lookup,
    }
}

/// The trusted store links one query carries before any store is asked.
fn seeded_hints(query: &PurchaseHintsQuery, region: &str) -> Vec<PurchaseHint> {
    let title = query.title.trim();
    let mut hints = Vec::new();
    for ed in &query.store_editions {
        if !seed_source_is_trusted(&ed.source) {
            continue;
        }
        if let Some(seed) =
            seed_purchase_hint(&ed.source, &ed.product_id, Some(title.to_string()), region)
        {
            push_dedupe(&mut hints, seed);
        }
    }
    if let (Some(source), Some(pid)) = (
        query.candidate_source.as_deref(),
        query.candidate_product_id.as_deref(),
    ) {
        if seed_source_is_trusted(source) {
            if let Some(seed) = seed_purchase_hint(source, pid, Some(title.to_string()), region) {
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
            audible_hint(asin, Some(title.to_string()), region),
        );
    }
    hints
}

/// Timeouts in a row after which a store's price lookups are skipped.
pub const BREAKER_TRIP_TIMEOUTS: u32 = 3;

/// How long a tripped store is skipped. After that, lookups ask it again,
/// and [`BREAKER_TRIP_TIMEOUTS`] more timeouts in a row pause it again.
pub const BREAKER_OPEN_FOR: Duration = Duration::from_secs(5 * 60);

/// How long a store that asked to slow down without saying for how long
/// is left alone the first time; each repeat doubles it.
pub const RATE_LIMIT_FIRST_PAUSE: Duration = Duration::from_secs(60);

/// Longest pause a rate limit can grow to by doubling, and the most a
/// store's own `Retry-After` is honored for.
pub const RATE_LIMIT_MAX_PAUSE: Duration = Duration::from_secs(15 * 60);

/// One store's recent price-lookup failures.
#[derive(Debug, Clone, Copy, Default)]
struct StoreBreaker {
    /// Timeouts since the store last answered.
    timeouts: u32,
    /// When the breaker tripped on timeouts, while it is open.
    tripped_at: Option<std::time::Instant>,
    /// Until when the store asked to be left alone.
    paused_until: Option<std::time::Instant>,
    /// Rate limits in a row, each doubling the next default pause.
    rate_limits: u32,
}

impl StoreBreaker {
    /// True when the store should be skipped right now.
    fn is_open(&self, now: std::time::Instant) -> bool {
        self.tripped_at
            .is_some_and(|at| now.duration_since(at) < BREAKER_OPEN_FOR)
            || self.paused_until.is_some_and(|until| now < until)
    }

    /// Records a timeout; trips after [`BREAKER_TRIP_TIMEOUTS`] in a row.
    fn timed_out(&mut self, now: std::time::Instant) {
        self.timeouts = self.timeouts.saturating_add(1);
        if self.timeouts >= BREAKER_TRIP_TIMEOUTS {
            self.tripped_at = Some(now);
            self.timeouts = 0;
        }
    }

    /// Records the store asking to slow down: paused for `wait` when it said
    /// (capped at [`RATE_LIMIT_MAX_PAUSE`]), else for a default that doubles
    /// on each rate limit in a row.
    fn rate_limited(&mut self, now: std::time::Instant, wait: Option<Duration>) {
        let pause = match wait {
            Some(wait) => wait.min(RATE_LIMIT_MAX_PAUSE),
            None => RATE_LIMIT_FIRST_PAUSE
                .saturating_mul(1u32 << self.rate_limits.min(10))
                .min(RATE_LIMIT_MAX_PAUSE),
        };
        self.rate_limits = self.rate_limits.saturating_add(1);
        self.paused_until = Some(now + pause);
    }

    /// Records an answer (or a failure that was not a timeout or a rate
    /// limit): the store is reachable and willing, so the breaker closes.
    fn answered(&mut self) {
        *self = Self::default();
    }
}

/// Records a store asking to slow down.
pub(crate) fn note_store_rate_limit(store: &str, wait: Option<Duration>, now: std::time::Instant) {
    if let Ok(mut map) = store_breakers().lock() {
        let breaker = map.entry(store.to_string()).or_default();
        breaker.rate_limited(now, wait);
        tracing::info!(
            %store,
            pause_secs = breaker.paused_until.map(|u| u.duration_since(now).as_secs()),
            "price lookups paused: the store asked to slow down"
        );
    }
}

/// Process-wide breaker per store id.
fn store_breakers() -> &'static std::sync::Mutex<HashMap<String, StoreBreaker>> {
    use std::sync::OnceLock;
    static BREAKERS: OnceLock<std::sync::Mutex<HashMap<String, StoreBreaker>>> = OnceLock::new();
    BREAKERS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// True when `store`'s price lookups are skipped because it kept timing out.
pub(crate) fn store_breaker_open(store: &str, now: std::time::Instant) -> bool {
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
        let now = std::time::Instant::now();
        let hint = match outcome {
            Ok(Ok(Some(hint))) => {
                note_store_outcome(&id, false, now);
                hint
            }
            Ok(Ok(None)) => {
                note_store_outcome(&id, false, now);
                continue;
            }
            Ok(Err(bookclerk_source::SourceError::RateLimited(wait))) => {
                note_store_rate_limit(&id, wait, now);
                continue;
            }
            Ok(Err(err)) => {
                note_store_outcome(&id, false, now);
                tracing::debug!(source = %id, error = %err, "purchase_hint failed");
                continue;
            }
            Err(_) => {
                note_store_outcome(&id, true, now);
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
    fn a_rate_limit_pauses_a_store_for_what_it_said_or_a_doubling_default() {
        let start = std::time::Instant::now();
        let mut breaker = StoreBreaker::default();
        // The store said 30 s: paused exactly that long.
        breaker.rate_limited(start, Some(Duration::from_secs(30)));
        assert!(breaker.is_open(start + Duration::from_secs(29)));
        assert!(!breaker.is_open(start + Duration::from_secs(30)));
        // Nothing said: the default, doubling on each repeat, capped.
        let mut breaker = StoreBreaker::default();
        breaker.rate_limited(start, None);
        assert_eq!(breaker.paused_until, Some(start + RATE_LIMIT_FIRST_PAUSE));
        breaker.rate_limited(start, None);
        assert_eq!(
            breaker.paused_until,
            Some(start + RATE_LIMIT_FIRST_PAUSE * 2)
        );
        for _ in 0..12 {
            breaker.rate_limited(start, None);
        }
        assert_eq!(breaker.paused_until, Some(start + RATE_LIMIT_MAX_PAUSE));
        // A store's own long wait is capped too, and an answer clears it.
        breaker.rate_limited(start, Some(Duration::from_secs(3_600)));
        assert_eq!(breaker.paused_until, Some(start + RATE_LIMIT_MAX_PAUSE));
        breaker.answered();
        assert!(!breaker.is_open(start));

        note_store_rate_limit("test-limited-store", Some(Duration::from_secs(5)), start);
        assert!(store_breaker_open("test-limited-store", start));
        assert!(!store_breaker_open(
            "test-limited-store",
            start + Duration::from_secs(5)
        ));
    }

    /// How a [`BatchStore`] answers `purchase_hints`.
    #[derive(Clone, Copy)]
    enum Answer {
        /// One hint per query: list 10.00, member 5.00.
        Priced,
        /// HTTP 429 with a wait of 30 s.
        Limited,
        /// A non-rate-limit failure.
        Broken,
        /// Success with `None` for every query, as a guest out of its own
        /// time budget answers for titles it never tried.
        Untried,
    }

    /// A storefront double that counts its batch calls.
    struct BatchStore {
        /// Store id.
        id: &'static str,
        /// What every batch gets.
        answer: Answer,
        /// `purchase_hints` calls received.
        batches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        /// Declares exact ASIN lookups.
        by_asin: bool,
        /// What a title search returns as records (none: a bare hint).
        records: Vec<bookclerk_source::PricedListing>,
    }

    impl BatchStore {
        /// A store answering every batch with `answer`, counting its calls
        /// in `batches`, declaring no identifier lookup and returning no
        /// records.
        fn new(
            id: &'static str,
            answer: Answer,
            batches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        ) -> Self {
            Self {
                id,
                answer,
                batches,
                by_asin: false,
                records: Vec::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl bookclerk_source::ContentSource for BatchStore {
        fn id(&self) -> &str {
            self.id
        }

        fn answers_price_by_asin(&self) -> bool {
            self.by_asin
        }

        async fn purchase_hint_answers(
            &self,
            opts: &[PurchaseHintOpts],
        ) -> bookclerk_source::Result<Vec<PricedAnswer>> {
            if self.records.is_empty() {
                let hints = self.purchase_hints(opts).await?;
                return Ok(hints.into_iter().map(PricedAnswer::from).collect());
            }
            self.batches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(opts
                .iter()
                .map(|o| match o.lookup {
                    PriceLookup::Title => PricedAnswer {
                        // The store's own pick is its first hit.
                        hint: self.records.first().map(|r| r.hint.clone()),
                        records: self.records.clone(),
                    },
                    PriceLookup::Unspecified
                    | PriceLookup::Product
                    | PriceLookup::Isbn
                    | PriceLookup::Asin => PricedAnswer::default(),
                })
                .collect())
        }

        fn portal_auth_mode(&self) -> bookclerk_source::PortalAuthMode {
            bookclerk_source::PortalAuthMode::Password
        }

        fn portal_brand(&self) -> bookclerk_source::SourceBrand {
            bookclerk_source::SourceBrand {
                id: "fake",
                name: "Fake",
                bg: "#000000",
                fg: "#ffffff",
                accent: "#000000",
                icon_url: "",
            }
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Ok(Vec::new())
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Ok(bookclerk_source::ScanSummary::default())
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            _opts: &bookclerk_source::FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            Err(bookclerk_source::SourceError::api("unused"))
        }

        async fn purchase_hint(
            &self,
            opts: &PurchaseHintOpts,
        ) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
            Ok(Some(SourcePurchaseHint {
                product_id: opts.product_id.clone().unwrap_or_default(),
                title: opts.title.clone(),
                ..SourcePurchaseHint::default()
            }))
        }

        async fn purchase_hints(
            &self,
            opts: &[PurchaseHintOpts],
        ) -> bookclerk_source::Result<Vec<Option<SourcePurchaseHint>>> {
            self.batches
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.answer {
                Answer::Priced => Ok(opts
                    .iter()
                    .map(|o| {
                        Some(SourcePurchaseHint {
                            product_id: o.product_id.clone().unwrap_or_else(|| "p".into()),
                            title: o.title.clone(),
                            price_cents: Some(500),
                            currency: Some("USD".into()),
                            price_label: Some("$5.00".into()),
                            list_price_cents: Some(1_000),
                            list_price_label: Some("$10.00".into()),
                            member_price_cents: Some(500),
                            member_price_label: Some("$5.00".into()),
                            ..SourcePurchaseHint::default()
                        })
                    })
                    .collect()),
                Answer::Limited => Err(bookclerk_source::SourceError::RateLimited(Some(
                    Duration::from_secs(30),
                ))),
                Answer::Broken => Err(bookclerk_source::SourceError::api("down")),
                Answer::Untried => Ok(vec![None; opts.len()]),
            }
        }
    }

    /// A query whose title is unique to `name`, so tests share no cache row.
    fn batch_query(name: &str, preferred: &[&str]) -> PurchaseHintsQuery {
        PurchaseHintsQuery {
            title: format!("Batch {name}"),
            authors: None,
            asin: None,
            isbn: None,
            candidate_source: None,
            candidate_product_id: None,
            store_editions: Vec::new(),
            region: None,
            preferred_sources: preferred.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    /// A registry of one [`BatchStore`] and the counter of its calls.
    fn one_store(
        id: &'static str,
        answer: Answer,
    ) -> (
        SourceRegistry,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let batches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = SourceRegistry::new();
        registry.register(std::sync::Arc::new(BatchStore::new(
            id,
            answer,
            std::sync::Arc::clone(&batches),
        )));
        (registry, batches)
    }

    /// A price for a product the listing pool placed in another recording
    /// than the card's is dropped and `best` is picked again; a price for
    /// the card's own recording, or for a product the pool never listed,
    /// stays.
    #[tokio::test]
    async fn prices_for_another_recording_are_dropped() {
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let hit = |product: &str, kind| bookclerk_source::CatalogHit {
            product_id: product.into(),
            title: "The Sunlit Man".into(),
            is_abridged: Some(false),
            work_kind: bookclerk_source::records::WorkKind::Book,
            expression_kind: kind,
            media_kind: bookclerk_source::records::MediaKind::Audio,
            ..bookclerk_source::CatalogHit::default()
        };
        let now = chrono::Utc::now();
        let narration = crate::pool::catalog_listing(
            "libro",
            "us",
            &hit("L1", bookclerk_source::records::ExpressionKind::Narration),
            now,
        );
        let drama = crate::pool::catalog_listing(
            "chirp",
            "us",
            &hit(
                "G1",
                bookclerk_source::records::ExpressionKind::Dramatisation,
            ),
            now,
        );
        let audible = crate::pool::catalog_listing(
            "audible",
            "us",
            &hit(
                "B0DRAMA",
                bookclerk_source::records::ExpressionKind::Dramatisation,
            ),
            now,
        );
        library
            .record_catalog_listings(&[narration, drama, audible])
            .await
            .unwrap();
        let mut registry = SourceRegistry::new();
        for id in ["libro", "chirp", "audible"] {
            registry.register(std::sync::Arc::new(BatchStore::new(
                id,
                Answer::Priced,
                std::sync::Arc::default(),
            )));
        }
        let priced = |source: &str, product: &str, cents: i64| PurchaseHint {
            source: source.into(),
            product_id: product.into(),
            price_cents: Some(cents),
            ..PurchaseHint::default()
        };
        let query = PurchaseHintsQuery {
            title: "The Sunlit Man".into(),
            candidate_source: Some("libro".into()),
            candidate_product_id: Some("L1".into()),
            ..PurchaseHintsQuery::default()
        };
        let mut response = PurchaseHintsResponse {
            hints: vec![
                priced("libro", "L1", 2_000),
                priced("chirp", "G1", 1_499),
                priced("chirp", "unlisted", 1_800),
            ],
            best: Some(priced("chirp", "G1", 1_499)),
            incomplete: false,
        };
        retain_card_recording(&library, &registry, &query, &mut response)
            .await
            .unwrap();
        let kept: Vec<&str> = response
            .hints
            .iter()
            .map(|h| h.product_id.as_str())
            .collect();
        assert_eq!(kept, vec!["L1", "unlisted"]);
        assert_eq!(
            response.best.map(|b| b.product_id),
            Some(String::from("unlisted"))
        );

        // A card whose editions the pool does not know keeps every price.
        let unknown = PurchaseHintsQuery {
            title: "The Sunlit Man".into(),
            candidate_source: Some("chirp".into()),
            candidate_product_id: Some("unlisted".into()),
            ..PurchaseHintsQuery::default()
        };
        let mut all = PurchaseHintsResponse {
            hints: vec![priced("chirp", "G1", 1_499), priced("libro", "L1", 2_000)],
            best: None,
            incomplete: false,
        };
        retain_card_recording(&library, &registry, &unknown, &mut all)
            .await
            .unwrap();
        assert_eq!(all.hints.len(), 2);

        // A card named only by an ASIN is that Audible product: here the
        // pool placed it in a dramatisation, so the narration's price goes.
        let by_asin = PurchaseHintsQuery {
            title: "The Sunlit Man".into(),
            asin: Some("B0DRAMA".into()),
            ..PurchaseHintsQuery::default()
        };
        let mut priced_both = PurchaseHintsResponse {
            hints: vec![priced("libro", "L1", 2_000), priced("chirp", "G1", 1_499)],
            best: None,
            incomplete: false,
        };
        retain_card_recording(&library, &registry, &by_asin, &mut priced_both)
            .await
            .unwrap();
        let kept: Vec<&str> = priced_both
            .hints
            .iter()
            .map(|h| h.product_id.as_str())
            .collect();
        assert!(!kept.contains(&"L1"), "kept {kept:?}");
    }

    /// A Sunlit Man listing `product` of `kind`, credited like the
    /// narration, with `isbn` when given.
    fn sunlit_record(
        product: &str,
        kind: bookclerk_source::records::ExpressionKind,
        isbn: Option<&str>,
        cents: i64,
    ) -> bookclerk_source::PricedListing {
        use bookclerk_source::records::{credit, CreditRole, Identifier, IdentifierScheme};
        bookclerk_source::PricedListing {
            record: bookclerk_source::CatalogHit {
                product_id: product.into(),
                title: "The Sunlit Man".into(),
                language: Some("en".into()),
                is_abridged: Some(false),
                work_kind: bookclerk_source::records::WorkKind::Book,
                expression_kind: kind,
                media_kind: bookclerk_source::records::MediaKind::Audio,
                credits: [
                    ("Brandon Sanderson", CreditRole::Author),
                    ("William DeMeritt", CreditRole::Narrator),
                ]
                .into_iter()
                .filter_map(|(name, role)| credit(name, role))
                .collect(),
                identifiers: isbn
                    .map(|value| Identifier {
                        scheme: IdentifierScheme::Isbn,
                        value: value.into(),
                        marketplace: None,
                    })
                    .into_iter()
                    .collect(),
                ..bookclerk_source::CatalogHit::default()
            },
            hint: SourcePurchaseHint {
                product_id: product.into(),
                title: Some("The Sunlit Man".into()),
                price_cents: Some(cents),
                currency: Some("USD".into()),
                ..SourcePurchaseHint::default()
            },
        }
    }

    /// A title search's records price a card only with the recording the
    /// card names: by its ISBN without a pool, by the pool's placement with
    /// one. The cheaper dramatisation of the same title never prices it.
    #[tokio::test]
    async fn title_records_price_only_the_cards_recording() {
        use bookclerk_source::records::ExpressionKind;
        let records = vec![
            sunlit_record("D1", ExpressionKind::Dramatisation, None, 500),
            sunlit_record(
                "N1",
                ExpressionKind::Narration,
                Some("9781250890009"),
                1_500,
            ),
        ];
        let mut registry = SourceRegistry::new();
        registry.register(std::sync::Arc::new(BatchStore {
            records,
            ..BatchStore::new("records-a", Answer::Priced, std::sync::Arc::default())
        }));
        let by_isbn = PurchaseHintsQuery {
            title: "Records by ISBN".into(),
            isbn: Some("978-1-250-89000-9".into()),
            ..PurchaseHintsQuery::default()
        };
        let results = resolve_purchase_hints_batch(&registry, None, &[by_isbn], 4).await;
        let response = results[0].as_ref().unwrap();
        let kept: Vec<&str> = response
            .hints
            .iter()
            .map(|h| h.product_id.as_str())
            .collect();
        assert_eq!(kept, vec!["N1"]);
        assert_eq!(response.best.as_ref().unwrap().price_cents, Some(1_500));
        assert!(!response.incomplete);

        // With a pool: the card is a narration the pool already lists at
        // another store; a record with no shared identifier is kept because
        // the pool places it in that narration, and the dramatisation goes.
        let library = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let card = sunlit_record("L1", ExpressionKind::Narration, None, 0);
        library
            .record_catalog_listings(&[crate::pool::catalog_listing(
                "records-libro",
                "us",
                &card.record,
                chrono::Utc::now(),
            )])
            .await
            .unwrap();
        let mut registry = SourceRegistry::new();
        registry.register(std::sync::Arc::new(BatchStore::new(
            "records-libro",
            Answer::Untried,
            std::sync::Arc::default(),
        )));
        registry.register(std::sync::Arc::new(BatchStore {
            records: vec![
                sunlit_record("D2", ExpressionKind::Dramatisation, None, 500),
                sunlit_record("N2", ExpressionKind::Narration, None, 1_400),
            ],
            ..BatchStore::new("records-b", Answer::Priced, std::sync::Arc::default())
        }));
        let by_pool = PurchaseHintsQuery {
            title: "Records by pool".into(),
            candidate_source: Some("records-libro".into()),
            candidate_product_id: Some("L1".into()),
            ..PurchaseHintsQuery::default()
        };
        let without_pool =
            resolve_purchase_hints_batch(&registry, None, std::slice::from_ref(&by_pool), 4).await;
        assert!(
            without_pool[0].as_ref().unwrap().hints.is_empty(),
            "no pool and no shared identifier keeps no record"
        );
        // Another title, so the answer above is not read from the cache.
        let by_pool = PurchaseHintsQuery {
            title: "Records by pool, placed".into(),
            ..by_pool
        };
        let results = resolve_purchase_hints_batch(&registry, Some(&library), &[by_pool], 4).await;
        let kept: Vec<&str> = results[0]
            .as_ref()
            .unwrap()
            .hints
            .iter()
            .map(|h| h.product_id.as_str())
            .collect();
        assert_eq!(kept, vec!["N2"]);
    }

    /// A page of cards is one call per store; member prices only reach a
    /// viewer with an account there; answered queries come from the cache.
    #[tokio::test]
    async fn a_batch_is_one_call_per_store_and_respects_membership() {
        let (registry, batches) = one_store("batch-priced", Answer::Priced);
        let queries = vec![
            batch_query("member", &["batch-priced"]),
            batch_query("stranger", &[]),
        ];
        let results = resolve_purchase_hints_batch(&registry, None, &queries, 4).await;
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        let member = results[0].as_ref().unwrap().best.clone().unwrap();
        assert_eq!(member.price_cents, Some(500));
        let stranger = results[1].as_ref().unwrap().best.clone().unwrap();
        assert_eq!(stranger.price_cents, Some(1_000));
        assert!(stranger.member_price_cents.is_none());

        let again = resolve_purchase_hints_batch(&registry, None, &queries, 4).await;
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(again[0].as_ref().unwrap().best.is_some());
    }

    /// A 429 pauses the store: the next batch does not ask it, and nothing
    /// unanswered is cached.
    #[tokio::test]
    async fn a_rate_limited_store_is_paused_for_the_next_batch() {
        let (registry, batches) = one_store("batch-limited", Answer::Limited);
        let first = vec![batch_query("limited-1", &[])];
        let results = resolve_purchase_hints_batch(&registry, None, &first, 4).await;
        assert!(results[0].as_ref().unwrap().hints.is_empty());
        assert!(store_breaker_open(
            "batch-limited",
            std::time::Instant::now()
        ));
        let second = vec![batch_query("limited-2", &[])];
        resolve_purchase_hints_batch(&registry, None, &second, 4).await;
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Any other failure leaves the card's links and closes the breaker.
    #[tokio::test]
    async fn a_failing_store_leaves_the_trusted_links() {
        let (registry, batches) = one_store("batch-broken", Answer::Broken);
        let mut query = batch_query("broken", &[]);
        query.asin = Some("B000000009".into());
        let results = resolve_purchase_hints_batch(&registry, None, &[query.clone()], 4).await;
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 1);
        let response = results[0].as_ref().unwrap();
        assert_eq!(response.hints[0].source, "audible");
        assert!(response.hints[0].price_cents.is_none());
        assert!(!store_breaker_open(
            "batch-broken",
            std::time::Instant::now()
        ));
    }

    /// A store's `None` is no answer: the query is `incomplete`, is not
    /// cached, and the next batch asks the store again; a priced answer is
    /// complete.
    #[tokio::test]
    async fn an_untried_query_is_incomplete_and_asked_again() {
        let (registry, batches) = one_store("batch-untried", Answer::Untried);
        let queries = vec![batch_query("untried", &[])];
        let first = resolve_purchase_hints_batch(&registry, None, &queries, 4).await;
        assert!(first[0].as_ref().unwrap().incomplete);
        let again = resolve_purchase_hints_batch(&registry, None, &queries, 4).await;
        assert!(again[0].as_ref().unwrap().incomplete);
        assert_eq!(batches.load(std::sync::atomic::Ordering::SeqCst), 2);

        let (registry, _) = one_store("batch-complete", Answer::Priced);
        let priced =
            resolve_purchase_hints_batch(&registry, None, &[batch_query("complete", &[])], 4).await;
        assert!(!priced[0].as_ref().unwrap().incomplete);
    }

    /// A single-title lookup honors a pause too.
    #[tokio::test]
    async fn a_paused_store_is_skipped_by_single_lookups() {
        let (registry, _) = one_store("single-paused", Answer::Priced);
        note_store_rate_limit(
            "single-paused",
            Some(Duration::from_secs(60)),
            std::time::Instant::now(),
        );
        let mut hints = Vec::new();
        let opts = PurchaseHintOpts {
            title: Some("Paused".into()),
            with_price: true,
            lookup: PriceLookup::Unspecified,
            region: "us".into(),
            ..PurchaseHintOpts::default()
        };
        append_registry_hints(&registry, &mut hints, &opts, None).await;
        assert!(hints.is_empty());
    }

    #[test]
    fn a_batch_asks_a_store_about_the_product_id_it_already_knows() {
        let query = PurchaseHintsQuery {
            title: "Dune".into(),
            authors: Some("Frank Herbert".into()),
            asin: Some("B002V1OF70".into()),
            isbn: None,
            candidate_source: Some("chirp".into()),
            candidate_product_id: Some("12345".into()),
            store_editions: vec![crate::identity::StoreEdition::new("libro", "9780441172719")],
            region: None,
            preferred_sources: Vec::new(),
        };
        let store = |id| BatchStore::new(id, Answer::Priced, std::sync::Arc::default());
        let chirp = store_query_opts(&query, "us", &store("chirp"));
        assert_eq!(chirp.product_id.as_deref(), Some("12345"));
        assert_eq!(chirp.lookup, PriceLookup::Product);
        assert_eq!(
            store_query_opts(&query, "us", &store("libro"))
                .product_id
                .as_deref(),
            Some("9780441172719")
        );
        // A store with no product for the card searches by title, unless
        // it declared exact lookups by an identifier the card has.
        let audible = store_query_opts(&query, "us", &store("audible"));
        assert!(audible.product_id.is_none());
        assert_eq!(audible.asin.as_deref(), Some("B002V1OF70"));
        assert_eq!(audible.lookup, PriceLookup::Title);
        assert!(audible.with_price);
        let by_asin = BatchStore {
            by_asin: true,
            ..store("audible")
        };
        assert_eq!(
            store_query_opts(&query, "us", &by_asin).lookup,
            PriceLookup::Asin
        );
    }

    #[test]
    fn a_query_no_store_answered_keeps_its_trusted_links_only() {
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
        let hints = seeded_hints(&query, "us");
        // Only Audible is trusted from an id; Libro and Chirp need a live
        // title match, and nothing was priced.
        let sources: Vec<&str> = hints.iter().map(|h| h.source.as_str()).collect();
        assert_eq!(sources, vec!["audible"]);
        assert!(hints[0].price_cents.is_none());
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

    #[test]
    fn catalog_hint_keeps_the_same_title() {
        let hint = |title: Option<&str>| {
            PurchaseHint::link("libro", "9780593135204", title.map(String::from), None)
        };
        assert!(catalog_hint_matches_query(
            "Project Hail Mary",
            Some("Andy Weir"),
            &hint(Some("Project Hail Mary"))
        ));
        assert!(!catalog_hint_matches_query(
            "Project Hail Mary",
            None,
            &hint(Some("  "))
        ));
        assert!(!catalog_hint_matches_query(
            "  ",
            None,
            &hint(Some("Project Hail Mary"))
        ));
    }

    #[test]
    fn push_dedupe_keeps_one_row_per_source() {
        let hint = |source: &str, id: &str| PurchaseHint::link(source, id, None, None);
        let mut hints = Vec::new();
        push_dedupe(&mut hints, hint("audible", "B00X"));
        push_dedupe(&mut hints, hint("AUDIBLE", "b00x"));
        push_dedupe(&mut hints, hint("AUDIBLE", "B00Y"));
        push_dedupe(&mut hints, hint("libro", "B00X"));
        let rows: Vec<(&str, &str)> = hints
            .iter()
            .map(|h| (h.source.as_str(), h.product_id.as_str()))
            .collect();
        assert_eq!(rows, [("audible", "B00X"), ("libro", "B00X")]);
    }
}
