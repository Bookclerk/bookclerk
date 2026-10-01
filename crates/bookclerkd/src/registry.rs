use std::collections::BTreeSet;

use bookclerk_config::Config;
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
    for source in live.all() {
        if owned.iter().any(|key| key == source.plugin_key())
            && registry.get(source.plugin_key()).is_none()
        {
            registry.register(source);
        }
    }
    Ok(registry)
}
