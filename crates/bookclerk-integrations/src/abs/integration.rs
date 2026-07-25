//! Audiobookshelf integration adapter.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_config::AudiobookshelfConfig;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use super::brand::BRAND;
use super::client::AbsApiClient;
use crate::brand::Brand;
use crate::error::{IntegrationError, Result};
use crate::traits::{Integration, IntegrationContext};
use crate::types::{ExternalUser, IntegrationEvent, IntegrationHealth};

const PROVIDER: &str = "audiobookshelf";

/// Audiobookshelf outbound integration.
///
/// Always constructed when `enabled = true`. If required config (API key / base
/// URL) is missing, the adapter stays registered, reports unhealthy, and
/// refuses operational calls so the misconfiguration is visible.
pub struct AbsIntegration {
    config: AudiobookshelfConfig,
    client: Option<AbsApiClient>,
    config_error: Option<String>,
    /// Debounce overlapping acquire→scan bursts.
    scan_lock: Mutex<()>,
    known_users: Arc<Mutex<HashSet<String>>>,
}

impl AbsIntegration {
    /// Build an ABS integration. Prefer [`Self::from_config`] — this only fails
    /// on client construction bugs, not missing API keys.
    pub fn new(config: AudiobookshelfConfig) -> Result<Self> {
        Ok(Self::from_config(config))
    }

