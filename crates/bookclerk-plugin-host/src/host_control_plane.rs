//! One bootstrap for every host process.
//!
//! `bookclerk` (CLI) and `bookclerkd` open the control plane the same way:
//! bootstrap configuration → library database → secret root and host
//! identity → database-backed core domains → plugin enrollment. After that,
//! plugins are loaded only from the deployments the database says are
//! present on this host, with `CONFIG` / `SECRETS` resolved from their
//! instance documents. There is no `config.toml` path for plugin settings
//! and no process that enrolls differently from another.
//!
//! The CLI spawns guests for the lifetime of one command and does not write
//! deployment observations; the daemon's reconciler owns those.

use std::collections::HashMap;
use std::sync::Arc;

use bookclerk_config::Config;
use bookclerk_integrations::IntegrationRegistry;
use bookclerk_library::control_plane::{
    bootstrap_control_plane, default_instance_for_key, list_present_deployments_for_host,
    load_plugin_instance, overlay_events, ControlPlaneSession, PluginDeployment, PluginInstance,
};
use bookclerk_library::LibraryStore;
use bookclerk_source::SourceRegistry;

use crate::discover::DiscoveredPlugin;
use crate::enrollment::{enroll_local_plugins, EnrolledPlugin};
use crate::host::{DatabaseRegistry, DestinationRegistry, ExternalIntegration, ExternalSource};
use crate::instance_bindings::prepare_open_for_instance;
use crate::rpc_session::SessionServices;
use crate::Result;

/// A deployment the database wants present on this host, joined with the
/// instance row and the local install it will run.
#[derive(Debug, Clone)]
pub struct PresentDeployment {
    /// Desired row.
    pub deployment: PluginDeployment,
    /// Instance the row runs.
    pub instance: PluginInstance,
    /// Install discovered on this host, when there is one.
    pub plugin: Option<DiscoveredPlugin>,
}

/// Open control plane for this process.
pub struct HostControlPlane {
    /// Bootstrap config with the database-backed domains overlaid.
    pub config: Config,
    /// Database adapters discovered for bootstrap.
    pub database_registry: DatabaseRegistry,
    /// Open library.
    pub store: LibraryStore,
    /// Host identity, cluster id, and core documents.
    pub session: ControlPlaneSession,
    /// Plugins enrolled on this pass.
    pub enrolled: Vec<EnrolledPlugin>,
    /// Deployments desired present on this host, in deployment-id order.
    pub present: Vec<PresentDeployment>,
    /// Default instance id per plugin key, for routing a bare key.
    pub default_instances: HashMap<String, String>,
}

/// Opens the library database and the control plane for `config`.
///
/// # Errors
///
/// Returns an error when the database cannot be opened, the secret root or
/// host identity cannot be established, or enrollment fails as a whole.
pub async fn open_host_control_plane(config: &Config) -> Result<HostControlPlane> {
    let database_registry = crate::load_external_database(config).await?;
    let store = crate::open_library_store(config, &database_registry)
        .await
        .map_err(library_err)?;
    open_host_control_plane_with_store(config, database_registry, store).await
}

/// [`open_host_control_plane`] on a library that is already open.
///
/// The daemon's config reload uses this so it does not reopen the database.
///
/// # Errors
///
/// Returns an error when bootstrap or enrollment fails.
pub async fn open_host_control_plane_with_store(
    config: &Config,
    database_registry: DatabaseRegistry,
    store: LibraryStore,
) -> Result<HostControlPlane> {
    let HostEnrollment {
        config,
        session,
        enrolled,
    } = enroll_host(config, &store).await?;
    let (present, default_instances) =
        present_deployments(&store, &session.host.host_id, &enrolled).await?;
    Ok(HostControlPlane {
        config,
        database_registry,
        store,
        session,
        enrolled,
        present,
        default_instances,
    })
}

/// Result of [`enroll_host`]: the bootstrap half of a host control plane.
pub struct HostEnrollment {
    /// Bootstrap config with the database-backed domains overlaid.
    pub config: Config,
    /// Host identity, cluster id, and core documents.
    pub session: ControlPlaneSession,
    /// Plugins enrolled on this pass.
    pub enrolled: Vec<EnrolledPlugin>,
}

