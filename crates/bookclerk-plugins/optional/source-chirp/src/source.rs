//! [`ChirpSource`]: [`ContentSource`] implementation for Chirp.

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::SourceScope;
use bookclerk_source::{
    CatalogHit, CatalogSearchField, CatalogSearchOpts, ContentSource, ExpandSeed, FetchOptions,
    LoginOptions, PortalAuthMode, PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount,
    SourceBrand, SourceFetch, SourcePurchaseHint, SourceRegistry,
};

use crate::auth::ChirpAuthFile;
use crate::client::{CatalogAudiobook, ChirpClient, DEFAULT_GRAPHQL_URL};
use crate::db::{delete_auth_from_db, list_auth_from_db, load_auth_from_db, save_auth_to_db};
use crate::download::fetch_title_materials;
use crate::error::{ChirpError, Result};
use crate::sync::{scan_library, ScanOptions as ChirpScanOptions};

/// Handshake / config id for this store (`chirp` in `[sources.chirp]`).
pub const ID: &str = "chirp";

/// Env var read for non-interactive password login when Accounts UI is unavailable.
pub const PASSWORD_ENV: &str = "BOOKCLERK_CHIRP_PASSWORD";

/// Chirp storefront adapter implementing [`ContentSource`].
///
/// Prefer [`ChirpSource::from_config`] when registering with the host; use
/// [`ChirpSource::with_graphql_url`] for wiremock / staging.
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

    /// Parse `[sources.chirp]` (enable flag only today).
    #[must_use]
    pub fn from_config(_config: &Config) -> Self {
        Self::new()
    }

    /// Overrides the GraphQL endpoint (tests / staging).
    #[must_use]
    pub fn with_graphql_url(graphql_url: impl Into<String>) -> Self {
        Self {
            graphql_url: graphql_url.into(),
        }
    }

    /// Arc-wrapped instance for [`bookclerk_source::SourceRegistry`].
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Logs in with email/password and seals credentials into `encrypted_secrets`.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn login_account(
        &self,
        library: &SourceScope,
        opts: LoginOptions,
    ) -> Result<SourceAccount> {
        let email = opts
            .email
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ChirpError::auth("Chirp login requires email"))?;
        let password = opts
            .password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ChirpError::auth("Chirp login requires password"))?;

        let mut client = ChirpClient::new(&self.graphql_url);
        let user = client.login(email, password).await?;

        let marketplace = if opts.marketplace.trim().is_empty() {
            String::from("us")
        } else {
            opts.marketplace.trim().to_ascii_lowercase()
        };

        let auth = ChirpAuthFile {
            access_token: user.token,
            web_token: user.web_token,
            email: user.email,
            user_id: Some(user.id),
            marketplace,
            label: opts.label.clone(),
        };

        let account_id = auth.account_id().to_string();
        save_auth_to_db(&auth, library, &account_id)
            .await
            .map_err(|e| ChirpError::auth(format!("failed to save Chirp auth: {e}")))?;
        library
            .upsert_account(&account_id, &auth.marketplace, auth.label.as_deref(), true)
            .await
            .map_err(|e| ChirpError::auth(format!("failed to upsert Chirp account: {e}")))?;

        tracing::info!(
            email = %auth.email,
            user_id = ?auth.user_id,
            "saved Chirp auth to encrypted_secrets"
        );

        Ok(source_account_from_auth(&auth))
    }

    /// Delete a Chirp account from the DB.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn delete_account(&self, library: &SourceScope, account_id: &str) -> Result<()> {
        delete_auth_from_db(library, account_id).await
    }
}