    /// Construct from config; missing credentials become a soft config error.
    #[must_use]
    pub fn from_config(config: AudiobookshelfConfig) -> Self {
        let api_key = config.api_key.clone().filter(|s| !s.is_empty());
        let (client, config_error) = match api_key {
            None => (
                None,
                Some(
                    "integrations.audiobookshelf.api_key (or BOOKCLERK_ABS_API_KEY) is required"
                        .to_string(),
                ),
            ),
            Some(api_key) => match AbsApiClient::new(config.base_url.clone(), api_key) {
                Ok(client) => (Some(client), None),
                Err(err) => (None, Some(err.to_string())),
            },
        };
        if let Some(err) = &config_error {
            error!(%err, "audiobookshelf integration enabled but misconfigured");
        }
        Self {
            config,
            client,
            config_error,
            scan_lock: Mutex::new(()),
            known_users: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    fn require_client(&self) -> Result<&AbsApiClient> {
        self.client.as_ref().ok_or_else(|| {
            IntegrationError::message(
                self.config_error
                    .clone()
                    .unwrap_or_else(|| "audiobookshelf integration is misconfigured".into()),
            )
        })
    }

    #[must_use]
    pub fn client(&self) -> Option<&AbsApiClient> {
        self.client.as_ref()
    }

    #[must_use]
    pub fn config(&self) -> &AudiobookshelfConfig {
        &self.config
    }

    #[must_use]
    pub fn config_error(&self) -> Option<&str> {
        self.config_error.as_deref()
    }

    async fn trigger_scan(&self) -> Result<()> {
        let client = self.require_client()?;
        let Some(library_id) = self.config.library_id.as_deref().filter(|s| !s.is_empty()) else {
            warn!("ABS scan skipped: integrations.audiobookshelf.library_id unset");
            return Ok(());
        };
        let _guard = self.scan_lock.lock().await;
        info!(%library_id, "triggering Audiobookshelf library scan");
        client.scan_library(library_id, false).await
    }

    /// Trigger a library scan (optional force).
    pub async fn scan_now(&self, force: bool) -> Result<()> {
        let client = self.require_client()?;
        let Some(library_id) = self.config.library_id.as_deref().filter(|s| !s.is_empty()) else {
            return Err(IntegrationError::message(
                "integrations.audiobookshelf.library_id unset",
            ));
        };
        let _guard = self.scan_lock.lock().await;
        client.scan_library(library_id, force).await
    }
}

#[async_trait]
impl Integration for AbsIntegration {
    fn id(&self) -> &str {
        PROVIDER
    }

    fn display_name(&self) -> &str {
        "Audiobookshelf"
    }

    async fn start(&self, ctx: IntegrationContext) -> Result<()> {
        let client = match self.require_client() {
            Ok(c) => c.clone(),
            Err(err) => {
                error!(%err, "ABS start refused");
                return Err(err);
            }
        };
        if !self.config.watch_users {
            debug!("ABS user watch disabled");
            return Ok(());
        }
        let known = self.known_users.clone();
        let on_user = ctx.on_external_user.clone();
        // Seed + poll loop (Socket.io client deferred; poll is reliable without extra deps).
        let this_client = client.clone();
        let known_seed = known.clone();
        tokio::spawn(async move {
            match this_client.list_users().await {
                Ok(users) => {
                    let mut g = known_seed.lock().await;
                    for u in users {
                        g.insert(u.id);
                    }
                    info!(count = g.len(), "seeded ABS user watch set");
                }
                Err(err) => warn!(%err, "failed to seed ABS users"),
            }
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                match client.list_users().await {
                    Ok(users) => {
                        let mut g = known.lock().await;
                        for user in users {
                            if g.insert(user.id.clone()) {
                                info!(
                                    user_id = %user.id,
                                    username = %user.username,
                                    "ABS user observed"
                                );
                                let external = ExternalUser {
                                    provider: PROVIDER.into(),
                                    external_user_id: user.id,
                                    display_name: Some(user.username),
                                    access_token: None,
                                };
                                if let Some(cb) = &on_user {
                                    cb(external);
                                }
                            }
                        }
                    }
                    Err(err) => warn!(%err, "ABS user poll failed"),
                }
            }
        });
        Ok(())
    }

    async fn on_event(&self, event: &IntegrationEvent) -> Result<()> {
        self.require_client()?;
        match event {
            IntegrationEvent::BookAcquired { .. } => {
                if self.config.notify_scan_on_acquire {
                    self.trigger_scan().await?;
                }
                Ok(())
            }
            IntegrationEvent::ExternalUserObserved { .. } => Ok(()),
        }
    }

    async fn health(&self) -> Result<IntegrationHealth> {
        let Some(client) = self.client.as_ref() else {
            return Ok(IntegrationHealth {
                id: PROVIDER.into(),
                enabled: self.config.enabled,
                ok: false,
                detail: self
                    .config_error
                    .clone()
                    .or_else(|| Some("audiobookshelf integration is misconfigured".into())),
            });
        };
        match client.authorize().await {
            Ok(auth) => Ok(IntegrationHealth {
                id: PROVIDER.into(),
                enabled: self.config.enabled,
                ok: true,
                detail: auth.user.map(|u| format!("authorized as {}", u.username)),
            }),
            Err(err) => Ok(IntegrationHealth {
                id: PROVIDER.into(),
                enabled: self.config.enabled,
                ok: false,
                detail: Some(err.to_string()),
            }),
        }
    }

    async fn authenticate_user(&self, username: &str, password: &str) -> Result<ExternalUser> {
        if !self.supports_credential_login() {
            return Err(IntegrationError::message(
                "integrations.audiobookshelf.allow_credential_login is false",
            ));
        }
        self.require_client()?
            .authenticate_user(username, password)
            .await
    }

    fn supports_credential_login(&self) -> bool {
        self.config.allow_credential_login && self.client.is_some()
    }

    fn supports_library_scan(&self) -> bool {
        self.client.is_some()
            && self
                .config
                .library_id
                .as_deref()
                .is_some_and(|s| !s.is_empty())
    }

    async fn scan_library(&self, force: bool) -> Result<()> {
        self.scan_now(force).await
    }

    async fn diagnose(&self) -> Result<Vec<String>> {
        let client = self.require_client()?;
        let auth = client.authorize().await?;
        let mut lines = Vec::new();
        if let Some(user) = auth.user {
            lines.push(format!("authorized as {} ({})", user.username, user.id));
        } else {
            lines.push("authorized (no user in response)".into());
        }
        for lib in client.list_libraries().await? {
            lines.push(format!("library {} — {}", lib.id, lib.name));
        }
        Ok(lines)
    }

    fn portal_brand(&self) -> Option<Brand> {
        Some(BRAND)
    }
}
