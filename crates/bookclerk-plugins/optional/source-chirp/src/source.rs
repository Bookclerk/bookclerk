//! [`ChirpSource`]: catalog lookups against the public Chirp storefront.
//!
//! Account login, library scans and title fetches run in the guest RPC
//! handlers ([`crate::guest`]); the host stores the credentials they return.

use bookclerk_storefront::records::{self, CreditRole};
use bookclerk_storefront::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, ExpandSeed, PurchaseHintOpts,
    SourcePurchaseHint,
};

use crate::client::{CatalogAudiobook, ChirpClient, DEFAULT_GRAPHQL_URL};

/// Describe / config id for this store (`chirp`).
pub const ID: &str = "chirp";

/// How long [`ChirpSource::purchase_hints`] keeps starting title lookups for
/// titles without a Chirp product id. Well under the host's 10 s batch
/// deadline, so the pricing query for every resolved title lands in time.
pub const RESOLVE_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// Chirp catalog client used by the guest's discovery entrypoints.
///
/// Construct with [`ChirpSource::new`]; use [`ChirpSource::with_graphql_url`]
/// for wiremock / staging.
#[derive(Debug, Clone)]
pub struct ChirpSource {
    /// Chirp GraphQL origin; production default or a wiremock/staging override.
    graphql_url: String,
}

impl Default for ChirpSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ChirpSource {
    /// Returns a Chirp source aimed at the production GraphQL endpoint.
    #[must_use]
    pub fn new() -> Self {
        Self {
            graphql_url: DEFAULT_GRAPHQL_URL.to_string(),
        }
    }

    /// Overrides the GraphQL endpoint (tests / staging).
    #[must_use]
    pub fn with_graphql_url(graphql_url: impl Into<String>) -> Self {
        Self {
            graphql_url: graphql_url.into(),
        }
    }

