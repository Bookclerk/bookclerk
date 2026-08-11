//! [`GraphicAudioSource`]: [`ContentSource`] implementation for GraphicAudio.

use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::SourceScope;
use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ContentSource, ExpandSeed, FetchOptions, LoginOptions,
    PortalAuthMode, PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount, SourceBrand,
    SourceFetch, SourcePurchaseHint, SourceRegistry,
};

use crate::auth::GraphicAudioAuthFile;
use crate::catalog::{
    catalog_http_client, expand_from_product_id, expand_from_search, MagentoCatalogProduct,
};
use crate::client::{GraphicAudioClient, DEFAULT_BASE_URL};
use crate::db::{delete_auth_from_db, list_auth_from_db, load_auth_from_db, save_auth_to_db};
use crate::download::{
    fetch_title_with_mode, password_from_env, product_title_for, TitleFetchRequest, GA_PASSWORD_ENV,
};
use crate::error::{GraphicAudioError, Result};
use crate::magento::{MagentoClient, DEFAULT_STORE_URL};
use crate::options::{GraphicAudioAccess, GraphicAudioBitrate, GraphicAudioContainer};
use crate::sync::{scan_library, ScanOptions as GaScanOptions};

/// Handshake / config id for this store (`graphicaudio` in `[sources.graphicaudio]`).
pub const ID: &str = "graphicaudio";

const ALIASES: &[&str] = &["ga", "graphic-audio"];

/// GraphicAudio storefront adapter implementing [`ContentSource`].
///
/// Access path (`web` / `zip` / `device`) comes from config; see crate-level docs.
#[derive(Debug, Clone)]
pub struct GraphicAudioSource {
    /// Access App API origin (`…/access`).
    base_url: String,
    /// Magento storefront origin (ZIP + Browser Player).
    store_url: String,
    /// Configured access path (login + default fetch). Env may still override fetch.
    pub access: GraphicAudioAccess,
    /// Device encode bitrate (`[sources.graphicaudio] bitrate`).
    pub bitrate: GraphicAudioBitrate,
    /// ZIP SKU container preference (`[sources.graphicaudio] container`).
    pub container: GraphicAudioContainer,
    /// Optional fetch-mode override (tests / embedding).
    fetch_mode: Option<GraphicAudioAccess>,
    /// Optional Magento password override; else [`password_from_env`].
    magento_password: Option<String>,
}

