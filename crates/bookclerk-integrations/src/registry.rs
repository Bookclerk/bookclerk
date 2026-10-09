//! Registry of outbound integrations.

use std::collections::BTreeSet;
use std::sync::Arc;

use tracing::{error, info, warn};

use crate::error::Result;
use crate::traits::{Integration, IntegrationContext};
use crate::types::IntegrationHealth;

/// Hard ceiling on registered integrations (plugin discovery is user-influenced).
pub const MAX_REGISTERED_INTEGRATIONS: usize = 256;

/// Fan-out registry for configured integrations.
#[derive(Clone, Default)]
pub struct IntegrationRegistry {
    /// Registered adapters in registration order.
    integrations: Vec<Arc<dyn Integration>>,
}

impl IntegrationRegistry {
    /// Creates an empty registry with no integrations registered.
    ///
    /// # Returns
    ///
    /// Newly constructed `new` value.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an integration to this registry (later entries keep their order).
    ///
    /// Silently refuses registration once [`MAX_REGISTERED_INTEGRATIONS`] is
    /// reached so health fan-out cannot allocate from an unbounded discovery set.
    ///
    /// # Arguments
    ///
    /// * `integration` - Integration instance to register.
    pub fn register(&mut self, integration: Arc<dyn Integration>) {
        if self.integrations.len() >= MAX_REGISTERED_INTEGRATIONS {
            warn!(
                id = integration.id(),
                cap = MAX_REGISTERED_INTEGRATIONS,
                "refusing to register integration; registry at capacity"
            );
            return;
        }
        info!(id = integration.id(), "registered integration");
        self.integrations.push(integration);
    }

    /// Removes every registration of `plugin_instance_id`.
    ///
    /// Other instances of the same plugin key stay. The caller stops each
    /// returned integration before publishing a replacement.
    #[must_use]
    pub fn take_instance(&mut self, plugin_instance_id: &str) -> Vec<Arc<dyn Integration>> {
        let mut retired = Vec::new();
        if plugin_instance_id.is_empty() {
            return retired;
        }
        self.integrations.retain(|integration| {
            if integration.plugin_instance_id() == Some(plugin_instance_id) {
                retired.push(Arc::clone(integration));
                false
            } else {
                true
            }
        });
        retired
    }

    /// Returns the integration with this id, if registered.
    ///
    /// # Arguments
    ///
    /// * `id` - Stable id to look up.
    ///
    /// # Returns
    ///
    /// `Some(...)` when found / applicable; otherwise `None`.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<Arc<dyn Integration>> {
        let mut matches = self.matches(id);
        (matches.len() == 1).then(|| matches.remove(0))
    }

    /// Integrations whose instance id, PluginKey, or display alias match `id`.
    ///
    /// An instance id is one hit even when another instance shares the plugin
    /// key. A key or alias matches every occupant so two instances stay
    /// ambiguous to [`Self::get`].
    fn matches(&self, id: &str) -> Vec<Arc<dyn Integration>> {
        let needle = id.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        let by_instance: Vec<_> = self
            .integrations
            .iter()
            .filter(|integration| integration.plugin_instance_id() == Some(needle))
            .cloned()
            .collect();
        if !by_instance.is_empty() {
            return by_instance;
        }
        let by_key: Vec<_> = self
            .integrations
            .iter()
            .filter(|i| i.plugin_key() == needle)
            .cloned()
            .collect();
        if !by_key.is_empty() {
            return by_key;
        }
        let lower = needle.to_ascii_lowercase();
        self.integrations
            .iter()
            .filter(|i| i.id().eq_ignore_ascii_case(&lower))
            .cloned()
            .collect()
    }

    /// Returns every registered integration in registration order.
    #[must_use]
    pub fn all(&self) -> &[Arc<dyn Integration>] {
        &self.integrations
    }

    /// Returns true when no integrations are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.integrations.is_empty()
    }

    /// Start all integrations (background watchers).
    ///
    /// # Arguments
    ///
    /// * `ctx` - Lifecycle context (e.g. external-user callback).
    ///
    /// # Returns
    ///
    /// The successful result value for this operation.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying I/O, parse, network, or store operation fails.
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
        self.stop_except(&BTreeSet::new()).await;
    }

    /// Stops integrations that are not a present deployed instance.
    ///
    /// `keep` is plugin instance ids. Config reload reattaches those guests
    /// onto the replacement registry and leaves them running. A transitional
    /// integration is stopped even when its plugin key has a deployment.
    /// Errors are logged, not fatal.
    pub async fn stop_except(&self, keep: &BTreeSet<String>) {
        for integration in &self.integrations {
            if integration
                .plugin_instance_id()
                .is_some_and(|id| !id.is_empty() && keep.contains(id))
            {
                continue;
            }
            if let Err(err) = integration.stop().await {
                error!(id = integration.id(), %err, "integration stop failed");
            }
        }
    }

    /// Probes every registered integration and returns one health row each.
    pub async fn health_all(&self) -> Vec<IntegrationHealth> {
        // Capacity is the public constant (not `len()`), so allocation size is not
        // dataflow-tainted from plugin discovery.
        let mut out = Vec::with_capacity(MAX_REGISTERED_INTEGRATIONS);
        for integration in self.integrations.iter().take(MAX_REGISTERED_INTEGRATIONS) {
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
    ///
    /// # Arguments
    ///
    /// * `library` - Open library store used for reads/writes.
    ///
    /// # Returns
    ///
    /// `crate::types::SyncListeningSummary` result.
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use bookclerk_plugin_abi::{DomainEvent, EventResult};

    use super::IntegrationRegistry;
    use crate::traits::{Integration, IntegrationContext};
    use crate::types::IntegrationHealth;

    struct Stub {
        instance: Option<&'static str>,
    }

    #[async_trait]
    impl Integration for Stub {
        fn id(&self) -> &str {
            "stub"
        }

        fn plugin_key(&self) -> &str {
            "plugin"
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            self.instance
        }

        async fn start(&self, _ctx: IntegrationContext) -> crate::error::Result<()> {
            Ok(())
        }

        async fn deliver_domain_event(
            &self,
            _event: DomainEvent,
        ) -> crate::error::Result<EventResult> {
            Ok(EventResult::Ack)
        }

        async fn health(&self) -> crate::error::Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: "stub".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }
    }

    #[test]
    fn take_instance_leaves_the_other_instance_of_the_same_key() {
        let mut registry = IntegrationRegistry::new();
        registry.register(Arc::new(Stub {
            instance: Some("instance-a"),
        }));
        registry.register(Arc::new(Stub {
            instance: Some("instance-b"),
        }));
        let retired = registry.take_instance("instance-a");
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].plugin_instance_id(), Some("instance-a"));
        assert_eq!(registry.all().len(), 1);
        assert_eq!(registry.all()[0].plugin_instance_id(), Some("instance-b"));
        assert!(registry.take_instance("instance-a").is_empty());
    }
}
