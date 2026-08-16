//! [`ContentSource`] adapter over an external plugin process.
//!
//! # Security
//!
//! External plugins are untrusted. This host adapter:
//! - never passes `library.db` or the Bookclerk files-dir root
//! - gives only a scoped `plugin_data_dir` (`…/plugins/<id>/data`) and fetch
//!   scratch under the guest `TMPDIR` (`…/plugins/<id>/tmp/fetch`)
//! - seals login credentials via [`SourceScope`] (`provider = plugin id`)
//! - loads those credentials for `scan` and `fetch_title` (plugin never opens the DB)
//! - upserts scan book DTOs via [`SourceScope`] with `source` forced to the plugin id
//!
//! First-party in-process adapters use the same [`SourceScope`] boundary.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::{NewBook, SourceScope};
use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ContentSource, ExpandSeed, FetchOptions, LoginOptions,
    OAuthProgress, PlainAudioPart, PlainFetch, PortalAuthMode, PurchaseHintOpts, ScanOptions,
    ScanSummary, SourceAccount, SourceBrand, SourceFetch, SourcePurchaseHint, SourceRegistry,
};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::protocol::{
    CatalogDetailParams, CatalogHitDto, ExpandCandidatesParams, FetchTitleParams,
    LoginCompleteParams, LoginParams, LoginResultDto, LoginStartResultDto, PurchaseHintDto,
    PurchaseHintParams, ScanBookDto, ScanParams, ScanSummaryDto, SearchCatalogParams,
    SourceAccountDto, SourceFetchDto,
};
use crate::rpc_v2::{V2PluginSession, HOST_SHARED_ACCOUNT};
use crate::Result;
use bookclerk_plugin_sdk::v2::PRODUCT_API_VERSION;

/// External content source backed by a discovered plugin binary.
pub struct ExternalSource {
    /// Cap'n Proto v2 session (never given `library.db`).
    session: Arc<V2PluginSession>,
    /// JSON factory context (plugin config table).
    ctx_json: String,
    /// Operator-facing storefront name from handshake or the manifest.
    display_name: String,
    /// UI brand colors and icon from handshake, or a slate fallback.
    brand: SourceBrand,
    /// `oauth` vs password login, from the guest handshake.
    auth_mode: PortalAuthMode,
    /// Leaked handshake aliases used as extra storefront ids.
    aliases: &'static [&'static str],
    /// Optional env var the guest accepts for a password (never put on argv).
    password_env: Option<&'static str>,
    /// Registry sort order from handshake (`200` when the guest omits it).
    sort_key: u32,
    /// Scoped data directory for this plugin only.
    plugin_data_dir: PathBuf,
    /// `[sources.<id>]` table from main config (also sent on handshake).
    source_config: Value,
}

impl ExternalSource {
    /// Spawn and handshake a source plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> Result<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(crate::PluginError::message(format!(
                "plugin `{}` api_version {} is not v2",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let session = Arc::new(
            V2PluginSession::spawn_for_account(
                plugin,
                config,
                config_json.clone(),
                HOST_SHARED_ACCOUNT,
            )
            .await?,
        );
        let source_config = crate::handshake_config_for_grant(session.grant(), config_json);
        let hs = session.handshake_metadata();
        let display_name = hs
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        let brand = brand_from_dto(hs.brand.as_ref(), &plugin.manifest.id, &display_name);
        let auth_mode = match hs.portal_auth_mode.as_deref() {
            Some("oauth") => PortalAuthMode::Oauth,
            _ => PortalAuthMode::Password,
        };
        let aliases = leak_str_slice(&hs.aliases, &[]);
        let password_env = hs
            .password_env_var
            .as_deref()
            .map(|s| Box::leak(s.to_string().into_boxed_str()) as &'static str);
        let plugin_data_dir = plugin_data_dir(config, &plugin.manifest.id)?;
        let ctx_json = source_config.to_string();
        Ok(Self {
            session,
            ctx_json,
            display_name,
            brand,
            auth_mode,
            aliases,
            password_env,
            sort_key: hs.sort_key.unwrap_or(200),
            plugin_data_dir,
            source_config,
        })
    }

