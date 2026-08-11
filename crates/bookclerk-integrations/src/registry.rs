//! Registry of outbound integrations.

use std::sync::Arc;

use tracing::{error, info, warn};

use crate::error::Result;
use crate::traits::{Integration, IntegrationContext};
use crate::types::{IntegrationEvent, IntegrationHealth};

/// Fan-out registry for configured integrations.
#[derive(Clone, Default)]
pub struct IntegrationRegistry {
    integrations: Vec<Arc<dyn Integration>>,
}

impl IntegrationRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, integration: Arc<dyn Integration>) {
        info!(id = integration.id(), "registered integration");
        self.integrations.push(integration);
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn Integration>> {
        self.integrations.iter().find(|i| i.id() == id).cloned()
    }

    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Integration>] {
        &self.integrations
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.integrations.is_empty()
    }

    /// Start all integrations (background watchers).
    pub async fn start_all(&self, ctx: IntegrationContext) -> Result<()> {
        for integration in &self.integrations {
            if let Err(err) = integration.start(ctx.clone()).await {
                error!(id = integration.id(), %err, "integration start failed");
            }
        }
        Ok(())
    }

    /// Stop all integrations (background watchers). Errors are logged, not fatal.
    pub async fn stop_all(&self) {
        for integration in &self.integrations {
            if let Err(err) = integration.stop().await {
                error!(id = integration.id(), %err, "integration stop failed");
            }
        }
    }

    /// Fan-out an event; individual failures are logged, not fatal.
    pub async fn emit(&self, event: &IntegrationEvent) {
        for integration in &self.integrations {
            if let Err(err) = integration.on_event(event).await {
                warn!(id = integration.id(), %err, "integration event handler failed");
            }
        }
    }

    /// Collect health for all integrations.
    pub async fn health_all(&self) -> Vec<IntegrationHealth> {
        let mut out = Vec::with_capacity(self.integrations.len());
        for integration in &self.integrations {
            match integration.health().await {
                Ok(h) => out.push(h),
                Err(err) => out.push(IntegrationHealth {
                    id: integration.id().to_string(),
                    enabled: true,
                    ok: false,
                    detail: Some(err.to_string()),
                }),
            }
        }
        out
    }

    /// Integrations that currently offer portal username/password login.
    #[must_use]
    pub fn credential_login_providers(&self) -> Vec<Arc<dyn Integration>> {
        self.integrations
            .iter()
            .filter(|i| i.supports_credential_login())
            .cloned()
            .collect()
    }

    /// Integrations that can sync listening / progress.
    #[must_use]
    pub fn listening_sync_providers(&self) -> Vec<Arc<dyn Integration>> {
        self.integrations
            .iter()
            .filter(|i| i.supports_listening_sync())
            .cloned()
            .collect()
    }

    /// Sync listening progress from every capable integration into the library DB.
    ///
    /// Individual failures are recorded in the summary and do not abort siblings.
    pub async fn sync_listening_progress_all(
        &self,
        library: &bookclerk_library::LibraryStore,
    ) -> crate::types::SyncListeningSummary {
        use crate::types::{SyncListeningProviderResult, SyncListeningSummary};

        let mut summary = SyncListeningSummary::default();
        let providers = self.listening_sync_providers();
        if providers.is_empty() {
            return summary;
        }
        for integration in providers {
            match integration.sync_listening_progress(library).await {
                Ok(n) => {
                    info!(
                        id = integration.id(),
                        upserted = n,
                        "listening sync complete"
                    );
                    summary.upserted += n;
                    summary.by_provider.push(SyncListeningProviderResult {
                        id: integration.id().to_string(),
                        upserted: n,
                        error: None,
                    });
                }
                Err(err) => {
                    warn!(id = integration.id(), %err, "listening sync failed");
                    summary.by_provider.push(SyncListeningProviderResult {
                        id: integration.id().to_string(),
                        upserted: 0,
                        error: Some(err.to_string()),
                    });
                }
            }
        }
        summary
    }
}