    /// Searches the public catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when a storefront request or its parse fails and the
    /// method cannot fall back. Some lookups fail soft and only yield fewer
    /// results (or none), so an empty result does not prove the store has none.
    pub async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
        let q = opts.query.trim();
        if q.is_empty() || opts.limit == 0 {
            return Ok(Vec::new());
        }
        let preferred_lang = opts
            .language
            .as_deref()
            .and_then(bookclerk_storefront::normalize_language);
        // Slight over-fetch when filtering by language so dropped Spanish
        // (etc.) rows don't starve the page — keep this modest for latency.
        let fetch_limit = language_fetch_limit(opts.limit, preferred_lang.is_some());
        let client = ChirpClient::new(&self.graphql_url);
        if opts.field == Some(CatalogSearchField::Series) {
            if let Ok(Some(catalog)) = client.resolve_series_catalog(q).await {
                let origin = format!("chirp series (“{}”)", catalog.series.name);
                return Ok(catalog
                    .audiobooks
                    .into_iter()
                    .map(|b| catalog_hit(&b, origin.clone()))
                    .filter(|h| language_matches(h.language.as_deref(), preferred_lang.as_deref()))
                    .take(opts.limit)
                    .collect());
            }
            // Fall through to typeahead/search when slug resolve misses.
        }
        let page = opts.page.max(1);
        let mut books = Vec::new();
        // Typeahead is page-1 only; deeper pages use GraphQL search.
        if page <= 1 {
            let tip = client.typeahead(q).await.unwrap_or_default();
            books = tip.audiobooks;
        }
        if books.len() < fetch_limit {
            if let Ok(more) = client.search_catalog(q, page, fetch_limit as u32).await {
                for b in more {
                    if !books.iter().any(|x| x.id == b.id) {
                        books.push(b);
                    }
                }
            }
        }
        Ok(books
            .into_iter()
            .map(|b| catalog_hit(&b, String::from("search")))
            .filter(|h| language_matches(h.language.as_deref(), preferred_lang.as_deref()))
            .take(opts.limit)
            .collect())
    }

    /// Finds up to `limit` titles related to `seed` (same author, series, …).
    ///
    /// # Errors
    ///
    /// Returns an error when a storefront request or its parse fails and the
    /// method cannot fall back. Some lookups fail soft and only yield fewer
    /// results (or none), so an empty result does not prove the store has none.
    pub async fn expand_candidates(
        &self,
        seed: &ExpandSeed,
        limit: usize,
    ) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let client = ChirpClient::new(&self.graphql_url);
        let mut hits: Vec<CatalogHit> = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let push = |hits: &mut Vec<CatalogHit>,
                    seen: &mut std::collections::HashSet<String>,
                    book: &CatalogAudiobook,
                    origin: String| {
            if hits.len() >= limit || !seen.insert(book.id.clone()) {
                return;
            }
            hits.push(catalog_hit(book, origin));
        };

        // Chirp-owned seeds: related + series siblings.
        if seed.source.eq_ignore_ascii_case(ID) && !seed.product_id.is_empty() {
            match client.related_audiobooks(&seed.product_id).await {
                Ok(related) => {
                    for book in &related.related {
                        push(
                            &mut hits,
                            &mut seen,
                            book,
                            format!("chirp related to “{}”", seed.title),
                        );
                    }
                    if let Some(series) = related.series {
                        if hits.len() < limit {
                            if let Ok(Some(catalog)) = client.series_catalog(&series.slug).await {
                                for book in &catalog.audiobooks {
                                    push(
                                        &mut hits,
                                        &mut seen,
                                        book,
                                        format!("chirp series (“{}”)", catalog.series.name),
                                    );
                                }
                            }
                        }
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        id = %seed.product_id,
                        error = %err,
                        "chirp related lookup failed"
                    );
                }
            }
        }

        // Series title → slug guesses.
        if hits.len() < limit {
            if let Some(series) = seed
                .series
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                match client.resolve_series_catalog(series).await {
                    Ok(Some(catalog)) => {
                        for book in &catalog.audiobooks {
                            push(
                                &mut hits,
                                &mut seen,
                                book,
                                format!("chirp series (“{}”)", catalog.series.name),
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::debug!(series, error = %err, "chirp series resolve failed");
                    }
                }
            }
        }

        // Author → typeahead slug → summary.
        if hits.len() < limit {
            if let Some(author) = primary_author(seed.authors.as_deref()) {
                match client.resolve_author_slug(author).await {
                    Ok(Some(slug)) => match client.author_summary(&slug).await {
                        Ok(Some(catalog)) => {
                            for book in &catalog.audiobooks {
                                push(
                                    &mut hits,
                                    &mut seen,
                                    book,
                                    format!("chirp author ({})", catalog.author.name),
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::debug!(slug, error = %err, "chirp author summary failed");
                        }
                    },
                    Ok(None) => {}
                    Err(err) => {
                        tracing::debug!(author, error = %err, "chirp author resolve failed");
                    }
                }
            }
        }

        // Fallback catalog search for non-Chirp seeds.
        if hits.len() < limit && !seed.source.eq_ignore_ascii_case(ID) {
            let q = match primary_author(seed.authors.as_deref()) {
                Some(a) => format!("{} {a}", seed.title),
                None => seed.title.clone(),
            };
            match client.search_catalog(&q, 1, 8).await {
                Ok(books) => {
                    for book in &books {
                        push(
                            &mut hits,
                            &mut seen,
                            book,
                            format!("chirp catalog search (“{}”)", seed.title),
                        );
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "chirp catalog search failed");
                }
            }
        }

        hits.truncate(limit);
        Ok(hits)
    }

    /// Resolves the storefront price and link for a title; `None` when not sold.
    ///
    /// # Errors
    ///
    /// Returns an error when a storefront request or its parse fails and the
    /// method cannot fall back. Failed lookups are soft, so `Ok(None)` can also
    /// mean the store could not be reached, not only that the title is not sold.
    pub async fn purchase_hint(
        &self,
        opts: &PurchaseHintOpts,
    ) -> bookclerk_storefront::Result<Option<SourcePurchaseHint>> {
        let client = ChirpClient::new(&self.graphql_url);
        let mut hint = if let Some(pid) = opts
            .product_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(SourcePurchaseHint {
                product_id: pid.to_string(),
                title: opts.title.clone(),
                url: Some(format!("https://www.chirpbooks.com/audiobooks/{pid}")),
                ..Default::default()
            })
        } else {
            let title = opts.title.as_deref().unwrap_or("").trim();
            if title.is_empty() {
                None
            } else {
                let q = match opts
                    .authors
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(a) => format!("{title} {a}"),
                    None => title.to_string(),
                };
                let tip = match client.typeahead(&q).await {
                    Ok(tip) => tip,
                    // The store's "slow down" stops the lookup and reaches
                    // the host, which pauses the store.
                    Err(crate::error::ChirpError::RateLimited(wait)) => {
                        return Err(bookclerk_storefront::SourceError::RateLimited(wait));
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "chirp typeahead failed");
                        crate::client::TypeaheadCatalog::default()
                    }
                };
                tip.audiobooks
                    .into_iter()
                    .find(|hit| {
                        chirp_purchase_title_matches(
                            title,
                            hit.display_title.as_deref().unwrap_or(""),
                        )
                    })
                    .map(|hit| {
                        let url = hit
                            .url
                            .map(|u| {
                                if u.starts_with("http") {
                                    u
                                } else {
                                    format!("https://www.chirpbooks.com{u}")
                                }
                            })
                            .or_else(|| {
                                Some(format!("https://www.chirpbooks.com/audiobooks/{}", hit.id))
                            });
                        SourcePurchaseHint {
                            product_id: hit.id,
                            title: hit.display_title.or_else(|| opts.title.clone()),
                            url,
                            ..Default::default()
                        }
                    })
            }
        };

        if opts.with_price {
            if let Some(ref mut h) = hint {
                match client.audiobook_pricing(&h.product_id).await {
                    Ok(Some(pricing)) => apply_chirp_pricing(h, &pricing),
                    Ok(None) => {}
                    // The store's own "slow down" reaches the host; other
                    // failures leave the card unpriced.
                    Err(crate::error::ChirpError::RateLimited(wait)) => {
                        return Err(bookclerk_storefront::SourceError::RateLimited(wait));
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "chirp pricing failed");
                    }
                }
            }
        }
        Ok(hint)
    }

    /// Resolves purchase hints for several titles, pricing them in one
    /// aliased GraphQL request instead of one per card.
    ///
    /// Titles whose Chirp product id is known resolve first, without a
    /// search; the rest are looked up by title until the 5 s `RESOLVE_BUDGET` runs
    /// out, and a title not tried by then answers `None`, which the host
    /// does not take as an answer.
    ///
    /// # Errors
    ///
    /// Returns [`bookclerk_storefront::SourceError::RateLimited`] when Chirp
    /// asks to slow down, during a title lookup or the pricing query; other
    /// failures leave the cards unpriced.
    pub async fn purchase_hints(
        &self,
        opts: &[PurchaseHintOpts],
    ) -> bookclerk_storefront::Result<Vec<Option<SourcePurchaseHint>>> {
        self.purchase_hints_within(opts, RESOLVE_BUDGET).await
    }

    /// [`Self::purchase_hints`] with a chosen title-lookup budget.
    ///
    /// # Errors
    ///
    /// As [`Self::purchase_hints`].
    async fn purchase_hints_within(
        &self,
        opts: &[PurchaseHintOpts],
        budget: std::time::Duration,
    ) -> bookclerk_storefront::Result<Vec<Option<SourcePurchaseHint>>> {
        let deadline = tokio::time::Instant::now() + budget;
        let known = |q: &PurchaseHintOpts| {
            q.product_id
                .as_deref()
                .map(str::trim)
                .is_some_and(|s| !s.is_empty())
        };
        let mut hints: Vec<Option<SourcePurchaseHint>> = vec![None; opts.len()];
        // Known products first, so a slow lookup never costs them a price.
        let mut order: Vec<usize> = (0..opts.len()).collect();
        order.sort_by_key(|&i| !known(&opts[i]));
        for i in order {
            let unpriced = PurchaseHintOpts {
                with_price: false,
                ..opts[i].clone()
            };
            if known(&opts[i]) {
                hints[i] = self.purchase_hint(&unpriced).await?;
                continue;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            match tokio::time::timeout_at(deadline, self.purchase_hint(&unpriced)).await {
                Ok(resolved) => hints[i] = resolved?,
                // Out of lookup time: the rest stay untried.
                Err(_) => break,
            }
        }
        let ids: Vec<String> = hints
            .iter()
            .zip(opts)
            .filter(|(hint, query)| query.with_price && hint.is_some())
            .filter_map(|(hint, _)| hint.as_ref().map(|h| h.product_id.clone()))
            .collect();
        if ids.is_empty() {
            return Ok(hints);
        }
        let client = ChirpClient::new(&self.graphql_url);
        let prices = match client.audiobook_pricings(&ids).await {
            Ok(prices) => prices,
            Err(crate::error::ChirpError::RateLimited(wait)) => {
                return Err(bookclerk_storefront::SourceError::RateLimited(wait));
            }
            Err(err) => {
                tracing::debug!(error = %err, "chirp batch pricing failed");
                return Ok(hints);
            }
        };
        for (hint, query) in hints.iter_mut().zip(opts) {
            if let (true, Some(h)) = (query.with_price, hint.as_mut()) {
                if let Some(pricing) = prices.get(&h.product_id) {
                    apply_chirp_pricing(h, pricing);
                }
            }
        }
        Ok(hints)
    }

    /// Lists up to `limit` titles on the storefront's current deals shelf.
    ///
    /// # Errors
    ///
    /// Returns an error when a storefront request or its parse fails and the
    /// method cannot fall back. Some lookups fail soft and only yield fewer
    /// results (or none), so an empty result does not prove the store has none.
    pub async fn list_deals(&self, limit: usize) -> bookclerk_storefront::Result<Vec<CatalogHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let client = ChirpClient::new(&self.graphql_url);
        let mut hits = Vec::new();
        let mut seen = std::collections::HashSet::new();

        match client
            .top_deals(u32::try_from(limit.min(40)).unwrap_or(40))
            .await
        {
            Ok(books) => {
                for book in books {
                    if hits.len() >= limit || !seen.insert(book.id.clone()) {
                        break;
                    }
                    hits.push(catalog_hit(&book, String::from("chirp top deals")));
                }
            }
            Err(err) => tracing::debug!(error = %err, "chirp top deals failed"),
        }

        if hits.len() < limit {
            match client.free_deals().await {
                Ok(books) => {
                    for book in books {
                        if hits.len() >= limit || !seen.insert(book.id.clone()) {
                            break;
                        }
                        hits.push(catalog_hit(&book, String::from("chirp free deals")));
                    }
                }
                Err(err) => tracing::debug!(error = %err, "chirp free deals failed"),
            }
        }

        Ok(hits)
    }
}

