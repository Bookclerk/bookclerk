//! Plugin host access for CLI commands.
//!
//! The CLI opens the control plane the same way `bookclerkd` does
//! ([`bookclerk_plugin_host::open_host_control_plane`]): every local plugin
//! is enrolled, and guests come only from the deployments the database wants
//! present on this host, with `CONFIG` / `SECRETS` resolved from their
//! instance documents. The CLI spawns guests for the lifetime of one command
//! and writes no deployment observations; those stay with the daemon's
//! reconciler.

use std::sync::Arc;

use bookclerk_config::Config;
use bookclerk_integrations::IntegrationRegistry;
use bookclerk_library::LibraryStore;
use bookclerk_plugin_host::{DestinationRegistry, HostControlPlane, SessionServices};
use bookclerk_source::{ContentSource, SourceRegistry};

/// Opens the library and enrolls local plugins. Loads no guest.
///
/// # Errors
///
/// Returns an error when the database cannot be opened or the control plane
/// cannot be bootstrapped.
pub async fn open_host(config: &Config) -> anyhow::Result<HostControlPlane> {
    Ok(bookclerk_plugin_host::open_host_control_plane(config).await?)
}

/// Opens the library and aligns the cluster secret root, without enrolling
/// plugins. For commands that never load a guest.
///
/// Alignment runs before the caller can cache a data-encryption key or seal a
/// secret. An enrolled database with a missing or different `master.key` fails
/// here, and this function does not leave a replacement key behind.
///
/// # Errors
///
/// Returns an error when the database cannot be opened or the secret root
/// does not align.
pub async fn open_library(config: &Config) -> anyhow::Result<LibraryStore> {
    let registry = bookclerk_plugin_host::load_external_database(config).await?;
    let store = bookclerk_plugin_host::open_library_store(config, &registry).await?;
    // Do not `?` the alignment result. Its success value is cluster metadata,
    // not this store. A missing or wrong key still fails the open.
    if let Err(err) = bookclerk_library::control_plane::align_cluster_root(
        &store,
        &config.paths().files_dir,
        config.auth_password().as_deref(),
    )
    .await
    {
        return Err(err.into());
    }
    Ok(store)
}

/// Spawns every storefront deployment present on this host.
///
/// # Errors
///
/// Returns an error when the files directory is unavailable.
pub async fn sources(host: &HostControlPlane) -> anyhow::Result<SourceRegistry> {
    Ok(host
        .load_sources(&SessionServices::from_outbox(Some(&host.store)))
        .await?)
}

/// Spawns every integration deployment present on this host.
///
/// # Errors
///
/// Returns an error when the files directory is unavailable.
pub async fn integrations(host: &HostControlPlane) -> anyhow::Result<IntegrationRegistry> {
    Ok(host
        .load_integrations(&SessionServices::from_outbox(Some(&host.store)))
        .await?)
}

/// Spawns every storage deployment present on this host.
///
/// # Errors
///
/// Returns an error when a present destination guest cannot start.
pub async fn destinations(host: &HostControlPlane) -> anyhow::Result<DestinationRegistry> {
    Ok(host.load_destinations().await?)
}

/// Resolves `--source` (plugin instance id, PluginKey, or alias) and an
/// optional `--instance` pin to one registered source.
///
/// A bare key or alias routes to the plugin's default instance. An unknown
/// needle lists what is registered; a pin that is not an instance of that
/// plugin fails rather than falling back.
///
/// # Errors
///
/// Returns an error when nothing matches.
pub fn resolve_source(
    registry: &SourceRegistry,
    needle: &str,
    instance: Option<&str>,
) -> anyhow::Result<Arc<dyn ContentSource>> {
    match registry.require_routed(needle, instance) {
        Ok(source) => Ok(source),
        Err(err) => {
            let known: Vec<String> = registry
                .all()
                .into_iter()
                .map(|src| match src.plugin_instance_id() {
                    Some(id) if !id.is_empty() => format!("{} ({id})", src.id()),
                    _ => src.id().to_string(),
                })
                .collect();
            if known.is_empty() {
                Err(anyhow::anyhow!(
                    "{err} (no storefront is deployed on this host — `bookclerk plugins enable <plugin>`)"
                ))
            } else {
                Err(anyhow::anyhow!("{err} (registered: {})", known.join(", ")))
            }
        }
    }
}