impl Default for GraphicAudioSource {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphicAudioSource {
    /// Production GraphicAudio Access API + storefront origins (access=`web`).
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            store_url: DEFAULT_STORE_URL.to_string(),
            access: GraphicAudioAccess::Web,
            bitrate: GraphicAudioBitrate::Hi,
            container: GraphicAudioContainer::Auto,
            fetch_mode: None,
            magento_password: None,
        }
    }

    /// Parse `[sources.graphicaudio]` knobs from config.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let access = config
            .sources
            .get_string(ID, "access")
            .and_then(GraphicAudioAccess::parse)
            .or_else(GraphicAudioAccess::from_env)
            .unwrap_or_default();
        let bitrate = config
            .sources
            .get_string(ID, "bitrate")
            .and_then(GraphicAudioBitrate::parse)
            .unwrap_or_default();
        let container = config
            .sources
            .get_string(ID, "container")
            .and_then(GraphicAudioContainer::parse)
            .unwrap_or_default();
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            store_url: DEFAULT_STORE_URL.to_string(),
            access,
            bitrate,
            container,
            fetch_mode: None,
            magento_password: None,
        }
    }

    /// Override Access API base (wiremock / staging).
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            store_url: DEFAULT_STORE_URL.to_string(),
            access: GraphicAudioAccess::Web,
            bitrate: GraphicAudioBitrate::Hi,
            container: GraphicAudioContainer::Auto,
            fetch_mode: None,
            magento_password: None,
        }
    }

    /// Override Magento storefront base (wiremock).
    #[must_use]
    pub fn with_store_url(mut self, store_url: impl Into<String>) -> Self {
        self.store_url = store_url.into().trim_end_matches('/').to_string();
        self
    }

    /// Set the configured access path (`[sources.graphicaudio] access`).
    #[must_use]
    pub fn with_access(mut self, access: GraphicAudioAccess) -> Self {
        self.access = access;
        self
    }

    /// Sets the Access App encode bitrate preference.
    #[must_use]
    pub fn with_bitrate(mut self, bitrate: GraphicAudioBitrate) -> Self {
        self.bitrate = bitrate;
        self
    }

    /// Sets the Magento ZIP container preference (`mp3` / `m4b` / auto).
    #[must_use]
    pub fn with_container(mut self, container: GraphicAudioContainer) -> Self {
        self.container = container;
        self
    }

    /// Force a fetch path (bypasses config / env).
    #[must_use]
    pub fn with_fetch_mode(mut self, mode: GraphicAudioAccess) -> Self {
        self.fetch_mode = Some(mode);
        self
    }

    /// Magento password for ZIP / Browser Player (bypasses `BOOKCLERK_GA_PASSWORD`).
    #[must_use]
    pub fn with_magento_password(mut self, password: impl Into<String>) -> Self {
        self.magento_password = Some(password.into());
        self
    }

    /// Arc-wrapped instance for [`bookclerk_source::SourceRegistry`].
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Login and persist credentials to the DB.
    ///
    /// - `access=web|zip`: Magento customer login only (no Access App device slot).
    /// - `access=device`: Access App `activation/login` (registers a device).
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
            .ok_or_else(|| GraphicAudioError::auth("GraphicAudio login requires email"))?;
        let password = opts
            .password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GraphicAudioError::auth("GraphicAudio login requires password"))?;

        let marketplace = if opts.marketplace.trim().is_empty() {
            String::from("us")
        } else {
            opts.marketplace.trim().to_ascii_lowercase()
        };

        // Determine if there's an existing auth in DB to preserve client_id / device token.
        // Account id for GraphicAudio is the email (see GraphicAudioAuthFile::account_id).
        let account_id_candidate = email.to_string();
        let existing = load_auth_from_db(library, &account_id_candidate)
            .await
            .unwrap_or(None);

        if let Some(ref existing_auth) = existing {
            if !opts.force {
                return Ok(source_account_from_auth(existing_auth));
            }
        }

        let (token, client_id) = match self.access {
            GraphicAudioAccess::Device => {
                let client_id = existing
                    .as_ref()
                    .map(|a| a.client_id.clone())
                    .unwrap_or_else(|| format!("bookclerk-{}", uuid::Uuid::new_v4()));
                let mut client = GraphicAudioClient::new(&self.base_url);
                let token = client.login(email, password, &client_id).await?;
                (token, client_id)
            }
            GraphicAudioAccess::Web | GraphicAudioAccess::Zip => {
                let store = MagentoClient::new(&self.store_url)?;
                store.login(email, password).await?;
                let client_id = existing
                    .as_ref()
                    .map(|a| a.client_id.clone())
                    .unwrap_or_else(|| format!("bookclerk-{}", uuid::Uuid::new_v4()));
                let token = existing
                    .as_ref()
                    .map(|a| a.token.clone())
                    .unwrap_or_default();
                (token, client_id)
            }
        };

        let auth = GraphicAudioAuthFile {
            token,
            client_id,
            email: email.to_string(),
            marketplace,
            label: opts.label.clone(),
        };
        let account_id = auth.account_id().to_string();
        save_auth_to_db(&auth, library, &account_id)
            .await
            .map_err(|e| {
                GraphicAudioError::auth(format!("failed to save GraphicAudio auth: {e}"))
            })?;
        library
            .upsert_account(&account_id, &auth.marketplace, auth.label.as_deref(), true)
            .await
            .map_err(|e| {
                GraphicAudioError::auth(format!("failed to upsert GraphicAudio account: {e}"))
            })?;

        tracing::info!(
            email = %auth.email,
            access = ?self.access,
            has_device_token = auth.has_device_token(),
            "saved GraphicAudio auth to encrypted_secrets"
        );

        Ok(source_account_from_auth(&auth))
    }

    /// Delete a GraphicAudio account from the DB.
    pub async fn delete_account(&self, library: &SourceScope, account_id: &str) -> Result<()> {
        delete_auth_from_db(library, account_id).await
    }
}