#[async_trait]
impl ContentSource for ChirpSource {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Chirp"
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Password
    }

    fn portal_brand(&self) -> SourceBrand {
        SourceBrand {
            id: "chirp",
            name: "Chirp",
            bg: "#0F766E",
            fg: "#ECFEFF",
            accent: "#14B8A6",
            icon_url: "https://www.google.com/s2/favicons?domain=chirpbooks.com&sz=128",
        }
    }

    fn password_env_var(&self) -> Option<&'static str> {
        Some(PASSWORD_ENV)
    }

    fn sort_key(&self) -> u32 {
        3
    }

    async fn login(
        &self,
        library: &SourceScope,
        opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        self.login_account(library, opts).await.map_err(Into::into)
    }

    async fn list_accounts(
        &self,
        library: &SourceScope,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        let records = list_auth_from_db(library)
            .await
            .map_err(Into::<bookclerk_source::SourceError>::into)?;
        Ok(records
            .into_iter()
            .map(|(_id, auth)| source_account_from_auth(&auth))
            .collect())
    }

    async fn scan(
        &self,
        library: &SourceScope,
        opts: ScanOptions,
    ) -> bookclerk_source::Result<ScanSummary> {
        scan_library(
            library,
            ChirpScanOptions::from(&opts),
            Some(self.graphql_url.as_str()),
        )
        .await
        .map_err(Into::into)
    }

    async fn fetch_title(
        &self,
        library: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> bookclerk_source::Result<SourceFetch> {
        let auth = load_auth_from_db(library, account_id)
            .await
            .map_err(|e| bookclerk_source::SourceError::Auth(e.to_string()))?
            .ok_or_else(|| {
                bookclerk_source::SourceError::Auth(format!(
                    "no Chirp credentials for account `{account_id}` in DB"
                ))
            })?;
        let _ = &opts.files_dir;
        let client = ChirpClient::new(&self.graphql_url).with_token(&auth.access_token);
        let plain = fetch_title_materials(&client, title_id, &opts.cache_dir).await?;
        Ok(plain)
    }

    async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
        let q = opts.query.trim();
        if q.is_empty() || opts.limit == 0 {
            return Ok(Vec::new());
        }
        let preferred_lang = opts
            .language
            .as_deref()
            .and_then(bookclerk_source::normalize_language);
        // Slight over-fetch when filtering by language so dropped Spanish
        // (etc.) rows don't starve the page — keep this modest for latency.
        let fetch_limit = if preferred_lang.is_some() {
            (opts.limit + 8).clamp(opts.limit, 40)
        } else {
            opts.limit
        };
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

    async fn expand_candidates(
        &self,
        seed: &ExpandSeed,
        limit: usize,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
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

    async fn purchase_hint(
        &self,
        opts: &PurchaseHintOpts,
    ) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
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
                let tip = client.typeahead(&q).await.unwrap_or_default();
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
                if let Ok(Some(pricing)) = client.audiobook_pricing(&h.product_id).await {
                    apply_chirp_pricing(h, &pricing);
                }
            }
        }
        Ok(hint)
    }

    async fn list_deals(&self, limit: usize) -> bookclerk_source::Result<Vec<CatalogHit>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let client = ChirpClient::new(&self.graphql_url);
        let mut hits = Vec::new();
        let mut seen = std::collections::HashSet::new();

        match client.top_deals(limit.min(40) as u32).await {
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
        authors: book.display_authors.clone(),
        narrators: book.display_narrators.clone(),
        series: book.series_name(),
        series_index: book.series_audiobook.as_ref().and_then(|s| {
            s.display_number
                .clone()
                .or_else(|| s.number.map(|n| n.to_string()))
        }),
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
    match hit_language.and_then(bookclerk_source::normalize_language) {
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

/// Builds a scan-enabled [`SourceAccount`] from a saved Chirp auth blob.
fn source_account_from_auth(auth: &ChirpAuthFile) -> SourceAccount {
    SourceAccount {
        account_id: auth.account_id().to_string(),
        source: ID.into(),
        marketplace: auth.marketplace.clone(),
        label: auth.label.clone().or_else(|| Some(auth.email.clone())),
        scan_enabled: true,
    }
}

/// Parse `[sources.chirp]` into a [`ChirpSource`].
#[must_use]
pub fn from_config(config: &Config) -> ChirpSource {
    ChirpSource::from_config(config)
}

/// Register Chirp when `[sources.chirp] enabled` (default true).
pub fn register(registry: &mut SourceRegistry, config: &Config) {
    if config.sources.is_enabled(ID) {
        registry.register(Arc::new(from_config(config)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{CatalogAudiobook, CatalogTag};

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