/// True on exact fold or a contiguous phrase match when the shorter title is ≥8 chars and ≥55% of the longer.
fn chirp_purchase_title_matches(query: &str, hit_title: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    ' '
                }
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let q = norm(query);
    let h = norm(hit_title);
    if q.is_empty() || h.is_empty() {
        return false;
    }
    if q == h {
        return true;
    }
    let (shorter, longer) = if q.chars().count() <= h.chars().count() {
        (q.as_str(), h.as_str())
    } else {
        (h.as_str(), q.as_str())
    };
    if shorter.chars().count() < 8 {
        return false;
    }
    let ratio = shorter.chars().count() as f32 / longer.chars().count() as f32;
    ratio >= 0.55
        && (longer.starts_with(&format!("{shorter} "))
            || longer.ends_with(&format!(" {shorter}"))
            || longer.contains(&format!(" {shorter} ")))
}

/// Maps a Chirp catalog audiobook onto a [`CatalogHit`], including duration in minutes.
fn catalog_hit(book: &CatalogAudiobook, origin: String) -> CatalogHit {
    let categories = chirp_genres(book);
    CatalogHit {
        product_id: book.id.clone(),
        title: book.title(),
        credits: {
            let mut credits =
                records::credits_from_display(book.display_authors.as_deref(), CreditRole::Author);
            credits.extend(records::credits_from_display(
                book.display_narrators.as_deref(),
                CreditRole::Narrator,
            ));
            credits
        },
        series_entries: records::series_entries(
            book.series_name().as_deref(),
            book.series_audiobook
                .as_ref()
                .and_then(|s| {
                    s.display_number
                        .clone()
                        .or_else(|| s.number.map(|n| n.to_string()))
                })
                .as_deref(),
        ),
        url: book.url.clone().map(|u| {
            if u.starts_with("http") {
                u
            } else {
                format!("https://www.chirpbooks.com{u}")
            }
        }),
        cover_url: book
            .cover_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        origin,
        subtitle: book.sub_title.clone().filter(|s| !s.trim().is_empty()),
        description: book.description.clone().filter(|s| !s.trim().is_empty()),
        publisher: book.publisher.clone().filter(|s| !s.trim().is_empty()),
        length_minutes: book
            .duration_ms
            .map(|ms| (ms / 60_000) as i64)
            .filter(|n| *n > 0),
        published_at: book.released_on.clone().filter(|s| !s.trim().is_empty()),
        categories,
        language: book
            .language
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        is_abridged: book.abridged,
        ..Default::default()
    }
}