#[async_trait]
impl ContentSource for GraphicAudioSource {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "GraphicAudio"
    }

    fn aliases(&self) -> &'static [&'static str] {
        ALIASES
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Password
    }

    fn portal_brand(&self) -> SourceBrand {
        SourceBrand {
            id: "graphicaudio",
            name: "GraphicAudio",
            bg: "#141414",
            fg: "#F5F5F5",
            accent: "#C41E3A",
            icon_url: "https://www.google.com/s2/favicons?domain=graphicaudio.net&sz=128",
        }
    }

    fn password_env_var(&self) -> Option<&'static str> {
        Some(GA_PASSWORD_ENV)
    }

    fn sort_key(&self) -> u32 {
        2
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
        let password = self.magento_password.clone().or_else(password_from_env);
        scan_library(
            library,
            GaScanOptions::from(&opts),
            crate::sync::ScanContext {
                access_base_url: Some(self.base_url.as_str()),
                store_base_url: Some(self.store_url.as_str()),
                access: self.access,
                magento_password: password.as_deref(),
            },
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
                    "no GraphicAudio credentials for account `{account_id}` in DB"
                ))
            })?;
        let _ = &opts.files_dir;
        let client = GraphicAudioClient::new(&self.base_url).with_token(&auth.token);
        let prefer_hi = self.bitrate.prefers_hi();
        let mode = self.fetch_mode.unwrap_or(self.access);

        let product_title = if matches!(mode, GraphicAudioAccess::Zip) && auth.has_device_token() {
            match product_title_for(&client, title_id).await {
                Ok(t) => t,
                Err(err) => {
                    tracing::debug!(error = %err, "could not resolve GraphicAudio product title");
                    None
                }
            }
        } else {
            None
        };

        let password = self.magento_password.clone().or_else(password_from_env);

        let plain = fetch_title_with_mode(
            &client,
            TitleFetchRequest {
                store_base_url: &self.store_url,
                email: &auth.email,
                product_id: title_id,
                product_title: product_title.as_deref(),
                cache_dir: &opts.cache_dir,
                prefer_hi,
                mode,
                password: password.as_deref(),
                zip_container: self.container,
            },
        )
        .await?;
        Ok(plain)
    }

    fn config_options(&self) -> &'static [bookclerk_source::SourceConfigOption] {
        GA_CONFIG_OPTIONS
    }

    async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
        let q = opts.query.trim();
        if q.is_empty() || opts.limit == 0 {
            return Ok(Vec::new());
        }
        let http = match catalog_http_client() {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };
        let page = opts.page.max(1);
        let products =
            match crate::catalog::search_catalog_page(&http, &self.store_url, q, page).await {
                Ok(p) => p,
                Err(err) => {
                    tracing::debug!(error = %err, "graphicaudio catalog search failed");
                    return Ok(Vec::new());
                }
            };
        Ok(products
            .into_iter()
            .take(opts.limit)
            .filter(|p| !p.product_id.trim().is_empty())
            .map(|p| ga_catalog_hit(&p, String::from("search")))
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
        let http = match catalog_http_client() {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };
        let mut by_id = std::collections::HashMap::new();

        if seed.source.eq_ignore_ascii_case(ID) && !seed.product_id.is_empty() {
            match expand_from_product_id(&http, Some(&self.store_url), &seed.product_id).await {
                Ok(products) => {
                    for p in products {
                        by_id.entry(p.product_id.clone()).or_insert_with(|| {
                            ga_catalog_hit(
                                &p,
                                format!("graphicaudio related/series for “{}”", seed.title),
                            )
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!(
                        id = %seed.product_id,
                        error = %err,
                        "graphicaudio product expand failed"
                    );
                }
            }
        }

        // Series / title Magento search for GA seeds or when series is known.
        let worth = seed.source.eq_ignore_ascii_case(ID)
            || seed
                .series
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
        if worth && by_id.len() < limit {
            let query = seed
                .series
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(seed.title.as_str());
            match expand_from_search(&http, Some(&self.store_url), query).await {
                Ok(products) => {
                    for p in products {
                        by_id.entry(p.product_id.clone()).or_insert_with(|| {
                            ga_catalog_hit(&p, format!("graphicaudio catalog search (“{query}”)"))
                        });
                    }
                }
                Err(err) => {
                    tracing::debug!(query, error = %err, "graphicaudio search failed");
                }
            }
        }

        let mut hits: Vec<_> = by_id.into_values().collect();
        hits.truncate(limit);
        Ok(hits)
    }

    async fn purchase_hint(
        &self,
        opts: &PurchaseHintOpts,
    ) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
        let mut hint = if let Some(pid) = opts
            .product_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(SourcePurchaseHint {
                product_id: pid.to_string(),
                title: opts.title.clone(),
                url: Some(format!(
                    "{}/catalog/product/view/id/{pid}",
                    self.store_url.trim_end_matches('/')
                )),
                ..Default::default()
            })
        } else {
            let title = opts.title.as_deref().unwrap_or("").trim();
            if title.is_empty() {
                None
            } else {
                let http = match catalog_http_client() {
                    Ok(c) => c,
                    Err(_) => return Ok(None),
                };
                let q = match opts
                    .authors
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    Some(a) => format!("{title} {a}"),
                    None => title.to_string(),
                };
                match crate::catalog::search_catalog(&http, &self.store_url, &q).await {
                    Ok(hits) => pick_ga_purchase_hit(title, &hits).map(|hit| {
                        let url = hit.url.clone().or_else(|| {
                            Some(format!(
                                "{}/catalog/product/view/id/{}",
                                self.store_url.trim_end_matches('/'),
                                hit.product_id
                            ))
                        });
                        SourcePurchaseHint {
                            product_id: hit.product_id.clone(),
                            title: Some(hit.title.clone()),
                            url,
                            ..Default::default()
                        }
                    }),
                    Err(err) => {
                        tracing::debug!(error = %err, "graphicaudio purchase search failed");
                        None
                    }
                }
            }
        };

        if opts.with_price {
            if let Some(ref mut h) = hint {
                if let Some(priced) =
                    fetch_ga_price(h.url.as_deref(), &h.product_id, &self.store_url).await
                {
                    h.price_cents = Some(priced.0);
                    h.currency = Some(String::from("USD"));
                    h.price_label = Some(priced.1);
                }
            }
        }
        Ok(hint)
    }
}

