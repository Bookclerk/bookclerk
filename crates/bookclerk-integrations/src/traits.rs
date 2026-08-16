//! Pluggable outbound integration trait.
//!
//! Required methods cover identity, lifecycle, events, and health. Optional
//! capabilities (credential login, library scan, listening sync) default off —
//! override only when the adapter can provide them. Ranking / discovery never
//! call adapter clients; they read the generic `listening_progress` table after
//! hosts sync via [`Integration::sync_listening_progress`].

use async_trait::async_trait;
use bookclerk_library::LibraryStore;

use crate::brand::Brand;
use crate::error::{IntegrationError, Result};
use crate::types::{ExternalUser, IntegrationEvent, IntegrationHealth};

/// Context passed when starting background watchers.
#[derive(Clone, Default)]
pub struct IntegrationContext {
    /// Optional callback when an external user is observed (claim minting).
    pub on_external_user: Option<std::sync::Arc<dyn Fn(ExternalUser) + Send + Sync>>,
}

/// Downstream consumer of acquired books / identity signals (e.g. Audiobookshelf).
#[async_trait]
pub trait Integration: Send + Sync {
    /// Stable integration id (`audiobookshelf`, …).
    fn id(&self) -> &str;

    /// Human-facing name for portal / CLI (defaults to [`Self::id`]).
    fn display_name(&self) -> &str {
        self.id()
    }

    /// Start background tasks (user watchers, etc.).
    async fn start(&self, ctx: IntegrationContext) -> Result<()>;

    /// Stop background tasks started by [`Self::start`].
    ///
    /// Default: no-op. Implementations that spawn poll loops should cancel them
    /// idempotently so config reload can replace the registry safely.
    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    /// Handle a fan-out event (best-effort; errors are logged by the registry).
    async fn on_event(&self, event: &IntegrationEvent) -> Result<()>;

    /// Probes connectivity and configuration for this integration.
    ///
    /// # Returns
    ///
    /// A health snapshot suitable for CLI / SPA status views.
    ///
    /// # Errors
    ///
    /// Returns an error when the probe itself fails unexpectedly (transports);
    /// soft failures should usually set [`IntegrationHealth::ok`] to `false`.
    async fn health(&self) -> Result<IntegrationHealth>;

    /// Whether this integration may appear as a portal username/password login.
    ///
    /// Default: `false`. Integrations that implement credential login override
    /// this (typically gated by a config flag such as `allow_credential_login`).
    fn supports_credential_login(&self) -> bool {
        false
    }

    /// Validate end-user credentials (portal return visits / self-service).
    ///
    /// Default: returns an error. Only called when
    /// [`Self::supports_credential_login`] is `true`.
    async fn authenticate_user(&self, _username: &str, _password: &str) -> Result<ExternalUser> {
        Err(IntegrationError::message(format!(
            "integration `{}` does not support credential login",
            self.id()
        )))
    }

    /// Whether this integration can trigger a remote library scan.
    fn supports_library_scan(&self) -> bool {
        false
    }

    /// Trigger a remote library scan (optional force rescan).
    ///
    /// Default: unsupported. Hosts (CLI / daemon) call this via the registry
    /// instead of talking to a specific adapter client.
    async fn scan_library(&self, _force: bool) -> Result<()> {
        Err(IntegrationError::message(format!(
            "integration `{}` does not support library scan",
            self.id()
        )))
    }

    /// Whether this integration can sync listening / progress into the library DB.
    ///
    /// Default: `false`. Ranking treats listening as optional — when no
    /// integration syncs (or recommend filters exclude it), discovery still
    /// runs on owned-library taste alone.
    fn supports_listening_sync(&self) -> bool {
        false
    }

    /// Pull listening / progress rows into the generic `listening_progress` table.
    ///
    /// Implementations must tag rows with [`Self::id`] as `provider`. Hosts
    /// fan this out via [`crate::IntegrationRegistry::sync_listening_progress_all`]
    /// so one or more integrations can contribute.
    ///
    /// Default: unsupported.
    async fn sync_listening_progress(&self, _library: &LibraryStore) -> Result<usize> {
        Err(IntegrationError::message(format!(
            "integration `{}` does not support listening sync",
            self.id()
        )))
    }

    /// Human-readable connectivity probe for CLI / ops (`integrations test`).
    ///
    /// Default: summarizes [`Self::health`].
    async fn diagnose(&self) -> Result<Vec<String>> {
        let h = self.health().await?;
        let mut lines = vec![format!("{} enabled={} ok={}", h.id, h.enabled, h.ok)];
        if let Some(detail) = h.detail {
            lines.push(detail);
        }
        Ok(lines)
    }

    /// Optional portal brand for credential-login / connection UI.
    ///
    /// Owned by the integration plugin; hosts must not hardcode provider colors.
    fn portal_brand(&self) -> Option<Brand> {
        None
    }

    /// Plugin-provided Bookclerk-as-IdP client templates (`oidcClients` RPC).
    ///
    /// Empty when the guest is not a relying party or the method is unsupported.
    async fn provided_oidc_clients(&self) -> Result<Vec<ProvidedOidcClient>> {
        Ok(Vec::new())
    }
}

/// Host-side copy of a guest `oidcClients` template (no ABI crate dependency).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvidedOidcClient {
    /// OAuth `client_id`.
    pub client_id: String,
    /// Operator-facing card title.
    pub display_name: String,
    /// Path appended to the plugin origin.
    pub callback_path: String,
    /// Public PKCE when true.
    pub public_client: bool,
    /// Scopes for first materialization.
    pub default_scopes: Vec<String>,
    /// Whether new rows may issue refresh tokens.
    pub issue_refresh_token: bool,
    /// Dotted config key for the player origin.
    pub origin_config_key: String,
}
