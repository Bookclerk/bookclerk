//! [`LibroSource`]: [`ContentSource`] implementation for Libro.fm.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::LibraryStore;
use bookclerk_source::{
    ContentSource, FetchOptions, LoginOptions, PortalAuthMode, ScanOptions, ScanSummary,
    SourceAccount, SourceBrand, SourceFetch, SourceRegistry,
};
use chrono::{Duration, TimeZone, Utc};

use crate::auth::{
    auth_file_for_account, ensure_accounts_dir, find_auth_file, list_auth_files, load_auth,
    save_auth, LibroAuthFile, AUTH_SUFFIX,
};
use crate::client::{LibroClient, DEFAULT_BASE_URL};
use crate::container::LibroContainer;
use crate::download::fetch_title_materials_with;
use crate::error::{LibroError, Result};
use crate::sync::{scan_library, ScanOptions as LibroScanOptions};

/// Canonical plugin id.
pub const ID: &str = "libro";

/// Env var for non-interactive password login.
pub const PASSWORD_ENV: &str = "BOOKCLERK_LIBRO_PASSWORD";

const ALIASES: &[&str] = &["libro.fm", "librofm"];

/// Libro.fm content source.
#[derive(Debug, Clone)]
pub struct LibroSource {
    base_url: String,
    /// Preferred download container (`[sources.libro] container`).
    pub container: LibroContainer,
}

impl Default for LibroSource {
    fn default() -> Self {
        Self::new()
    }
}

impl LibroSource {
    /// Production Libro.fm API origin.
    #[must_use]
    pub fn new() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            container: LibroContainer::M4b,
        }
    }

    /// Parse `[sources.libro]` knobs from config.
    #[must_use]
    pub fn from_config(config: &Config) -> Self {
        let container = config
            .sources
            .get_string(ID, "container")
            .and_then(LibroContainer::parse)
            .unwrap_or_default();
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            container,
        }
    }

    /// Override API base (wiremock / staging).
    #[must_use]
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            container: LibroContainer::M4b,
        }
    }

    #[must_use]
    pub fn with_container(mut self, container: LibroContainer) -> Self {
        self.container = container;
        self
    }

    /// Arc-wrapped instance for [`bookclerk_source::SourceRegistry`].
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Login and persist `.libro.auth` (crate-level helper).
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
            .ok_or_else(|| LibroError::auth("Libro.fm login requires email"))?;
        let password = opts
            .password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LibroError::auth("Libro.fm login requires password"))?;

        ensure_accounts_dir(files_dir)?;
        let path = auth_file_for_account(files_dir, opts.label.as_deref(), email);
        if path.is_file() && !opts.force {
            let existing = load_auth(&path)?;
            return Ok(source_account_from_auth(&existing));
        }

        let mut client = LibroClient::new(&self.base_url);
        let token = client.login(email, password).await?;

        let expires_at = match (token.created_at, token.expires_in) {
            (Some(created), Some(expires_in)) => Utc
                .timestamp_opt(created, 0)
                .single()
                .map(|t| t + Duration::seconds(expires_in)),
            (None, Some(expires_in)) => Some(Utc::now() + Duration::seconds(expires_in)),
            _ => None,
        };

        let marketplace = if opts.marketplace.trim().is_empty() {
            String::from("us")
        } else {
            opts.marketplace.trim().to_ascii_lowercase()
        };

        let auth = LibroAuthFile {
            access_token: token.access_token,
            token_type: token.token_type,
            expires_at,
            email: email.to_string(),
            user_id: None,
            marketplace,
            label: opts.label.clone(),
        };
        save_auth(&path, &auth)?;

        tracing::info!(
            email = %auth.email,
            path = %path.display(),
            "saved Libro.fm auth file"
        );

        Ok(source_account_from_auth(&auth))
    }
}

#[async_trait]
impl ContentSource for LibroSource {
    fn id(&self) -> &str {
        ID
    }

    fn display_name(&self) -> &str {
        "Libro.fm"
    }

    fn aliases(&self) -> &'static [&'static str] {
        ALIASES
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Password
    }

    fn portal_brand(&self) -> SourceBrand {
        SourceBrand {
            id: "libro",
            name: "Libro.fm",
            bg: "#1F4E3D",
            fg: "#F4F1EA",
            accent: "#2F6B53",
            icon_url: "https://www.google.com/s2/favicons?domain=libro.fm&sz=128",
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
        1
    }

    async fn login(
        &self,
        files_dir: &Path,
        opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        self.login_account(files_dir, opts)
            .await
            .map_err(Into::into)
    }

    async fn list_accounts(
        &self,
        files_dir: &Path,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        let mut out = Vec::new();
        for path in list_auth_files(files_dir)? {
            match load_auth(&path) {
                Ok(auth) => out.push(source_account_from_auth(&auth)),
                Err(err) => {
                    tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "skipping unreadable Libro.fm auth file"
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
    ) -> bookclerk_source::Result<ScanSummary> {
        scan_library(
            files_dir,
            library,
            LibroScanOptions::from(&opts),
            Some(self.base_url.as_str()),
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
    ) -> bookclerk_source::Result<SourceFetch> {
        let path = find_auth_file(files_dir, account_id)?;
        let auth = load_auth(&path)?;
        let client = LibroClient::new(&self.base_url).with_token(&auth.access_token);
        let plain =
            fetch_title_materials_with(&client, title_id, &opts.cache_dir, self.container).await?;
        Ok(SourceFetch::Plain(plain))
    }

    fn config_options(&self) -> &'static [bookclerk_source::SourceConfigOption] {
        LIBRO_CONFIG_OPTIONS
    }
}

const LIBRO_CONFIG_OPTIONS: &[bookclerk_source::SourceConfigOption] =
    &[bookclerk_source::SourceConfigOption {
        key: "container",
        label: "Container",
        values: &[
            bookclerk_source::ConfigOptionValue {
                id: "m4b",
                label: "M4B",
            },
            bookclerk_source::ConfigOptionValue {
                id: "zip",
                label: "ZIP (MP3 parts)",
            },
        ],
    }];

fn source_account_from_auth(auth: &LibroAuthFile) -> SourceAccount {
    SourceAccount {
        account_id: auth.account_id().to_string(),
        source: ID.into(),
        marketplace: auth.marketplace.clone(),
        label: auth.label.clone().or_else(|| Some(auth.email.clone())),
        scan_enabled: true,
    }
}

/// Parse `[sources.libro]` into a [`LibroSource`].
#[must_use]
pub fn from_config(config: &Config) -> LibroSource {
    LibroSource::from_config(config)
}

/// Register Libro.fm when `[sources.libro] enabled` (default true).
pub fn register(registry: &mut SourceRegistry, config: &Config) {
    if config.sources.is_enabled(ID) {
        registry.register(Arc::new(from_config(config)));
    }
}
