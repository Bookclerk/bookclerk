use std::collections::BTreeSet;
use std::sync::Arc;

use bookclerk_config::Config;
use bookclerk_integrations::IntegrationRegistry;
use bookclerk_library::LibraryStore;
use bookclerk_plugin_host::SessionServices;
use bookclerk_source::SourceRegistry;

/// Plugin keys with a present deployment on `host_id`.
///
/// # Errors
///
/// Returns an error when the deployment table cannot be read.
pub async fn deployment_skip_keys(
    store: &LibraryStore,
    host_id: &str,
) -> anyhow::Result<BTreeSet<String>> {
    let keys =
        bookclerk_library::control_plane::present_plugin_keys_for_host(store, host_id).await?;
    Ok(keys.into_iter().collect())
}

/// Plugin instance ids with a present deployment on `host_id`.
///
/// Reload copies a live session only when its instance id is in this set.
///
/// # Errors
///
/// Returns an error when the deployment table cannot be read.
pub async fn deployment_instance_ids(
    store: &LibraryStore,
    host_id: &str,
) -> anyhow::Result<BTreeSet<String>> {
    let deployments =
        bookclerk_library::control_plane::list_present_deployments_for_host(store, host_id).await?;
    Ok(deployments
        .into_iter()
        .map(|deployment| deployment.plugin_instance_id.as_str().to_string())
        .collect())
}

/// Sources for this process, leaving local deployments to the reconciler.
///
/// # Errors
///
/// Returns an error when discovery or guest spawn fails.
pub async fn registry_skipping_deployments(
    config: &Config,
    outbox: &LibraryStore,
    skip: &BTreeSet<String>,
) -> anyhow::Result<SourceRegistry> {
    Ok(bookclerk_plugin_host::load_sources_skipping(
        config,
        &SessionServices::from_outbox(Some(outbox)),
        skip,
    )
    .await?)
}

/// Fresh source registry for a job, reusing sessions the reconciler already owns.
///
/// Each present instance is copied under its plugin instance id. A transitional
/// source that only shares the plugin key is not copied. A plugin key is not
/// treated as a unique address when two instances of that key are live.
///
/// # Errors
///
/// Returns an error when discovery, the deployment table, or guest spawn fails.
pub async fn registry_for_job(state: &crate::api::AppState) -> anyhow::Result<SourceRegistry> {
    let cfg = state.config.read().await.clone();
    let library = state.library.read().await.clone();
    let host =
        bookclerk_library::control_plane::load_or_create_host_identity(&cfg.paths().files_dir)?;
    let skip = deployment_skip_keys(&library, &host.host_id).await?;
    let deployed = deployment_instance_ids(&library, &host.host_id).await?;
    let mut registry = registry_skipping_deployments(&cfg, &library, &skip).await?;
    let live = state.sources.read().await;
    reattach_deployed_sources(&mut registry, &live, &deployed);
    Ok(registry)
}

/// Copies present deployed sources from `live` onto `candidate`.
///
/// `deployed` is plugin instance ids. Reload and job lookup both build a
/// registry that omits those plugin keys. The process-stable guest map still
/// treats the bound sessions as healthy, so those source objects have to stay
/// reachable. A source with no instance id is transitional and is not copied.
pub(crate) fn reattach_deployed_sources(
    candidate: &mut SourceRegistry,
    live: &SourceRegistry,
    deployed: &BTreeSet<String>,
) {
    for source in live.all() {
        if session_is_deployed(source.plugin_instance_id(), deployed) {
            candidate.register(source);
        }
    }
}

/// Copies present deployed integrations from `live` onto `candidate`.
///
/// `deployed` is plugin instance ids. Two instances of one key are both copied
/// when each id is present. A transitional integration for that key is not.
pub(crate) fn reattach_deployed_integrations(
    candidate: &mut IntegrationRegistry,
    live: &IntegrationRegistry,
    deployed: &BTreeSet<String>,
) {
    for integration in live.all() {
        if !session_is_deployed(integration.plugin_instance_id(), deployed) {
            continue;
        }
        let already = candidate
            .all()
            .iter()
            .any(|existing| Arc::ptr_eq(existing, integration));
        if !already {
            candidate.register(Arc::clone(integration));
        }
    }
}

