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
/// Each deployed instance is copied under its plugin instance id. A plugin key
/// is not treated as a unique address when two instances of that key are live.
///
/// # Errors
///
/// Returns an error when discovery, the deployment table, or guest spawn fails.
pub async fn registry_for_job(state: &crate::api::AppState) -> anyhow::Result<SourceRegistry> {
    let cfg = state.config.read().await.clone();
    let library = state.library.read().await.clone();
    let host =
        bookclerk_library::control_plane::load_or_create_host_identity(&cfg.paths().files_dir)?;
    let owned = deployment_skip_keys(&library, &host.host_id).await?;
    let mut registry = registry_skipping_deployments(&cfg, &library, &owned).await?;
    let live = state.sources.read().await;
    reattach_deployed_sources(&mut registry, &live, &owned);
    Ok(registry)
}

/// Copies deployed sources from `live` onto `candidate`.
///
/// Reload and job lookup both build a registry that omits those plugin keys.
/// The process-stable guest map still treats the sessions as healthy, so the
/// same source objects have to stay reachable.
pub(crate) fn reattach_deployed_sources(
    candidate: &mut SourceRegistry,
    live: &SourceRegistry,
    owned: &BTreeSet<String>,
) {
    for source in live.all() {
        if owned.iter().any(|key| key == source.plugin_key()) {
            candidate.register(source);
        }
    }
}

/// Copies deployed integrations from `live` onto `candidate`.
///
/// A plugin key already present as the same session is left in place. Two
/// instances of one key are both copied.
pub(crate) fn reattach_deployed_integrations(
    candidate: &mut IntegrationRegistry,
    live: &IntegrationRegistry,
    owned: &BTreeSet<String>,
) {
    for integration in live.all() {
        if !owned.contains(integration.plugin_key()) {
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
