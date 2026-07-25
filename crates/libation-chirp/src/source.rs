//! [`ChirpSource`]: [`ContentSource`] implementation for Chirp.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use libation_config::Config;
use libation_library::LibraryStore;
use libation_source::{
    ContentSource, FetchOptions, LoginOptions, PortalAuthMode, ScanOptions, ScanSummary,
    SourceAccount, SourceBrand, SourceFetch, SourceRegistry,
};

use crate::auth::{
    auth_file_for_account, ensure_accounts_dir, find_auth_file, list_auth_files, load_auth,
    save_auth, ChirpAuthFile, AUTH_SUFFIX,
};
use crate::client::{ChirpClient, DEFAULT_GRAPHQL_URL};
use crate::download::fetch_title_materials;
use crate::error::{ChirpError, Result};
use crate::sync::{scan_library, ScanOptions as ChirpScanOptions};

/// Canonical plugin id.
pub const ID: &str = "chirp";

/// Env var for non-interactive password login.
pub const PASSWORD_ENV: &str = "LIBATION_CHIRP_PASSWORD";

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

    /// Login and persist `.chirp.auth`.
    pub async fn login_account(
        &self,
        files_dir: &Path,
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

        ensure_accounts_dir(files_dir)?;
        let path = auth_file_for_account(files_dir, opts.label.as_deref(), email);
        if path.is_file() && !opts.force {
            let existing = load_auth(&path)?;
            return Ok(source_account_from_auth(&existing));
        }

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
        save_auth(&path, &auth)?;

        tracing::info!(
            email = %auth.email,
            user_id = ?auth.user_id,
            path = %path.display(),
            "saved Chirp auth file"
        );

        Ok(source_account_from_auth(&auth))
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

    fn auth_credential_suffixes(&self) -> &'static [&'static str] {
        const SUFFIXES: &[&str] = &[AUTH_SUFFIX];
        SUFFIXES
    }

    fn password_env_var(&self) -> Option<&'static str> {
        Some(PASSWORD_ENV)
    }

    fn sort_key(&self) -> u32 {
        3
    }

    async fn login(
        &self,
        files_dir: &Path,
        opts: LoginOptions,
    ) -> libation_source::Result<SourceAccount> {
        self.login_account(files_dir, opts)
            .await
            .map_err(Into::into)
    }

    async fn list_accounts(&self, files_dir: &Path) -> libation_source::Result<Vec<SourceAccount>> {
        let mut out = Vec::new();
        for path in list_auth_files(files_dir)? {
            match load_auth(&path) {
                Ok(auth) => out.push(source_account_from_auth(&auth)),
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "skipping unreadable Chirp auth file"
                    );
                }
            }
        }
        Ok(out)
    }

    async fn scan(
        &self,
        files_dir: &Path,
        library: &LibraryStore,
        opts: ScanOptions,
    ) -> libation_source::Result<ScanSummary> {
        scan_library(
            files_dir,
            library,
            ChirpScanOptions::from(&opts),
            Some(self.graphql_url.as_str()),
        )
        .await
        .map_err(Into::into)
    }

    async fn fetch_title(
        &self,
        files_dir: &Path,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> libation_source::Result<SourceFetch> {
        let path = find_auth_file(files_dir, account_id)?;
        let auth = load_auth(&path)?;
        let client = ChirpClient::new(&self.graphql_url).with_token(&auth.access_token);
        let plain = fetch_title_materials(&client, title_id, &opts.cache_dir).await?;
        Ok(SourceFetch::Plain(plain))
    }
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
