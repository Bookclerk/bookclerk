//! Runs the library's one active ingress instance (see
//! `docs/adr/ingress-plugins.md`).
//!
//! The deployment reconciler spawns an `ingress` plugin like any other
//! deployed instance, but only while the daemon's [`IngressHost`] admits it:
//! it must be the library's active ingress, and an Owner must exist. The
//! guest is resident, gets `ORIGIN` when it is a relay, and is asked to
//! `start` once healthy; the origin it reports goes back to the daemon. A
//! background task checks `health` every [`INGRESS_HEALTH_INTERVAL`], and the
//! guest is asked to `stop` before it is retired or replaced.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_plugin_abi::{IngressHealth, IngressStarted, SocketMessageOk};

use crate::socket_broker::{SocketEvent, SocketSink};
use crate::PluginSession;

/// How often a running ingress is asked whether it is serving.
pub const INGRESS_HEALTH_INTERVAL: Duration = Duration::from_secs(60);

/// What the daemon supplies to run the library's ingress.
#[async_trait]
pub trait IngressHost: Send + Sync {
    /// Whether `plugin_instance_id` may serve the library's public address
    /// now.
    ///
    /// # Errors
    ///
    /// Returns why not, for the deployment observation: not the active
    /// ingress, no Owner yet, or the settings could not be read.
    async fn admit(&self, plugin_instance_id: &str) -> Result<(), String>;

    /// The library as relay plugins reach it (`ORIGIN`).
    fn origin(&self) -> Arc<dyn bookclerk_plugin_abi::Origin + Send + Sync>;

    /// `plugin_instance_id` started and reported `started`.
    ///
    /// # Errors
    ///
    /// Returns why the report was refused (an origin that is not `https://`
    /// and a DNS name); the instance is then stopped.
    async fn started(
        &self,
        plugin_instance_id: &str,
        started: IngressStarted,
    ) -> Result<(), String>;

    /// `plugin_instance_id` stopped serving: retired, replaced, or refused.
    async fn stopped(&self, plugin_instance_id: &str);
}

/// The calls an [`IngressGuest`] makes on its guest; [`PluginSession`] in
/// the host, a double in tests.
#[async_trait]
pub(crate) trait IngressSession: Send + Sync {
    /// `Ingress.start`.
    async fn start(&self) -> Result<IngressStarted, String>;
    /// `Ingress.health`.
    async fn health(&self) -> Result<IngressHealth, String>;
    /// `Ingress.stop`.
    async fn stop(&self) -> Result<(), String>;
    /// Whether the guest process still runs.
    fn running(&self) -> bool;
    /// Hands the guest a socket event from its broker.
    async fn deliver(&self, event: SocketEvent) -> Result<SocketMessageOk, String>;
}

#[async_trait]
impl IngressSession for PluginSession {
    async fn start(&self) -> Result<IngressStarted, String> {
        self.ingress(|ingress| async move { ingress.start().await })
            .await
            .map_err(|err| err.to_string())
    }

    async fn health(&self) -> Result<IngressHealth, String> {
        self.ingress(|ingress| async move { ingress.health().await })
            .await
            .map_err(|err| err.to_string())
    }

    async fn stop(&self) -> Result<(), String> {
        self.ingress(|ingress| async move { ingress.stop().await })
            .await
            .map_err(|err| err.to_string())
    }

    fn running(&self) -> bool {
        self.guest_running()
    }

    async fn deliver(&self, event: SocketEvent) -> Result<SocketMessageOk, String> {
        self.deliver_socket_event(event)
            .await
            .map_err(|err| err.to_string())
    }
}

/// One running ingress guest.
pub(crate) struct IngressGuest {
    /// The guest's session.
    session: Arc<dyn IngressSession>,
    /// The deployment instance it serves.
    plugin_instance_id: String,
    /// The periodic health check; aborted when the guest stops.
    health: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl IngressGuest {
    /// Wraps a spawned, opened session.
    pub(crate) fn new(session: Arc<dyn IngressSession>, plugin_instance_id: &str) -> Self {
        Self {
            session,
            plugin_instance_id: plugin_instance_id.to_string(),
            health: std::sync::Mutex::new(None),
        }
    }

    /// Asks the guest to bring the address up.
    ///
    /// # Errors
    ///
    /// Returns the guest's failure as text.
    pub(crate) async fn start(&self) -> Result<IngressStarted, String> {
        self.session.start().await
    }

    /// Asks the guest whether it is serving.
    ///
    /// # Errors
    ///
    /// Returns the guest's failure as text.
    pub(crate) async fn health(&self) -> Result<IngressHealth, String> {
        self.session.health().await
    }

    /// Asks the guest to take the address down, stops the health check and
    /// tells the daemon. A guest that fails to stop is logged; it is retired
    /// anyway.
    pub(crate) async fn stop(&self, host: Option<&Arc<dyn IngressHost>>) {
        if let Some(task) = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
        if self.session.running() {
            if let Err(err) = self.session.stop().await {
                tracing::warn!(
                    plugin_instance_id = %self.plugin_instance_id,
                    error = %err,
                    "ingress stop failed"
                );
            }
        }
        if let Some(host) = host {
            host.stopped(&self.plugin_instance_id).await;
        }
    }

    /// Starts the periodic health check.
    pub(crate) fn watch_health(self: &Arc<Self>, interval: Duration) {
        let weak = Arc::downgrade(self);
        let task = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(guest) = weak.upgrade() else {
                    return;
                };
                log_health(&guest.plugin_instance_id, guest.health().await);
            }
        });
        *self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(task);
    }
}

/// Logs one health answer: quiet while serving, a warning otherwise.
fn log_health(plugin_instance_id: &str, health: Result<IngressHealth, String>) {
    match health {
        Ok(IngressHealth { serving: true, .. }) => {
            tracing::debug!(plugin_instance_id, "ingress is serving");
        }
        Ok(IngressHealth { detail, .. }) => tracing::warn!(
            plugin_instance_id,
            detail = detail.as_deref().unwrap_or(""),
            "ingress is not serving"
        ),
        Err(err) => tracing::warn!(plugin_instance_id, error = %err, "ingress health failed"),
    }
}

#[async_trait]
impl SocketSink for IngressGuest {
    async fn deliver_socket_event(
        &self,
        event: SocketEvent,
    ) -> std::result::Result<SocketMessageOk, String> {
        self.session.deliver(event).await
    }
}

#[cfg(test)]
#[path = "ingress_tests.rs"]
mod tests;
