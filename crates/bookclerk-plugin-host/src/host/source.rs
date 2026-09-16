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
use bookclerk_library::SourceScope;
use bookclerk_plugin_sdk::{ContentSourceContext, ExtensibleConfig, PRODUCT_API_VERSION};
use bookclerk_source::abi::{
    self as source_abi, account_credentials, credentials_from_bytes, credentials_to_bytes,
    expand_candidates_params, scan_book_to_new, scan_summary_from_abi, DEFAULT_EXTERNAL_SORT_KEY,
    DEFAULT_LIST_DEALS_LIMIT,
};
use bookclerk_source::{
    CatalogHit, CatalogSearchOpts, ContentSource, ExpandSeed, FetchOptions, LoginOptions,
    OAuthProgress, PortalAuthMode, PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount,
    SourceBrand, SourceFetch, SourcePurchaseHint, SourceRegistry,
};
use serde_json::Value;

use crate::discover::DiscoveredPlugin;
use crate::jail::plugin_data_dir;
use crate::protocol::{
    CatalogDetailParams, FetchTitleParams, ListDealsParams, LoginCompleteParams, LoginParams,
    LoginResult, ScanParams, SearchCatalogParams,
};
use crate::rpc_session::{PluginSession, HOST_SHARED_ACCOUNT};
use crate::Result;

/// External content source backed by a discovered plugin binary.
pub struct ExternalSource {
    /// Cap'n Proto session (never given `library.db`).
    session: Arc<PluginSession>,
    /// Granted factory context (plugin config table as JSON config).
    ctx: ContentSourceContext,
    /// Operator-facing storefront name from `describe()` or the manifest.
    display_name: String,
    /// UI brand colors and icon from `describe()`, or a slate fallback.
    brand: SourceBrand,
    /// `oauth` vs password login, from `describe()`.
    auth_mode: PortalAuthMode,
    /// Leaked `describe()` aliases used as extra storefront ids.
    aliases: &'static [&'static str],
    /// Optional env var the guest accepts for a password (never put on argv).
    password_env: Option<&'static str>,
    /// Registry sort order from `describe()` (`200` when the guest omits it).
    sort_key: u32,
    /// Scoped data directory for this plugin only.
    plugin_data_dir: PathBuf,
    /// `[sources.<id>]` table from main config (also delivered in the spawn config).
    source_config: Value,
}