/// Bootstraps the control plane on an open library and enrolls every local
/// plugin, without loading any guest.
///
/// The daemon's config reload uses this: guests stay with the deployment
/// reconciler, which picks up the enrolled deployments on its next pass.
///
/// # Errors
///
/// Returns an error when the secret root or host identity cannot be
/// established, or enrollment fails as a whole.
pub async fn enroll_host(config: &Config, store: &LibraryStore) -> Result<HostEnrollment> {
    let mut config = config.clone();
    let paths = config.paths().clone();
    let scratch_dir = config.download_cache_dir();
    let session = bootstrap_control_plane(
        store,
        &paths.files_dir,
        &scratch_dir,
        config.auth_password().as_deref(),
        &config.events,
    )
    .await
    .map_err(|err| crate::PluginError::message(err.to_string()))?;
    overlay_events(&mut config, &session.events, &session.cluster_id);
    let enrolled = enroll_local_plugins(store, &config, &session.host.host_id).await?;
    Ok(HostEnrollment {
        config,
        session,
        enrolled,
    })
}

/// Joins present deployment rows with instances and discovered installs.
async fn present_deployments(
    store: &LibraryStore,
    host_id: &str,
    enrolled: &[EnrolledPlugin],
) -> Result<(Vec<PresentDeployment>, HashMap<String, String>)> {
    let by_key: HashMap<&str, &DiscoveredPlugin> = enrolled
        .iter()
        .map(|one| (one.plugin.plugin_key().canonical(), &one.plugin))
        .collect();
    let rows = list_present_deployments_for_host(store, host_id)
        .await
        .map_err(library_err)?;
    let mut present = Vec::with_capacity(rows.len());
    let mut default_instances = HashMap::new();
    for deployment in rows {
        let Some(instance) = load_plugin_instance(store, &deployment.plugin_instance_id)
            .await
            .map_err(library_err)?
        else {
            continue;
        };
        if !default_instances.contains_key(&instance.plugin_key) {
            if let Some(default) = default_instance_for_key(store, &instance.plugin_key)
                .await
                .map_err(library_err)?
            {
                default_instances.insert(instance.plugin_key.clone(), default.id.to_string());
            }
        }
        let plugin = by_key
            .get(instance.plugin_key.as_str())
            .map(|p| (*p).clone());
        present.push(PresentDeployment {
            deployment,
            instance,
            plugin,
        });
    }
    Ok((present, default_instances))
}

