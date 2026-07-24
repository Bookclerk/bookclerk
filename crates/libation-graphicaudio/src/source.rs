//! [`GraphicAudioSource`]: [`ContentSource`] implementation for GraphicAudio.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use libation_config::GraphicAudioAccess;
use libation_library::LibraryStore;
use libation_source::{
    ContentSource, FetchOptions, LoginOptions, ScanOptions, ScanSummary, SourceAccount,
    SourceFetch, SourceKind,
};

use crate::auth::{
    auth_file_for_account, ensure_accounts_dir, find_auth_file, list_auth_files, load_auth,
    save_auth, GraphicAudioAuthFile,
};
use crate::client::{GraphicAudioClient, DEFAULT_BASE_URL};
use crate::download::{
    fetch_title_with_mode, password_from_env, product_title_for, TitleFetchRequest,
};
use crate::error::{GraphicAudioError, Result};
use crate::magento::{MagentoClient, DEFAULT_STORE_URL};
use crate::sync::{scan_library, ScanOptions as GaScanOptions};

/// GraphicAudio content source.
#[derive(Debug, Clone)]
pub struct GraphicAudioSource {
    /// Access App API origin (`…/access`).
    base_url: String,
    /// Magento storefront origin (ZIP + Browser Player).
    store_url: String,
    /// Configured access path (login + default fetch). Env may still override fetch.
    access: GraphicAudioAccess,
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

    /// Force a fetch path (bypasses config / env).
    #[must_use]
    pub fn with_fetch_mode(mut self, mode: GraphicAudioAccess) -> Self {
        self.fetch_mode = Some(mode);
        self
    }

    /// Magento password for ZIP / Browser Player (bypasses `LIBATION_GA_PASSWORD`).
    #[must_use]
    pub fn with_magento_password(mut self, password: impl Into<String>) -> Self {
        self.magento_password = Some(password.into());
        self
    }

    /// Arc-wrapped instance for [`libation_source::SourceRegistry`].
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }

    /// Login and persist `.ga.auth`.
    ///
    /// - `access=web|zip`: Magento customer login only (no Access App device slot).
    /// - `access=device`: Access App `activation/login` (registers a device).
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
            .ok_or_else(|| GraphicAudioError::auth("GraphicAudio login requires email"))?;
        let password = opts
            .password
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GraphicAudioError::auth("GraphicAudio login requires password"))?;

        ensure_accounts_dir(files_dir)?;
        let path = auth_file_for_account(files_dir, opts.label.as_deref(), email);
        if path.is_file() && !opts.force {
            let existing = load_auth(&path)?;
            return Ok(source_account_from_auth(&existing));
        }

        let marketplace = if opts.marketplace.trim().is_empty() {
            String::from("us")
        } else {
            opts.marketplace.trim().to_ascii_lowercase()
        };

        let (token, client_id) = match self.access {
            GraphicAudioAccess::Device => {
                let client_id = if path.is_file() {
                    load_auth(&path)
                        .map(|a| a.client_id)
                        .unwrap_or_else(|_| format!("libation-{}", uuid::Uuid::new_v4()))
                } else {
                    format!("libation-{}", uuid::Uuid::new_v4())
                };
                let mut client = GraphicAudioClient::new(&self.base_url);
                let token = client.login(email, password, &client_id).await?;
                (token, client_id)
            }
            GraphicAudioAccess::Web | GraphicAudioAccess::Zip => {
                // Validate Magento credentials without consuming an Access App device slot.
                let store = MagentoClient::new(&self.store_url)?;
                store.login(email, password).await?;
                let client_id = if path.is_file() {
                    load_auth(&path)
                        .map(|a| a.client_id)
                        .unwrap_or_else(|_| format!("libation-{}", uuid::Uuid::new_v4()))
                } else {
                    format!("libation-{}", uuid::Uuid::new_v4())
                };
                // Preserve an existing Access App token when re-saving after Magento
                // validation so prior device activations remain usable for scan/device.
                let token = if path.is_file() {
                    load_auth(&path).map(|a| a.token).unwrap_or_default()
                } else {
                    String::new()
                };
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
        save_auth(&path, &auth)?;

        tracing::info!(
            email = %auth.email,
            access = ?self.access,
            has_device_token = auth.has_device_token(),
            path = %path.display(),
            "saved GraphicAudio auth file"
        );

        Ok(source_account_from_auth(&auth))
    }
}

#[async_trait]
impl ContentSource for GraphicAudioSource {
    fn kind(&self) -> SourceKind {
        SourceKind::GraphicAudio
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
                        "skipping unreadable GraphicAudio auth file"
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
        let password = self.magento_password.clone().or_else(password_from_env);
        scan_library(
            files_dir,
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
        files_dir: &Path,
        account_id: &str,
        title_id: &str,
        opts: &FetchOptions,
    ) -> libation_source::Result<SourceFetch> {
        let path = find_auth_file(files_dir, account_id)?;
        let auth = load_auth(&path)?;
        let client = GraphicAudioClient::new(&self.base_url).with_token(&auth.token);
        let prefer_hi = opts
            .download
            .ingest_quality("graphicaudio")
            .prefers_graphicaudio_hi();

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
            },
        )
        .await?;
        Ok(SourceFetch::Plain(plain))
    }
}

fn source_account_from_auth(auth: &GraphicAudioAuthFile) -> SourceAccount {
    SourceAccount {
        account_id: auth.account_id().to_string(),
        source: SourceKind::GraphicAudio,
        marketplace: auth.marketplace.clone(),
        label: auth.label.clone().or_else(|| Some(auth.email.clone())),
        scan_enabled: true,
    }
}