impl ExternalSource {
    /// Spawn and describe a source plugin.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation fails.
    pub async fn spawn(plugin: &DiscoveredPlugin, config: &Config) -> Result<Self> {
        if plugin.manifest.api_version != PRODUCT_API_VERSION {
            return Err(crate::PluginError::message(format!(
                "plugin `{}` api_version {} is not supported",
                plugin.manifest.id, plugin.manifest.api_version
            )));
        }
        let table = crate::settings_table(config, plugin);
        let config_json = toml_to_json(&toml::Value::Table(table));
        let session = Arc::new(
            PluginSession::spawn_for_account(
                plugin,
                config,
                config_json.clone(),
                HOST_SHARED_ACCOUNT,
            )
            .await?,
        );
        let source_config = crate::spawn_config_for_grant(session.grant(), config_json);
        let describe = session.describe_snapshot();
        let display_name = describe
            .display_name
            .clone()
            .or_else(|| plugin.manifest.name.clone())
            .unwrap_or_else(|| plugin.manifest.id.clone());
        let brand = brand_from_abi(describe.brand.as_ref(), &plugin.manifest.id, &display_name);
        let auth_mode = match describe.portal_auth_mode {
            bookclerk_plugin_sdk::PortalAuthMode::Oauth => PortalAuthMode::Oauth,
            bookclerk_plugin_sdk::PortalAuthMode::Password
            | bookclerk_plugin_sdk::PortalAuthMode::Unspecified => PortalAuthMode::Password,
        };
        let aliases = leak_str_slice(&describe.aliases, &[]);
        let password_env = describe
            .password_env_var
            .as_deref()
            .map(|s| Box::leak(s.to_string().into_boxed_str()) as &'static str);
        let sort_key = if describe.sort_key == 0 {
            DEFAULT_EXTERNAL_SORT_KEY
        } else {
            describe.sort_key
        };
        let plugin_data_dir = plugin_data_dir(config, &plugin.manifest.id)?;
        let ctx = ContentSourceContext {
            config: ExtensibleConfig::json(&source_config),
        };
        Ok(Self {
            session,
            ctx,
            display_name,
            brand,
            auth_mode,
            aliases,
            password_env,
            sort_key,
            plugin_data_dir,
            source_config,
        })
    }

    /// Runs one typed content-source method through the plugin session.
    ///
    /// # Errors
    ///
    /// Returns when the factory or the guest method fails.
    async fn cs_call<T, F, Fut>(&self, call: F) -> bookclerk_source::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Box<dyn bookclerk_plugin_sdk::ContentSource>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::result::Result<T, bookclerk_plugin_sdk::PluginError>>
            + 'static,
    {
        self.session
            .content_source(self.ctx.clone(), call)
            .await
            .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))
    }

    /// True when the guest advertised OAuth plus `loginStart`/`loginComplete`.
    fn supports_oauth_rpc(&self) -> bool {
        self.auth_mode == PortalAuthMode::Oauth
            && self.session.has_capability("loginStart")
            && self.session.has_capability("loginComplete")
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
        let params = source_abi::login_params(self.plugin_data_dir.display().to_string(), opts);
        let result = self
            .cs_call(move |src| async move { src.login(params).await })
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

        let mut params: LoginParams =
            source_abi::login_params(self.plugin_data_dir.display().to_string(), opts);
        params.callback_ipc = Some(proxy.ipc_endpoint.clone());
        params.callback_public_base = Some(proxy.public_base.clone());

        let start = self
            .cs_call(move |src| async move { src.login_start(params).await })
            .await?;
        on_progress(OAuthProgress::LoginUrl {
            url: start.url.clone(),
            qr: None,
        });
        on_progress(OAuthProgress::CallbackListening {
            addr: proxy.bind_addr().to_string(),
        });
        on_progress(OAuthProgress::WaitingForCallback);
        let complete = LoginCompleteParams {
            session_id: start.session_id,
        };
        let result = self
            .cs_call(move |src| async move { src.login_complete(complete).await })
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
        let params = ScanParams {
            plugin_data_dir: self.plugin_data_dir.display().to_string(),
            accounts: opts.accounts,
            page_size: opts.page_size,
            import_episodes: opts.import_episodes,
            import_plus_titles: opts.import_plus_titles,
            credentials,
        };
        let summary = self
            .cs_call(move |src| async move { src.scan(params).await })
            .await?;
        let mut upserted = 0usize;
        for book in &summary.books {
            scope
                .upsert_book(&scan_book_to_new(self.id(), book.clone()))
                .await
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
            upserted += 1;
        }
        Ok(scan_summary_from_abi(&summary, upserted))
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
        let credentials = credentials.as_ref().map(credentials_to_bytes).transpose()?;
        // Jail-granted scratch (already TMPDIR), not the host download cache.
        let cache_dir = {
            let dir = self.session.scratch_dir().join("fetch");
            tokio::fs::create_dir_all(&dir)
                .await
                .map_err(|e| bookclerk_source::SourceError::api(e.to_string()))?;
            dir
        };
        let params = FetchTitleParams {
            plugin_data_dir: self.plugin_data_dir.display().to_string(),
            account_id: account_id.to_string(),
            title_id: title_id.to_string(),
            cache_dir: cache_dir.display().to_string(),
            credentials,
            source_config: ExtensibleConfig::json(&self.source_config),
            fetch: (&opts.download).into(),
        };
        let plain = self
            .cs_call(move |src| async move { src.fetch_title(params).await })
            .await?;
        Ok(plain.into())
    }

    async fn search_catalog(
        &self,
        opts: &CatalogSearchOpts,
    ) -> bookclerk_source::Result<Vec<CatalogHit>> {
        if !self.session.has_capability("searchCatalog") {
            return Ok(Vec::new());
        }
        let params = SearchCatalogParams::from(opts);
        match self
            .cs_call(move |src| async move { src.search_catalog(params).await })
            .await
        {
            Ok(hits) => Ok(hits.into_iter().map(catalog_hit_from_abi).collect()),
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
            .cs_call(move |src| async move { src.catalog_detail(params).await })
            .await
        {
            Ok(hit) => Ok(hit.map(catalog_hit_from_abi)),
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
        let params = expand_candidates_params(seed, limit);
        match self
            .cs_call(move |src| async move { src.expand_candidates(params).await })
            .await
        {
            Ok(hits) => Ok(hits.into_iter().map(catalog_hit_from_abi).collect()),
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
        let params = bookclerk_plugin_sdk::PurchaseHintParams::from(opts);
        match self
            .cs_call(move |src| async move { src.purchase_hint(params).await })
            .await
        {
            Ok(hint) => Ok(hint.map(purchase_hint_from_abi)),
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
        let params = ListDealsParams {
            limit: Some(u32::try_from(limit).unwrap_or(DEFAULT_LIST_DEALS_LIMIT)),
        };
        match self
            .cs_call(move |src| async move { src.list_deals(params).await })
            .await
        {
            Ok(hits) => Ok(hits.into_iter().map(catalog_hit_from_abi).collect()),
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

/// Maps a guest catalog hit onto a host [`CatalogHit`], decoding HTML entities.
fn catalog_hit_from_abi(hit: bookclerk_plugin_sdk::CatalogHit) -> CatalogHit {
    CatalogHit::from(hit).decode_html_entities()
}

/// Maps a guest purchase hint onto a host hint, decoding HTML entities.
fn purchase_hint_from_abi(hint: bookclerk_plugin_sdk::PurchaseHint) -> SourcePurchaseHint {
    SourcePurchaseHint::from(hint).decode_html_entities()
}

/// Load host-sealed credentials for the accounts a scan will cover.
///
/// Empty `accounts` filter → all scoped accounts that have credentials.
/// Explicit account needles match `account_id` or label (case-insensitive).
async fn scan_credentials_for(
    scope: &SourceScope,
    account_filter: &[String],
) -> bookclerk_source::Result<Vec<bookclerk_plugin_sdk::AccountCredential>> {
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
                out.insert(acct.account_id, credentials_to_bytes(&creds)?);
            }
            Ok(None) => {}
            Err(e) => {
                return Err(bookclerk_source::SourceError::Auth(e.to_string()));
            }
        }
    }
    Ok(account_credentials(out))
}

/// Upserts the account and seals guest credentials via [`SourceScope`] (plugin cannot write the DB).
async fn seal_login_result(
    scope: &SourceScope,
    plugin_id: &str,
    result: LoginResult,
) -> bookclerk_source::Result<SourceAccount> {
    let mut account = SourceAccount::from(result.account);
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
        let creds = credentials_from_bytes(&creds)?;
        scope
            .save_credentials_json(&account.account_id, &creds)
            .await
            .map_err(|e| bookclerk_source::SourceError::Auth(e.to_string()))?;
    }
    Ok(account)
}

/// Leaks `describe()` strings into `'static` slices for [`SourceBrand`] / aliases.
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

/// Builds a [`SourceBrand`] from `describe()`, or a slate fallback using plugin id/name.
fn brand_from_abi(
    brand: Option<&bookclerk_plugin_sdk::Brand>,
    id: &str,
    name: &str,
) -> SourceBrand {
    if let Some(b) = brand {
        SourceBrand {
            id: Box::leak(b.id.clone().into_boxed_str()),
            name: Box::leak(b.name.clone().into_boxed_str()),
            bg: Box::leak(b.bg.clone().into_boxed_str()),
            fg: Box::leak(b.fg.clone().into_boxed_str()),
            accent: Box::leak(b.accent.clone().into_boxed_str()),
            icon_url: Box::leak(b.icon_url.clone().unwrap_or_default().into_boxed_str()),
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

/// Converts a TOML value tree into JSON for spawn `config` delivery.
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
    use std::sync::OnceLock;
    use tempfile::TempDir;

    static MASTER_KEY_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn shared_test_master_key() {
        static SHARED: OnceLock<TempDir> = OnceLock::new();
        let dir = SHARED.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            configure_master_key(dir.path()).unwrap();
            dir
        });
        configure_master_key(dir.path()).unwrap();
    }

    #[tokio::test]
    async fn scan_credentials_only_from_this_scope() {
        let _guard = MASTER_KEY_TEST_LOCK.lock().await;
        shared_test_master_key();
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
        assert_eq!(creds[0].account_id, "a1");
        let json = credentials_from_bytes(&creds[0].credentials).unwrap();
        assert_eq!(json["token"], "echo-secret");
    }

    #[tokio::test]
    async fn scan_credentials_skips_scan_disabled_unless_explicit() {
        let _guard = MASTER_KEY_TEST_LOCK.lock().await;
        shared_test_master_key();
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
}