impl HostControlPlane {
    /// This host's id.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.session.host.host_id
    }

    /// Present deployments whose plugin is installed here and exports `entrypoint`.
    fn present_with(
        &self,
        entrypoint: crate::Entrypoint,
    ) -> impl Iterator<Item = (&PresentDeployment, &DiscoveredPlugin)> {
        self.present.iter().filter_map(move |row| {
            let plugin = row.plugin.as_ref()?;
            plugin
                .manifest
                .has_entrypoint(entrypoint)
                .then_some((row, plugin))
        })
    }

    /// Present deployments whose plugin is installed here and is an integration.
    fn present_integrations(
        &self,
    ) -> impl Iterator<Item = (&PresentDeployment, &DiscoveredPlugin)> {
        self.present.iter().filter_map(|row| {
            let plugin = row.plugin.as_ref()?;
            (plugin
                .manifest
                .has_entrypoint(crate::Entrypoint::RemoteLibrary)
                || plugin
                    .manifest
                    .families()
                    .contains(&crate::PluginFamily::Integration))
            .then_some((row, plugin))
        })
    }

    /// Spawns every present storefront deployment and registers it under its
    /// instance id, with the plugin's default instance recorded for routing.
    ///
    /// A guest that fails to start is logged and skipped so the rest of the
    /// command still runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the files directory is unavailable.
    pub async fn load_sources(&self, services: &SessionServices) -> Result<SourceRegistry> {
        let files_dir = self.config.paths().files_dir.clone();
        let mut registry = SourceRegistry::new();
        for (row, plugin) in self.present_with(crate::Entrypoint::Storefront) {
            let prepared =
                match prepare_open_for_instance(&self.store, &files_dir, plugin, &row.instance.id)
                    .await
                {
                    Ok(prepared) => prepared,
                    Err(err) => {
                        tracing::warn!(
                            id = %plugin.manifest.id,
                            plugin_instance_id = %row.instance.id,
                            %err,
                            "skipping storefront: instance bindings did not resolve"
                        );
                        continue;
                    }
                };
            match ExternalSource::spawn_prepared(plugin, &self.config, services.clone(), prepared)
                .await
            {
                Ok(mut source) => {
                    source.bind_plugin_instance(row.instance.id.as_str());
                    registry.register(Arc::new(source));
                }
                Err(err) => tracing::warn!(
                    id = %plugin.manifest.id,
                    plugin_instance_id = %row.instance.id,
                    %err,
                    "skipping storefront: guest failed to start"
                ),
            }
        }
        for (key, instance) in &self.default_instances {
            registry.set_default_instance(key, instance);
        }
        Ok(registry)
    }

    /// Spawns every present integration deployment.
    ///
    /// # Errors
    ///
    /// Returns an error when the files directory is unavailable.
    pub async fn load_integrations(
        &self,
        services: &SessionServices,
    ) -> Result<IntegrationRegistry> {
        let files_dir = self.config.paths().files_dir.clone();
        let mut registry = IntegrationRegistry::new();
        for (row, plugin) in self.present_integrations() {
            let prepared =
                match prepare_open_for_instance(&self.store, &files_dir, plugin, &row.instance.id)
                    .await
                {
                    Ok(prepared) => prepared,
                    Err(err) => {
                        tracing::warn!(
                            id = %plugin.manifest.id,
                            plugin_instance_id = %row.instance.id,
                            %err,
                            "skipping integration: instance bindings did not resolve"
                        );
                        continue;
                    }
                };
            let allow_credential_login = allow_credential_login(&prepared.granted_config);
            match ExternalIntegration::spawn_prepared(
                plugin,
                &self.config,
                services.clone(),
                prepared,
                allow_credential_login,
            )
            .await
            {
                Ok(mut integration) => {
                    integration.bind_plugin_instance(row.instance.id.as_str());
                    registry.register(Arc::new(integration));
                }
                Err(err) => tracing::warn!(
                    id = %plugin.manifest.id,
                    plugin_instance_id = %row.instance.id,
                    %err,
                    "skipping integration: guest failed to start"
                ),
            }
        }
        Ok(registry)
    }

    /// Spawns every present storage deployment.
    ///
    /// # Errors
    ///
    /// Returns an error when a present destination guest cannot start: a
    /// destination the database wants here that does not come up would
    /// otherwise silently drop acquire output.
    pub async fn load_destinations(&self) -> Result<DestinationRegistry> {
        let files_dir = self.config.paths().files_dir.clone();
        let mut registry = DestinationRegistry::default();
        for (row, plugin) in self.present_with(crate::Entrypoint::Storage) {
            let prepared =
                prepare_open_for_instance(&self.store, &files_dir, plugin, &row.instance.id)
                    .await?;
            let spawned = crate::host::spawn_deployed_storage(
                plugin,
                &self.config,
                Some(&self.store),
                prepared,
            )
            .await?;
            registry
                .publish_spawned(row.instance.id.as_str(), spawned)
                .map_err(crate::PluginError::message)?;
        }
        Ok(registry)
    }
}

/// `allow_credential_login` from the resolved `CONFIG`; absent means allowed.
#[must_use]
pub fn allow_credential_login(granted_config: &serde_json::Value) -> bool {
    granted_config
        .get("allow_credential_login")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true)
}

/// Loads an already-open host control plane's deployments again, for callers
/// that changed desired state and want a fresh view without re-enrolling.
///
/// # Errors
///
/// Returns an error when the deployment table cannot be read.
pub async fn refresh_present(
    store: &LibraryStore,
    host_id: &str,
    enrolled: &[EnrolledPlugin],
) -> Result<(Vec<PresentDeployment>, HashMap<String, String>)> {
    present_deployments(store, host_id, enrolled).await
}

/// Maps a library error onto the host error type.
fn library_err(err: bookclerk_library::LibraryError) -> crate::PluginError {
    crate::PluginError::message(err.to_string())
}
