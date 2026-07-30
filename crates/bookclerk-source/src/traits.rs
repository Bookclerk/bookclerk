//! ContentSource trait.

use async_trait::async_trait;
use bookclerk_library::SourceScope;

use crate::brand::SourceBrand;
use crate::error::Result;
use crate::types::{
    FetchOptions, LoginOptions, ScanOptions, ScanSummary, SourceAccount, SourceConfigOption,
    SourceFetch,
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

    /// Whether acquire may supply a preloaded Audible-style license voucher.
    fn supports_preloaded_license(&self) -> bool {
        false
    }

    /// Authenticate and persist credentials via [`SourceScope`].
    async fn login(&self, scope: &SourceScope, opts: LoginOptions) -> Result<SourceAccount>;

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
}
