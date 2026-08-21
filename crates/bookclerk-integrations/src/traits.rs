//! Pluggable outbound integration trait.
//!
//! Required methods cover identity, lifecycle, events, and health. Optional
//! capabilities (credential login, library scan, listening sync) default off —
//! override only when the adapter can provide them. Ranking / discovery never
//! call adapter clients; they read the generic `listening_progress` table after
//! hosts sync via [`Integration::sync_listening_progress`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_library::LibraryStore;
use bookclerk_plugin_abi::v2::{DomainEvent, EventResult};

use crate::brand::Brand;
use crate::error::{IntegrationError, Result};
use crate::types::{ExternalUser, IntegrationEvent, IntegrationHealth};

/// Declared durable `onEvent` subscription (mirrors `plugin.toml`).
#[derive(Debug, Clone, PartialEq)]
pub struct EventSubscription {
    /// Event type (`book_acquired`).
    pub event_type: String,
    /// Schema versions this guest can consume.
    pub schema_versions: Vec<u32>,
    /// Whether `EventResult::Suspended` is advertised for this type.
    pub supports_suspend: bool,
    /// Concurrency class copied onto deliveries (default `"network"`).
    pub resource_class: String,
    /// Optional host-owned payload object filter (top-level key equality).
    pub filter: Option<serde_json::Value>,
}

impl EventSubscription {
    /// Type + schema subscription with the default `network` resource class.
    #[must_use]
    pub fn new(event_type: impl Into<String>, schema_versions: Vec<u32>) -> Self {
        Self {
            event_type: event_type.into(),
            schema_versions,
            supports_suspend: false,
            resource_class: "network".into(),
            filter: None,
        }
    }
}

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

    /// Deliver a versioned [`DomainEvent`] and return a durable [`EventResult`].
    ///
    /// Default maps [`Self::on_event`] success to `Ack` and errors to `Retry`.
    async fn deliver_domain_event(&self, event: DomainEvent) -> Result<EventResult> {
        match serde_json::from_slice::<serde_json::Value>(&event.payload) {
            Ok(payload) => {
                let mapped = integration_event_from_payload(&event.event_type, &payload);
                match mapped {
                    Some(host_event) => match self.on_event(&host_event).await {
                        Ok(()) => Ok(EventResult::Ack),
                        Err(err) => Ok(EventResult::Retry {
                            retry_at_unix_ms: 0,
                            reason: err.to_string(),
                        }),
                    },
                    None => Ok(EventResult::Reject {
                        reason: format!("unsupported event type `{}`", event.event_type),
                    }),
                }
            }
            Err(err) => Ok(EventResult::Reject {
                reason: format!("invalid event payload: {err}"),
            }),
        }
    }

    /// Like [`Self::deliver_domain_event`], aborting when `cancel` is set (fence loss).
    ///
    /// # Errors
    ///
    /// Returns [`IntegrationError::Message`] `"fence lost"` when `cancel` is set,
    /// or whatever [`Self::deliver_domain_event`] returns.
    async fn deliver_domain_event_cancelable(
        &self,
        event: DomainEvent,
        cancel: Arc<AtomicBool>,
    ) -> Result<EventResult> {
        tokio::select! {
            biased;
            () = wait_cancel_flag(Arc::clone(&cancel)) => {
                Err(IntegrationError::message("fence lost"))
            }
            result = self.deliver_domain_event(event) => result,
        }
    }

    /// Durable outbox subscriptions. Empty means this integration is not a subscriber.
    fn event_subscriptions(&self) -> Vec<EventSubscription> {
        Vec::new()
    }

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

/// Map a versioned outbox payload onto the legacy [`IntegrationEvent`] enum.
fn integration_event_from_payload(
    event_type: &str,
    payload: &serde_json::Value,
) -> Option<IntegrationEvent> {
    let inner = payload.get("payload").unwrap_or(payload);
    match event_type {
        "book_acquired" => {
            let title_id = inner
                .get("titleId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let storage_key = inner
                .get("pathKeys")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let now = chrono::Utc::now();
            Some(IntegrationEvent::BookAcquired {
                book: Box::new(bookclerk_library::BookRecord {
                    id: 0,
                    uuid: title_id.clone(),
                    source: inner
                        .get("source")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                    account_id: inner
                        .get("accountId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    product_id: title_id,
                    asin: inner
                        .get("asin")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    isbn: inner
                        .get("isbn")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    marketplace: String::new(),
                    title: String::new(),
                    authors: None,
                    narrators: None,
                    series: None,
                    series_index: None,
                    series_asin: None,
                    acquire_status: bookclerk_library::AcquireStatus::Acquired,
                    storage_key: Some(storage_key.clone()),
                    error_message: None,
                    purchased_at: None,
                    tags: None,
                    rating_overall: None,
                    rating_performance: None,
                    rating_story: None,
                    is_finished: false,
                    pdf_status: bookclerk_library::AcquireStatus::NotAcquired,
                    pdf_storage_key: None,
                    publisher: None,
                    length_minutes: None,
                    is_abridged: false,
                    content_kind: "book".into(),
                    categories: None,
                    subtitle: None,
                    published_at: None,
                    description: None,
                    language: None,
                    cover_url: None,
                    subjects: None,
                    enrich_source: None,
                    enrich_confidence: None,
                    enrich_updated_at: None,
                    created_at: now,
                    updated_at: now,
                }),
                storage_key,
                absolute_path: None,
            })
        }
        _ => None,
    }
}

/// Poll until `flag` is set so `select!` can abort an in-flight delivery.
async fn wait_cancel_flag(flag: Arc<AtomicBool>) {
    while !flag.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::missing_docs_in_private_items)]
    use super::*;
    use crate::types::IntegrationHealth;
    use std::time::Instant;

    struct SlowAck;

    #[async_trait]
    impl Integration for SlowAck {
        fn id(&self) -> &str {
            "slow"
        }

        async fn start(&self, _ctx: IntegrationContext) -> Result<()> {
            Ok(())
        }

        async fn on_event(&self, _event: &IntegrationEvent) -> Result<()> {
            Ok(())
        }

        async fn health(&self) -> Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: "slow".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        async fn deliver_domain_event(&self, _event: DomainEvent) -> Result<EventResult> {
            tokio::time::sleep(Duration::from_secs(2)).await;
            Ok(EventResult::Ack)
        }
    }

    #[tokio::test]
    async fn cancelable_delivery_aborts_when_fence_is_lost() {
        let cancel = Arc::new(AtomicBool::new(false));
        let integ = SlowAck;
        let event = DomainEvent {
            event_type: "book_acquired".into(),
            delivery_attempt: 1,
            ..DomainEvent::default()
        };
        let handle = tokio::spawn({
            let cancel = Arc::clone(&cancel);
            async move { integ.deliver_domain_event_cancelable(event, cancel).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.store(true, Ordering::SeqCst);
        let started = Instant::now();
        let err = handle.await.expect("join").expect_err("fence lost");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "must abort the 2s handler, took {:?}",
            started.elapsed()
        );
        assert!(
            err.to_string().contains("fence lost"),
            "unexpected error: {err}"
        );
    }
}