/// Semicolon-joined promoted-tag display names, or `None` when none are present.
fn chirp_genres(book: &CatalogAudiobook) -> Option<String> {
    let names: Vec<&str> = book
        .promoted_tags
        .iter()
        .filter_map(|t| t.display_name.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        None
    } else {
        Some(names.join("; "))
    }
}

/// Keep preferred language; drop known others; keep unknown/missing.
fn language_matches(hit_language: Option<&str>, preferred: Option<&str>) -> bool {
    let Some(pref) = preferred else {
        return true;
    };
    match hit_language.and_then(bookclerk_storefront::normalize_language) {
        None => true,
        Some(code) => code == pref,
    }
}

/// First non-empty author token split on comma, semicolon, or ampersand.
fn primary_author(authors: Option<&str>) -> Option<&str> {
    authors?
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

/// Sets USD cents/label (and optional purchase URL) from Chirp product pricing; no-op when unparseable.
fn apply_chirp_pricing(
    hint: &mut SourcePurchaseHint,
    pricing: &crate::client::ChirpProductPricing,
) {
    let label = pricing.discount_price.trim();
    let (cents, label) = if pricing.is_free_listing
        || label.eq_ignore_ascii_case("free")
        || label.eq_ignore_ascii_case("free!")
        || pricing.discounted_price_cents == Some(0)
    {
        (0_i64, String::from("FREE"))
    } else if let Some(c) = pricing.discounted_price_cents.filter(|c| *c > 0) {
        let display = if label.is_empty() {
            format!("${}.{:02}", c / 100, (c % 100).unsigned_abs())
        } else {
            label.to_string()
        };
        (c, display)
    } else if let Some(c) = parse_money_label_to_cents(label) {
        (c, label.to_string())
    } else {
        return;
    };
    hint.price_cents = Some(cents.max(0));
    hint.currency = Some(String::from("USD"));
    hint.price_label = Some(label);
    if let Some(url) = pricing.purchase_url.clone() {
        hint.url = Some(if url.starts_with("http") {
            url
        } else {
            format!("https://www.chirpbooks.com{url}")
        });
    }
}

/// Parses `$12.34` / `FREE` into integer cents; commas ignored, first number wins.
fn parse_money_label_to_cents(raw: &str) -> Option<i64> {
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

/// Rows to request for a page of `limit` hits.
///
/// A language filter drops rows after the fetch, so it asks for up to 8 more
/// (capped at 40 for latency), but never fewer than `limit` itself.
fn language_fetch_limit(limit: usize, filtered: bool) -> usize {
    if filtered {
        limit.saturating_add(8).min(limit.max(40))
    } else {
        limit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{CatalogAudiobook, CatalogTag};

    /// Search options for `query` with a page of `limit` results.
    fn search(query: &str, limit: usize) -> CatalogSearchOpts {
        CatalogSearchOpts {
            query: query.into(),
            region: "us".into(),
            limit,
            page: 1,
            sort: bookclerk_storefront::CatalogSearchSort::Relevance,
            field: None,
            language: None,
        }
    }

    /// A related-titles seed with no optional fields set.
    fn seed() -> ExpandSeed {
        ExpandSeed {
            source: "chirp".into(),
            product_id: "1".into(),
            title: "Title".into(),
            authors: None,
            narrators: None,
            series: None,
            series_asin: None,
            asin: None,
            isbn: None,
            region: "us".into(),
        }
    }

    #[test]
    fn language_over_fetch_never_drops_below_the_page() {
        assert_eq!(language_fetch_limit(10, false), 10);
        assert_eq!(language_fetch_limit(10, true), 18);
        assert_eq!(language_fetch_limit(35, true), 40);
        assert_eq!(language_fetch_limit(50, true), 50);
        assert_eq!(language_fetch_limit(usize::MAX, true), usize::MAX);
    }

    /// With no lookup time left, a title without a product id is left
    /// untried while a known product still resolves, without calling Chirp.
    #[tokio::test]
    async fn known_products_resolve_before_the_lookup_budget() {
        let source = ChirpSource::with_graphql_url("http://127.0.0.1:9/graphql");
        let by_title = PurchaseHintOpts {
            title: Some("Title Only".into()),
            region: "us".into(),
            ..PurchaseHintOpts::default()
        };
        let known = PurchaseHintOpts {
            product_id: Some("42".into()),
            region: "us".into(),
            ..PurchaseHintOpts::default()
        };
        let hints = source
            .purchase_hints_within(&[by_title, known], std::time::Duration::ZERO)
            .await
            .unwrap();
        assert!(hints[0].is_none());
        assert_eq!(hints[1].as_ref().unwrap().product_id, "42");
    }

    #[tokio::test]
    async fn empty_requests_answer_without_calling_the_store() {
        // These inputs return before any request; the unroutable URL keeps
        // the test offline if that ever regresses.
        let source = ChirpSource::with_graphql_url("http://127.0.0.1:9/graphql");
        assert!(source
            .search_catalog(&search("  ", 10))
            .await
            .expect("blank")
            .is_empty());
        assert!(source
            .search_catalog(&search("dune", 0))
            .await
            .expect("zero")
            .is_empty());
        assert!(source
            .expand_candidates(&seed(), 0)
            .await
            .expect("expand")
            .is_empty());
        assert!(source.list_deals(0).await.expect("deals").is_empty());
    }

    #[test]
    fn catalog_hit_maps_genres_language_abridged() {
        let book = CatalogAudiobook {
            id: "249058".into(),
            display_title: Some("The Black Ice".into()),
            display_authors: Some("Michael Connelly".into()),
            display_narrators: Some("Dick Hill".into()),
            url: Some("/audiobooks/249058".into()),
            cover_url: None,
            series_audiobook: None,
            sub_title: None,
            description: None,
            publisher: Some("Brilliance Audio".into()),
            duration_ms: Some(40_260_000),
            released_on: None,
            language: Some("English".into()),
            abridged: Some(false),
            promoted_tags: vec![
                CatalogTag {
                    display_name: Some("Thrillers".into()),
                },
                CatalogTag {
                    display_name: Some("Crime Fiction & Mysteries".into()),
                },
            ],
        };
        let hit = catalog_hit(&book, "search".into());
        assert_eq!(
            hit.categories.as_deref(),
            Some("Thrillers; Crime Fiction & Mysteries")
        );
        assert_eq!(hit.language.as_deref(), Some("English"));
        assert_eq!(hit.is_abridged, Some(false));
        assert_eq!(hit.length_minutes, Some(671));
    }

    #[test]
    fn language_matches_drops_spanish_when_english_preferred() {
        assert!(language_matches(Some("English"), Some("en")));
        assert!(!language_matches(Some("Spanish"), Some("en")));
        assert!(language_matches(None, Some("en")));
        assert!(language_matches(Some("Spanish"), None));
    }
}