fn ga_catalog_hit(p: &MagentoCatalogProduct, origin: String) -> CatalogHit {
    CatalogHit {
        product_id: p.product_id.clone(),
        title: p.title.clone(),
        series: p.series.clone(),
        url: p.url.clone(),
        cover_url: p.cover_url.clone(),
        origin,
        ..Default::default()
    }
}

/// Pick a Magento hit that actually matches the queried title (not the first rank).
fn pick_ga_purchase_hit<'a>(
    query_title: &str,
    hits: &'a [MagentoCatalogProduct],
) -> Option<&'a MagentoCatalogProduct> {
    let q = normalize_ga_title(query_title);
    if q.is_empty() {
        return None;
    }
    hits.iter()
        .filter(|h| !h.is_series_set())
        .filter(|h| ga_titles_match(&q, &normalize_ga_title(&h.title)))
        .max_by(|a, b| {
            let sa = ga_title_score(&q, &normalize_ga_title(&a.title));
            let sb = ga_title_score(&q, &normalize_ga_title(&b.title));
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn normalize_ga_title(raw: &str) -> String {
    raw.chars()
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
}

fn ga_titles_match(query: &str, hit: &str) -> bool {
    if query.is_empty() || hit.is_empty() {
        return false;
    }
    if query == hit {
        return true;
    }
    // Contiguous phrase match only when the shorter is a large fraction of the longer
    // (avoids "man" / "ashes" token collisions with unrelated Magento series).
    let (shorter, longer) = if query.chars().count() <= hit.chars().count() {
        (query, hit)
    } else {
        (hit, query)
    };
    if shorter.chars().count() < 8 {
        return false;
    }
    let ratio = shorter.chars().count() as f32 / longer.chars().count() as f32;
    ratio >= 0.55
        && (longer == shorter
            || longer.starts_with(&format!("{shorter} "))
            || longer.ends_with(&format!(" {shorter}"))
            || longer.contains(&format!(" {shorter} ")))
}

fn ga_title_score(query: &str, hit: &str) -> f32 {
    if query == hit {
        return 1.0;
    }
    let (shorter, longer) = if query.len() <= hit.len() {
        (query, hit)
    } else {
        (hit, query)
    };
    if longer.contains(shorter) {
        shorter.len() as f32 / longer.len() as f32
    } else {
        0.0
    }
}

async fn fetch_ga_price(
    product_url: Option<&str>,
    product_id: &str,
    store_url: &str,
) -> Option<(i64, String)> {
    let http = catalog_http_client().ok()?;
    let url = product_url.map(str::to_string).unwrap_or_else(|| {
        format!(
            "{}/catalog/product/view/id/{product_id}",
            store_url.trim_end_matches('/')
        )
    });
    let resp = http.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let html = resp.text().await.ok()?;
    parse_ga_price_html(&html)
}

fn parse_ga_price_html(html: &str) -> Option<(i64, String)> {
    if let Some(idx) = html.find("data-price-amount=\"") {
        let rest = &html[idx + "data-price-amount=\"".len()..];
        let end = rest.find('"')?;
        let raw = &rest[..end];
        if let Ok(amount) = raw.parse::<f64>() {
            let cents = (amount * 100.0).round() as i64;
            return Some((cents.max(0), format_usd(cents)));
        }
    }
    for marker in ["price-wrapper", "product-info-price", "price-box"] {
        if let Some(idx) = html.find(marker) {
            let window = &html[idx..html.len().min(idx + 800)];
            if let Some(cents) = window
                .split('$')
                .nth(1)
                .and_then(|s| parse_money_label_to_cents(&format!("${}", &s[..s.len().min(12)])))
            {
                return Some((cents, format_usd(cents)));
            }
        }
    }
    None
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

fn format_usd(cents: i64) -> String {
    if cents <= 0 {
        return String::from("FREE");
    }
    format!("${}.{:02}", cents / 100, (cents % 100).unsigned_abs())
}

const GA_CONFIG_OPTIONS: &[bookclerk_source::SourceConfigOption] = &[
    bookclerk_source::SourceConfigOption {
        key: "access",
        label: "Access",
        values: &[
            bookclerk_source::ConfigOptionValue {
                id: "web",
                label: "Browser Player",
            },
            bookclerk_source::ConfigOptionValue {
                id: "zip",
                label: "Magento ZIP",
            },
            bookclerk_source::ConfigOptionValue {
                id: "device",
                label: "Access App",
            },
        ],
    },
    bookclerk_source::SourceConfigOption {
        key: "bitrate",
        label: "Bitrate",
        values: &[
            bookclerk_source::ConfigOptionValue {
                id: "hi",
                label: "Hi",
            },
            bookclerk_source::ConfigOptionValue {
                id: "lo",
                label: "Lo",
            },
        ],
    },
    bookclerk_source::SourceConfigOption {
        key: "container",
        label: "Container",
        values: &[
            bookclerk_source::ConfigOptionValue {
                id: "auto",
                label: "Auto",
            },
            bookclerk_source::ConfigOptionValue {
                id: "m4b",
                label: "M4B",
            },
            bookclerk_source::ConfigOptionValue {
                id: "mp3",
                label: "MP3",
            },
            bookclerk_source::ConfigOptionValue {
                id: "flac",
                label: "FLAC",
            },
        ],
    },
];

fn source_account_from_auth(auth: &GraphicAudioAuthFile) -> SourceAccount {
    SourceAccount {
        account_id: auth.account_id().to_string(),
        source: ID.into(),
        marketplace: auth.marketplace.clone(),
        label: auth.label.clone().or_else(|| Some(auth.email.clone())),
        scan_enabled: true,
    }
}

/// Parse `[sources.graphicaudio]` into a [`GraphicAudioSource`].
#[must_use]
pub fn from_config(config: &Config) -> GraphicAudioSource {
    GraphicAudioSource::from_config(config)
}

/// Register GraphicAudio when `[sources.graphicaudio] enabled` (default true).
pub fn register(registry: &mut SourceRegistry, config: &Config) {
    if config.sources.is_enabled(ID) {
        registry.register(Arc::new(from_config(config)));
    }
}
