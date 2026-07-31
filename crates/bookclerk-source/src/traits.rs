//! ContentSource trait.

use std::path::Path;

use async_trait::async_trait;
use bookclerk_library::{secret_kind, SourceScope};

use crate::brand::SourceBrand;
use crate::error::{Result, SourceError};
use crate::types::{
    CatalogHit, CatalogSearchOpts, ExpandSeed, FetchOptions, ImportCredentialsOptions,
    LoginOptions, OAuthProgress, PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount,
    SourceConfigOption, SourceFetch, SourcePurchaseHint,
};

/// How the connect portal authenticates this source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalAuthMode {
    /// Browser / QR OAuth (Audible).
    Oauth,
    /// Username + password form.
    Password,
}

impl PortalAuthMode {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oauth => "oauth",
            Self::Password => "password",
        }
    }
}

/// Pluggable audiobook store.
///
/// Plugins own their id, brand, auth mode, and config parsing. Hosts register
/// concrete crates at startup and talk only through this trait.
///
/// All credential and library mutations go through [`SourceScope`], which
/// forces `source` / `provider` to this plugin's id. First-party in-repo
/// adapters and third-party JSON-RPC plugins share the same scope rules.
#[async_trait]
pub trait ContentSource: Send + Sync {
    /// Stable plugin id (`audible`, `libro`, …).
    fn id(&self) -> &str;

    /// Human-facing store name for UI / logs.
    fn display_name(&self) -> &str {
        self.id()
    }

    /// Alternate ids accepted by CLI / config (`libro.fm`, `ga`, …).
    fn aliases(&self) -> &'static [&'static str] {
        &[]
    }

    /// Connect-portal auth mode.
    fn portal_auth_mode(&self) -> PortalAuthMode;

    /// Portal button brand (colors + favicon).
    fn portal_brand(&self) -> SourceBrand;

    /// Optional env var for non-interactive password login.
    fn password_env_var(&self) -> Option<&'static str> {
        None
    }

    /// Stable sort key for registry listing (lower first).
    fn sort_key(&self) -> u32 {
        100
    }

    /// Authenticate and persist credentials via [`SourceScope`].
    async fn login(&self, scope: &SourceScope, opts: LoginOptions) -> Result<SourceAccount>;

    /// Interactive OAuth login with progress (URL / waiting).
    ///
    /// Default: ignores progress and calls [`Self::login`]. OAuth sources
    /// (Audible, external guests with `login.start`) override this.
    async fn login_with_oauth_progress(
        &self,
        scope: &SourceScope,
        opts: LoginOptions,
        on_progress: &(dyn Fn(OAuthProgress) + Send + Sync),
    ) -> Result<SourceAccount> {
        let _ = on_progress;
        self.login(scope, opts).await
    }

    /// Import credentials from a file (auth JSON, Libation export, …).
    ///
    /// Default: unsupported. Audible implements auth-file / Libation / mkb79.
    async fn import_credentials(
        &self,
        scope: &SourceScope,
        path: &Path,
        opts: ImportCredentialsOptions,
    ) -> Result<Vec<SourceAccount>> {
        let _ = (scope, path, opts);
        Err(SourceError::api(format!(
            "credential import is not supported for source `{}`",
            self.id()
        )))
    }

    /// Delete stored credentials for `account_id` (books / account rows kept).
    ///
    /// Default: removes `{account}.plugin.auth`, any other `source_auth` secrets
    /// for the account, common legacy name patterns, and a Widevine `{id}.wvd`.
    async fn revoke_credentials(&self, scope: &SourceScope, account_id: &str) -> Result<()> {
        revoke_credentials_default(scope, account_id).await
    }

    /// List accounts for this plugin (scope filters by source id).
    async fn list_accounts(&self, scope: &SourceScope) -> Result<Vec<SourceAccount>>;

    /// Sync library rows using scoped credentials / upserts.
    async fn scan(&self, scope: &SourceScope, opts: ScanOptions) -> Result<ScanSummary>;

    /// Fetch everything needed to acquire one title (no storage writes).
    ///
    /// `title_id` is the source-native product id (Audible ASIN or Libro ISBN).
    /// Auth is loaded through `scope`; `opts.files_dir` carries the
    /// `BOOKCLERK_FILES_DIR` path for CDM / Widevine resolution.
    async fn fetch_title(
        &self,
        scope: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> Result<SourceFetch>;

    /// Source-native `[sources.<id>]` knobs for config / UI discovery.
    fn config_options(&self) -> &'static [SourceConfigOption] {
        &[]
    }

    /// Optional diagnostic / license inspect for one title (opaque JSON).
    ///
    /// Default: unsupported. Hosts must not special-case store APIs — sources
    /// that expose license dumps (e.g. Audible) override this.
    async fn inspect_title(
        &self,
        scope: &SourceScope,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> Result<serde_json::Value> {
        let _ = (scope, account_id, title_id, opts);
        Err(SourceError::api(format!(
            "title inspect is not supported for source `{}`",
            self.id()
        )))
    }

    /// Public catalog typeahead / search (no account required).
    ///
    /// Default: empty. Discover aggregates hits across registered sources.
    async fn search_catalog(&self, opts: &CatalogSearchOpts) -> Result<Vec<CatalogHit>> {
        let _ = opts;
        Ok(Vec::new())
    }

    /// Expand related / series / author candidates from a taste seed.
    ///
    /// Default: empty. `limit` caps returned hits (sources may also budget HTTP).
    async fn expand_candidates(&self, seed: &ExpandSeed, limit: usize) -> Result<Vec<CatalogHit>> {
        let _ = (seed, limit);
        Ok(Vec::new())
    }

    /// Resolve a purchase / catalog URL (optionally with live price).
    ///
    /// Default: none.
    async fn purchase_hint(&self, opts: &PurchaseHintOpts) -> Result<Option<SourcePurchaseHint>> {
        let _ = opts;
        Ok(None)
    }

    /// Current deals / promos for discovery shelves.
    ///
    /// Default: empty.
    async fn list_deals(&self, limit: usize) -> Result<Vec<CatalogHit>> {
        let _ = limit;
        Ok(Vec::new())
    }
}

/// Shared revoke path for plugins that seal as `.plugin.auth` (and legacy names).
pub async fn revoke_credentials_default(scope: &SourceScope, account_id: &str) -> Result<()> {
    let plugin_name = format!("{account_id}.plugin.auth");
    let _ = scope.delete_source_auth(account_id, &plugin_name).await;

    for suffix in [
        ".libro.auth",
        ".chirp.auth",
        ".graphicaudio.auth",
        ".plugin.auth",
    ] {
        let name = format!("{account_id}{suffix}");
        let _ = scope.delete_source_auth(account_id, &name).await;
    }

    if let Ok(secrets) = scope.list_source_auth().await {
        for secret in secrets {
            if secret.account_id.as_deref() == Some(account_id) {
                let _ = scope.delete_source_auth(account_id, &secret.name).await;
            }
        }
    }

    let wvd = format!("{account_id}.wvd");
    let _ = scope
        .delete_secret(secret_kind::WIDEVINE, account_id, &wvd)
        .await;

    Ok(())
}
