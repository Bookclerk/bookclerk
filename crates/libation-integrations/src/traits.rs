//! Pluggable outbound integration trait.
//!
//! Required methods cover identity, lifecycle, events, and health. Credential
//! login for the connect portal is **optional** — override
//! [`Integration::supports_credential_login`] / [`Integration::authenticate_user`]
//! only when the adapter can validate end-user credentials.

use async_trait::async_trait;

use crate::brand::Brand;
use crate::error::{IntegrationError, Result};
use crate::types::{ExternalUser, IntegrationEvent, IntegrationHealth};

/// Context passed when starting background watchers.
#[derive(Clone, Default)]
pub struct IntegrationContext {
    /// Optional callback when an external user is observed (claim minting).
    pub on_external_user: Option<std::sync::Arc<dyn Fn(ExternalUser) + Send + Sync>>,
}

/// Downstream consumer of liberated books / identity signals (e.g. Audiobookshelf).
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

    /// Handle a fan-out event (best-effort; errors are logged by the registry).
    async fn on_event(&self, event: &IntegrationEvent) -> Result<()>;

    /// Connectivity / config health check.
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
}