/// True when `instance_id` is a non-empty member of the present deployment set.
fn session_is_deployed(instance_id: Option<&str>, deployed: &BTreeSet<String>) -> bool {
    instance_id.is_some_and(|id| !id.is_empty() && deployed.contains(id))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use bookclerk_integrations::{Integration, IntegrationContext, IntegrationHealth};
    use bookclerk_source::{ContentSource, PortalAuthMode, SourceBrand, SourceRegistry};

    use super::{reattach_deployed_integrations, reattach_deployed_sources};

    const BRAND: SourceBrand = SourceBrand {
        id: "stub",
        name: "Stub",
        bg: "#000000",
        fg: "#ffffff",
        accent: "#111111",
        icon_url: "https://example.invalid/icon",
    };

    struct StubSource {
        key: &'static str,
        instance: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl ContentSource for StubSource {
        fn id(&self) -> &str {
            "stub"
        }

        fn plugin_key(&self) -> &str {
            self.key
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            self.instance
        }

        fn portal_auth_mode(&self) -> PortalAuthMode {
            PortalAuthMode::Password
        }

        fn portal_brand(&self) -> SourceBrand {
            BRAND
        }

        async fn login(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::LoginOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceAccount> {
            Err(bookclerk_source::SourceError::api("stub"))
        }

        async fn list_accounts(
            &self,
            _scope: &bookclerk_library::SourceScope,
        ) -> bookclerk_source::Result<Vec<bookclerk_source::SourceAccount>> {
            Err(bookclerk_source::SourceError::api("stub"))
        }

        async fn scan(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _opts: bookclerk_source::ScanOptions,
        ) -> bookclerk_source::Result<bookclerk_source::ScanSummary> {
            Err(bookclerk_source::SourceError::api("stub"))
        }

        async fn fetch_title(
            &self,
            _scope: &bookclerk_library::SourceScope,
            _account_id: &str,
            _title_id: &str,
            _opts: &bookclerk_source::FetchOptions,
        ) -> bookclerk_source::Result<bookclerk_source::SourceFetch> {
            Err(bookclerk_source::SourceError::api("stub"))
        }
    }

    struct StubIntegration {
        key: &'static str,
        instance: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl Integration for StubIntegration {
        fn id(&self) -> &str {
            "stub"
        }

        fn plugin_key(&self) -> &str {
            self.key
        }

        fn plugin_instance_id(&self) -> Option<&str> {
            self.instance
        }

        async fn start(&self, _ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
            Ok(())
        }

        async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
            Ok(IntegrationHealth {
                id: "stub".into(),
                enabled: true,
                ok: true,
                detail: None,
            })
        }

        async fn deliver_domain_event(
            &self,
            _event: bookclerk_integrations::DomainEvent,
        ) -> bookclerk_integrations::Result<bookclerk_integrations::EventResult> {
            Ok(bookclerk_integrations::EventResult::Ack)
        }
    }

    #[test]
    fn reattach_keeps_present_instances_and_drops_the_transitional_key() {
        let mut one = SourceRegistry::new();
        one.register(Arc::new(StubSource {
            key: "plugin",
            instance: None,
        }));
        one.register(Arc::new(StubSource {
            key: "plugin",
            instance: Some("instance-a"),
        }));
        let only_a = BTreeSet::from(["instance-a".to_string()]);
        let mut copied = SourceRegistry::new();
        reattach_deployed_sources(&mut copied, &one, &only_a);
        let by_key = copied.get("plugin").expect("legacy key");
        assert_eq!(by_key.plugin_instance_id(), Some("instance-a"));

        let mut live = SourceRegistry::new();
        live.register(Arc::new(StubSource {
            key: "plugin",
            instance: None,
        }));
        live.register(Arc::new(StubSource {
            key: "plugin",
            instance: Some("instance-a"),
        }));
        live.register(Arc::new(StubSource {
            key: "plugin",
            instance: Some("instance-b"),
        }));
        let deployed = BTreeSet::from(["instance-a".to_string(), "instance-b".to_string()]);
        let mut candidate = SourceRegistry::new();
        reattach_deployed_sources(&mut candidate, &live, &deployed);
        assert!(candidate.get("plugin").is_none());
        assert_eq!(
            candidate.get("instance-a").expect("a").plugin_instance_id(),
            Some("instance-a")
        );
        assert_eq!(
            candidate.get("instance-b").expect("b").plugin_instance_id(),
            Some("instance-b")
        );
        assert_eq!(candidate.all().len(), 2);

        let mut live_integrations = bookclerk_integrations::IntegrationRegistry::new();
        live_integrations.register(Arc::new(StubIntegration {
            key: "plugin",
            instance: None,
        }));
        live_integrations.register(Arc::new(StubIntegration {
            key: "plugin",
            instance: Some("instance-a"),
        }));
        let mut candidate_integrations = bookclerk_integrations::IntegrationRegistry::new();
        reattach_deployed_integrations(&mut candidate_integrations, &live_integrations, &deployed);
        assert_eq!(candidate_integrations.all().len(), 1);
        assert_eq!(
            candidate_integrations.all()[0].plugin_instance_id(),
            Some("instance-a")
        );
    }
}