    /// Forwards one content-source RPC through the v2 session and deserializes the JSON result.
    ///
    /// # Errors
    ///
    /// Returns when the session call fails, params cannot be serialized, or the JSON result cannot be decoded.
    async fn cs_call<T: serde::de::DeserializeOwned>(
        &self,
        op: &str,
        params: Value,
    ) -> bookclerk_source::Result<T> {
        let raw = self
            .session
            .content_source_json(
                self.ctx_json.clone(),
                op,
                serde_json::to_string(&params)
                    .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await
            .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
        serde_json::from_str(&raw).map_err(|e| bookclerk_source::SourceError::api(e.to_string()))
    }

    /// True when the guest advertised OAuth plus `loginStart`/`loginComplete`.
    fn supports_oauth_rpc(&self) -> bool {
        self.auth_mode == PortalAuthMode::Oauth
            && self.session.has_capability("loginStart")
            && self.session.has_capability("loginComplete")
    }

    /// Builds guest login params; host fills callback IPC after starting the proxy.
    fn login_params(plugin_data_dir: String, opts: LoginOptions) -> LoginParams {
        LoginParams {
            plugin_data_dir,
            marketplace: opts.marketplace,
            label: opts.label,
            email: opts.email,
            password: opts.password,
            force: opts.force,
            callback_bind: opts.callback_bind,
            callback_ipc: None,
            callback_public_base: None,
            external: opts.external,
            response_url: opts.response_url,
            show_qr: opts.show_qr,
            timeout_secs: opts.timeout_secs,
            extra: opts.extra,
        }
    }

    /// Password login RPC; requires the `secrets` binding when a password is sent.
    async fn password_login(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        if opts.password.is_some() {
            self.session.require_binding("secrets")?;
        }
        let result: LoginResultDto = self
            .cs_call(
                "login",
                serde_json::to_value(Self::login_params(
                    self.plugin_data_dir.display().to_string(),
                    opts,
                ))
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        seal_login_result(scope, self.id(), result).await
    }

    /// Host-owned OAuth callback proxy plus `loginStart`/`loginComplete` RPCs.
    async fn oauth_login(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
        on_progress: &(dyn Fn(OAuthProgress) + Send + Sync),
    ) -> bookclerk_source::Result<SourceAccount> {
        self.session.require_binding("oauth")?;
        // Host owns the browser TCP listener and forwards bytes to the guest
        // over IPC — required under Windows AppContainer loopback isolation.
        let proxy = crate::callback_proxy::CallbackProxy::start(
            opts.callback_bind.as_deref(),
            self.session.scratch_dir(),
            self.session.package_sid(),
        )
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;

        let mut params = Self::login_params(self.plugin_data_dir.display().to_string(), opts);
        params.callback_ipc = Some(proxy.ipc_endpoint.clone());
        params.callback_public_base = Some(proxy.public_base.clone());

        let start: LoginStartResultDto = self
            .cs_call(
                "loginStart",
                serde_json::to_value(params)
                    .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        on_progress(OAuthProgress::LoginUrl {
            url: start.url.clone(),
            qr: None,
        });
        on_progress(OAuthProgress::CallbackListening {
            addr: proxy.bind_addr().to_string(),
        });
        on_progress(OAuthProgress::WaitingForCallback);
        let result: LoginResultDto = self
            .cs_call(
                "loginComplete",
                serde_json::to_value(LoginCompleteParams {
                    session_id: start.session_id,
                })
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        drop(proxy);
        let account = seal_login_result(scope, self.id(), result).await?;
        on_progress(OAuthProgress::Completed {
            account_id: account.account_id.clone(),
        });
        Ok(account)
    }
}

/// Discover and register external source plugins.
///
/// Duplicate `(kind, id)` claims among discovered manifests are a hard error
/// (from [`crate::discover_plugins`]). When an external id is already registered
/// in-process (dual-load `register()` path), the external copy is skipped so
/// `cargo run` keeps the linked adapter.
///
/// # Errors
///
/// Returns an error when the operation fails.
pub async fn load_external_sources(config: &Config, registry: &mut SourceRegistry) -> Result<()> {
    for plugin in crate::discover_plugins(config)? {
        if plugin.manifest.kind != crate::PluginKind::Source {
            continue;
        }
        if !config.sources.is_enabled(&plugin.manifest.id) {
            continue;
        }
        if registry.get(&plugin.manifest.id).is_some() {
            tracing::debug!(
                id = %plugin.manifest.id,
                path = %plugin.root.join("plugin.toml").display(),
                "skipping external source — already registered in-process"
            );
            continue;
        }
        match ExternalSource::spawn(&plugin, config).await {
            Ok(s) => {
                tracing::info!(id = %plugin.manifest.id, "loaded external source plugin");
                registry.register(Arc::new(s));
            }
            Err(err) => {
                tracing::warn!(id = %plugin.manifest.id, %err, "skipping external source plugin");
            }
        }
    }
    Ok(())
}

#[async_trait]
impl ContentSource for ExternalSource {
    fn id(&self) -> &str {
        self.session.id()
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn aliases(&self) -> &'static [&'static str] {
        self.aliases
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        self.auth_mode
    }

    fn portal_brand(&self) -> SourceBrand {
        self.brand
    }

    fn password_env_var(&self) -> Option<&'static str> {
        self.password_env
    }

    fn sort_key(&self) -> u32 {
        self.sort_key
    }

    async fn login(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        if self.supports_oauth_rpc() {
            return self.oauth_login(scope, opts, &|_| {}).await;
        }
        self.password_login(scope, opts).await
    }

    async fn login_with_oauth_progress(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
        on_progress: &(dyn Fn(OAuthProgress) + Send + Sync),
    ) -> bookclerk_source::Result<SourceAccount> {
        if self.supports_oauth_rpc() {
            return self.oauth_login(scope, opts, on_progress).await;
        }
        self.password_login(scope, opts).await
    }

    async fn list_accounts(
        &self,
        scope: &SourceScope,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        // Host-mediated: accounts table rows for this source id (never ask plugin to open DB).
        let all = scope
            .list_accounts()
            .await
            .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
        Ok(all
            .into_iter()
            .map(|a| SourceAccount {
                account_id: a.account_id,
                source: a.source,
                marketplace: a.marketplace,
                label: a.label,
                scan_enabled: a.scan_enabled,
            })
            .collect())
    }

    async fn scan(
        &self,
        scope: &SourceScope,
        opts: ScanOptions,
    ) -> bookclerk_source::Result<ScanSummary> {
        let credentials = scan_credentials_for(scope, &opts.accounts).await?;
        if !credentials.is_empty() {
            self.session.require_binding("secrets")?;
        }
        let dto: ScanSummaryDto = self
            .cs_call(
                "scan",
                serde_json::to_value(ScanParams {
                    plugin_data_dir: self.plugin_data_dir.display().to_string(),
                    accounts: opts.accounts,
                    page_size: opts.page_size,
                    import_episodes: opts.import_episodes,
                    import_plus_titles: opts.import_plus_titles,
                    credentials,
                })
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        let mut upserted = 0usize;
        for book in dto.books {
            scope
                .upsert_book(&scan_book_to_new(self.id(), book))
                .await
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
            upserted += 1;
        }
        Ok(ScanSummary {
            accounts: dto.accounts,
            books_upserted: if upserted > 0 {
                upserted
            } else {
                dto.books_upserted
            },
            pages: dto.pages,
            skipped_disabled: dto.skipped_disabled,
        })
    }

    async fn fetch_title(
        &self,
        scope: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> bookclerk_source::Result<SourceFetch> {
        let credentials = scope
            .load_credentials_json(account_id)
            .await
            .map_err(|e| bookclerk_source::SourceError::Auth(e.to_string()))?;
        if credentials.is_some() {
            self.session.require_binding("secrets")?;
        }
        let download = serde_json::to_value(&opts.download)
            .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
        // Jail-granted scratch (already TMPDIR), not the host download cache.
        let cache_dir = {
            let dir = self.session.scratch_dir().join("fetch");
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
            dir
        };
        let dto: SourceFetchDto = self
            .cs_call(
                "fetchTitle",
                serde_json::to_value(FetchTitleParams {
                    plugin_data_dir: self.plugin_data_dir.display().to_string(),
                    account_id: account_id.to_string(),
                    title_id: title_id.to_string(),
                    cache_dir: cache_dir.display().to_string(),
                    credentials,
                    source_config: self.source_config.clone(),
                    download,
                })
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await?;
        Ok(source_fetch_from_dto(dto))
    }

    async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
        if !self.session.has_capability("searchCatalog") {
            return Ok(Vec::new());
        }
        let params = SearchCatalogParams {
            query: opts.query.clone(),
            region: opts.region.clone(),
            limit: opts.limit,
            page: opts.page.max(1),
            sort: Some(opts.sort.as_wire().to_string()),
            field: opts.field.map(|f| f.as_wire().to_string()),
            language: opts.language.clone(),
        };
        match self
            .cs_call::<Vec<CatalogHitDto>>(
                "searchCatalog",
                serde_json::to_value(params)
                    .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await
        {
            Ok(hits) => Ok(hits.into_iter().map(catalog_hit_from_dto).collect()),
            Err(err) => {
                tracing::warn!(
                    plugin = %self.id(),
                    error = %err,
                    "external search_catalog soft-failed"
                );
                Ok(Vec::new())
            }
        }
    }

    async fn catalog_detail(
        &self,
        product_id: &str,
    ) -> bookclerk_source::Result<Option<CatalogHit>> {
        if !self.session.has_capability("catalogDetail") {
            return Ok(None);
        }
        let params = CatalogDetailParams {
            product_id: product_id.to_string(),
            isbn: None,
        };
        match self
            .cs_call::<Option<CatalogHitDto>>(
                "catalogDetail",
                serde_json::to_value(params)
                    .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await
        {
            Ok(hit) => Ok(hit.map(catalog_hit_from_dto)),
            Err(err) => {
                tracing::debug!(
                    plugin = %self.id(),
                    error = %err,
                    "external catalog_detail soft-failed"
                );
                Ok(None)
            }
        }
    }

    async fn expand_candidates(
        &self,
        seed: &ExpandSeed,
        limit: usize,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
        if !self.session.has_capability("expandCandidates") {
            return Ok(Vec::new());
        }
        let params = ExpandCandidatesParams {
            source: seed.source.clone(),
            product_id: seed.product_id.clone(),
            title: seed.title.clone(),
            authors: seed.authors.clone(),
            narrators: seed.narrators.clone(),
            series: seed.series.clone(),
            series_asin: seed.series_asin.clone(),
            asin: seed.asin.clone(),
            isbn: seed.isbn.clone(),
            region: seed.region.clone(),
            limit,
        };
        match self
            .cs_call::<Vec<CatalogHitDto>>(
                "expandCandidates",
                serde_json::to_value(params)
                    .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await
        {
            Ok(hits) => Ok(hits.into_iter().map(catalog_hit_from_dto).collect()),
            Err(err) => {
                tracing::debug!(
                    plugin = %self.id(),
                    error = %err,
                    "external expand_candidates soft-failed"
                );
                Ok(Vec::new())
            }
        }
    }

    async fn purchase_hint(
        &self,
        opts: &PurchaseHintOpts,
    ) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
        if !self.session.has_capability("purchaseHint") {
            return Ok(None);
        }
        let params = PurchaseHintParams {
            product_id: opts.product_id.clone(),
            title: opts.title.clone(),
            authors: opts.authors.clone(),
            asin: opts.asin.clone(),
            isbn: opts.isbn.clone(),
            region: opts.region.clone(),
            with_price: opts.with_price,
        };
        match self
            .cs_call::<Option<PurchaseHintDto>>(
                "purchaseHint",
                serde_json::to_value(params)
                    .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?,
            )
            .await
        {
            Ok(hint) => Ok(hint.map(purchase_hint_from_dto)),
            Err(err) => {
                tracing::debug!(
                    plugin = %self.id(),
                    error = %err,
                    "external purchase_hint soft-failed"
                );
                Ok(None)
            }
        }
    }

    async fn list_deals(&self, limit: usize) -> bookclerk_source::Result<Vec<CatalogHit>> {
        if !self.session.has_capability("listDeals") {
            return Ok(Vec::new());
        }
        match self
            .cs_call::<Vec<CatalogHitDto>>("listDeals", serde_json::json!({ "limit": limit }))
            .await
        {
            Ok(hits) => Ok(hits.into_iter().map(catalog_hit_from_dto).collect()),
            Err(err) => {
                tracing::debug!(
                    plugin = %self.id(),
                    error = %err,
                    "external list_deals soft-failed"
                );
                Ok(Vec::new())
            }
        }
    }
}

/// Map a protocol [`SourceFetchDto`] to the host [`SourceFetch`] (`PlainFetch`).
#[must_use]
pub(crate) fn source_fetch_from_dto(dto: SourceFetchDto) -> SourceFetch {
    match dto {
        SourceFetchDto::Plain {
            parts,
            m4b_path,
            cover_path,
            chapters,
            pdf_url,
        } => PlainFetch {
            parts: parts
                .into_iter()
                .map(|p| PlainAudioPart {
                    path: PathBuf::from(p.path),
                    title: p.title,
                    duration_ms: p.duration_ms,
                })
                .collect(),
            m4b_path: m4b_path.map(PathBuf::from),
            cover_path: cover_path.map(PathBuf::from),
            chapters,
            pdf_url,
        },
    }
}

/// Maps a guest catalog DTO onto a host [`CatalogHit`], decoding HTML entities.
fn catalog_hit_from_dto(dto: CatalogHitDto) -> CatalogHit {
    CatalogHit {
        product_id: dto.product_id,
        title: dto.title,
        authors: dto.authors,
        narrators: dto.narrators,
        series: dto.series,
        series_index: dto.series_index,
        asin: dto.asin,
        isbn: dto.isbn,
        url: dto.url,
        cover_url: dto.cover_url,
        origin: dto.origin,
        subtitle: dto.subtitle,
        description: dto.description,
        publisher: dto.publisher,
        length_minutes: dto.length_minutes,
        published_at: dto.published_at,
        categories: dto.categories,
        language: dto.language,
        price_cents: dto.price_cents,
        currency: dto.currency,
        price_label: dto.price_label,
        rating_overall: dto.rating_overall,
        rating_count: dto.rating_count,
        is_abridged: dto.is_abridged,
    }
    .decode_html_entities()
}

/// Maps a guest purchase-hint DTO onto a host hint, decoding HTML entities.
fn purchase_hint_from_dto(dto: PurchaseHintDto) -> SourcePurchaseHint {
    SourcePurchaseHint {
        product_id: dto.product_id,
        title: dto.title,
        url: dto.url,
        price_cents: dto.price_cents,
        currency: dto.currency,
        price_label: dto.price_label,
        list_price_cents: dto.list_price_cents,
        list_price_label: dto.list_price_label,
        member_price_cents: dto.member_price_cents,
        member_price_label: dto.member_price_label,
    }
    .decode_html_entities()
}

/// Load host-sealed credentials for the accounts a scan will cover.
///
/// Empty `accounts` filter → all scoped accounts that have credentials.
/// Explicit account needles match `account_id` or label (case-insensitive).
async fn scan_credentials_for(
    scope: &SourceScope,
    account_filter: &[String],
) -> bookclerk_source::Result<std::collections::BTreeMap<String, Value>> {
    let accounts = scope
        .list_accounts()
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    let explicit = !account_filter.is_empty();
    let mut out = std::collections::BTreeMap::new();
    for acct in accounts {
        if explicit {
            let matched = account_filter.iter().any(|needle| {
                acct.account_id.eq_ignore_ascii_case(needle)
                    || acct
                        .label
                        .as_deref()
                        .is_some_and(|l| l.eq_ignore_ascii_case(needle))
            });
            if !matched {
                continue;
            }
        } else if !acct.scan_enabled {
            continue;
        }
        match scope.load_credentials_json(&acct.account_id).await {
            Ok(Some(creds)) => {
                out.insert(acct.account_id, creds);
            }
            Ok(None) => {}
            Err(e) => {
                return Err(bookclerk_source::SourceError::Auth(e.to_string()));
            }
        }
    }
    Ok(out)
}

/// Maps a scan DTO onto [`NewBook`], forcing `source` to the plugin id.
fn scan_book_to_new(plugin_id: &str, book: ScanBookDto) -> NewBook {
    NewBook {
        uuid: None,
        product_id: book.product_id.clone(),
        source: plugin_id.to_string(),
        account_id: book.account_id,
        asin: book.asin,
        isbn: book.isbn,
        marketplace: book.marketplace.unwrap_or_else(|| String::from("us")),
        title: book.title,
        authors: book.authors,
        narrators: book.narrators,
        series: book.series,
        series_index: book.series_index,
        series_asin: None,
        purchased_at: None,
        publisher: book.publisher,
        length_minutes: book.length_minutes,
        is_abridged: false,
        content_kind: book.content_kind.unwrap_or_else(|| String::from("book")),
        categories: None,
        subtitle: book.subtitle,
        published_at: None,
    }
}

/// Maps a guest account DTO onto a host [`SourceAccount`].
fn account_from_dto(dto: SourceAccountDto) -> SourceAccount {
    SourceAccount {
        account_id: dto.account_id,
        source: dto.source,
        marketplace: dto.marketplace,
        label: dto.label,
        scan_enabled: dto.scan_enabled,
    }
}

/// Upserts the account and seals guest credentials via [`SourceScope`] (plugin cannot write the DB).
async fn seal_login_result(
    scope: &SourceScope,
    plugin_id: &str,
    result: LoginResultDto,
) -> bookclerk_source::Result<SourceAccount> {
    let mut account = account_from_dto(result.account);
    // Force source id to the plugin id — plugins cannot claim another storefront.
    account.source = plugin_id.to_string();
    scope
        .upsert_account(
            &account.account_id,
            &account.marketplace,
            account.label.as_deref(),
            account.scan_enabled,
        )
        .await
        .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
    if let Some(creds) = result.credentials {
        scope
            .save_credentials_json(&account.account_id, &creds)
            .await
            .map_err(|e| bookclerk_source::SourceError::Auth(e.to_string()))?;
    }
    Ok(account)
}

/// Leaks handshake strings into `'static` slices for [`SourceBrand`] / aliases.
fn leak_str_slice(owned: &[String], fallback: &[&'static str]) -> &'static [&'static str] {
    if owned.is_empty() {
        return Box::leak(fallback.to_vec().into_boxed_slice());
    }
    let leaked: Vec<&'static str> = owned
        .iter()
        .map(|s| Box::leak(s.clone().into_boxed_str()) as &'static str)
        .collect();
    Box::leak(leaked.into_boxed_slice())
}

/// Builds a [`SourceBrand`] from handshake, or a slate fallback using plugin id/name.
fn brand_from_dto(dto: Option<&crate::protocol::BrandDto>, id: &str, name: &str) -> SourceBrand {
    if let Some(b) = dto {
        SourceBrand {
            id: Box::leak(b.id.clone().into_boxed_str()),
            name: Box::leak(b.name.clone().into_boxed_str()),
            bg: Box::leak(b.bg.clone().into_boxed_str()),
            fg: Box::leak(b.fg.clone().into_boxed_str()),
            accent: Box::leak(b.accent.clone().into_boxed_str()),
            icon_url: Box::leak(b.icon_url.clone().into_boxed_str()),
        }
    } else {
        SourceBrand {
            id: Box::leak(id.to_string().into_boxed_str()),
            name: Box::leak(name.to_string().into_boxed_str()),
            bg: "#334155",
            fg: "#f8fafc",
            accent: "#64748b",
            icon_url: "",
        }
    }
}

/// Converts a TOML value tree into JSON for handshake `config` delivery.
fn toml_to_json(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s.clone()),
        toml::Value::Integer(i) => Value::Number((*i).into()),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(a) => Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_library::configure_master_key;
    use tempfile::tempdir;

    #[tokio::test]
    async fn scan_credentials_only_from_this_scope() {
        let dir = tempdir().unwrap();
        configure_master_key(dir.path()).unwrap();
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let echo = store.scope("echo");
        let other = store.scope("other");

        echo.upsert_account("a1", "us", Some("Echo"), true)
            .await
            .unwrap();
        other
            .upsert_account("b1", "us", Some("Other"), true)
            .await
            .unwrap();
        echo.save_credentials_json("a1", &serde_json::json!({"token": "echo-secret"}))
            .await
            .unwrap();
        other
            .save_credentials_json("b1", &serde_json::json!({"token": "other-secret"}))
            .await
            .unwrap();

        let creds = scan_credentials_for(&echo, &[]).await.unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds["a1"]["token"], "echo-secret");
        assert!(!creds.contains_key("b1"));
    }

    #[tokio::test]
    async fn scan_credentials_skips_scan_disabled_unless_explicit() {
        let dir = tempdir().unwrap();
        configure_master_key(dir.path()).unwrap();
        let store = bookclerk_plugin_database_sqlite::open_store_memory()
            .await
            .unwrap();
        let echo = store.scope("echo");
        echo.upsert_account("a1", "us", None, false).await.unwrap();
        echo.save_credentials_json("a1", &serde_json::json!({"t": 1}))
            .await
            .unwrap();

        assert!(scan_credentials_for(&echo, &[]).await.unwrap().is_empty());
        let explicit = scan_credentials_for(&echo, &["a1".into()]).await.unwrap();
        assert_eq!(explicit.len(), 1);
    }

    #[test]
    fn scan_book_forces_plugin_source() {
        let book = ScanBookDto {
            account_id: "a".into(),
            product_id: "p".into(),
            title: "T".into(),
            marketplace: None,
            asin: None,
            isbn: None,
            authors: None,
            narrators: None,
            series: None,
            series_index: None,
            content_kind: None,
            publisher: None,
            length_minutes: None,
            subtitle: None,
        };
        let new = scan_book_to_new("echo", book);
        assert_eq!(new.source, "echo");
    }

    #[test]
    fn source_fetch_dto_maps_pdf_url() {
        let dto = SourceFetchDto::Plain {
            parts: vec![],
            m4b_path: Some("/tmp/book.m4b".into()),
            cover_path: None,
            chapters: vec![("Ch 1".into(), 0)],
            pdf_url: Some("https://cdn.example/book.pdf".into()),
        };
        let plain = source_fetch_from_dto(dto);
        assert_eq!(
            plain.pdf_url.as_deref(),
            Some("https://cdn.example/book.pdf")
        );
        assert_eq!(
            plain.m4b_path.as_deref().map(|p| p.to_string_lossy()),
            Some("/tmp/book.m4b".into())
        );
        assert_eq!(plain.chapters.len(), 1);
    }

    #[test]
    fn source_fetch_dto_pdf_url_roundtrip_serde() {
        let dto = SourceFetchDto::Plain {
            parts: vec![],
            m4b_path: None,
            cover_path: None,
            chapters: vec![],
            pdf_url: Some("https://x/y.pdf".into()),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["pdfUrl"], "https://x/y.pdf");
        assert!(json.get("pdf_url").is_none());
        let back: SourceFetchDto = serde_json::from_value(json).unwrap();
        let plain = source_fetch_from_dto(back);
        assert_eq!(plain.pdf_url.as_deref(), Some("https://x/y.pdf"));
    }
}
