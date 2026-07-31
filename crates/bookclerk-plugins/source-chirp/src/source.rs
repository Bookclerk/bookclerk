//! [`ChirpSource`]: [`ContentSource`] implementation for Chirp.

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::SourceScope;
use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ContentSource, ExpandSeed, FetchOptions, LoginOptions,
    PortalAuthMode, PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount, SourceBrand,
    SourceFetch, SourcePurchaseHint, SourceRegistry,
};

use crate::auth::ChirpAuthFile;
use crate::client::{CatalogAudiobook, ChirpClient, DEFAULT_GRAPHQL_URL};
use crate::db::{delete_auth_from_db, list_auth_from_db, load_auth_from_db, save_auth_to_db};
use crate::download::fetch_title_materials;
use crate::error::{ChirpError, Result};
use crate::sync::{scan_library, ScanOptions as ChirpScanOptions};

/// Canonical plugin id.
pub const ID: &str = "chirp";

/// Env var for non-interactive password login.
pub const PASSWORD_ENV: &str = "BOOKCLERK_CHIRP_PASSWORD";

/// Chirp content source.
#[derive(Debug, Clone)]
pub struct ChirpSource {
    graphql_url: String,
}

impl Default for ChirpSource {
    fn default() -> Self {
        Self::new()
    }
}

impl ChirpSource {
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

    #[must_use]
    pub fn with_graphql_url(graphql_url: impl Into<String>) -> Self {
        Self {
            graphql_url: graphql_url.into(),
        }
    }

    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Login and persist credentials to DB.
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
        let client = ChirpClient::new(&self.graphql_url);
        let tip = client.typeahead(q).await.unwrap_or_default();
        let mut books = tip.audiobooks;
        if books.len() < opts.limit {
            if let Ok(more) = client.search_catalog(q, 1, opts.limit as u32).await {
                for b in more {
                    if !books.iter().any(|x| x.id == b.id) {
                        books.push(b);
                    }
                }
            }
        }
        Ok(books
            .into_iter()
            .take(opts.limit)
            .map(|b| catalog_hit(&b, String::from("search")))
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
                price_cents: None,
                currency: None,
                price_label: None,
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
                tip.audiobooks.into_iter().next().map(|hit| {
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
                        price_cents: None,
                        currency: None,
                        price_label: None,
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

fn catalog_hit(book: &CatalogAudiobook, origin: String) -> CatalogHit {
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
        asin: None,
        isbn: None,
        url: book.url.clone().map(|u| {
            if u.starts_with("http") {
                u
            } else {
                format!("https://www.chirpbooks.com{u}")
            }
        }),
        origin,
    }
}

fn primary_author(authors: Option<&str>) -> Option<&str> {
    authors?
        .split([',', ';', '&'])
        .map(str::trim)
        .find(|s| !s.is_empty())
}

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
