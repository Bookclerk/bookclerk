//! Reconcile present plugin deployments onto this host.
//!
//! The loop selects `plugin_deployments` for the local [`HostId`] only. Other
//! hosts are not installed and do not receive observation writes. Startup does
//! not download plugins: a missing ledger row is installed only when the caller
//! supplies a local package. A plugin already discovered on disk is left in
//! place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    bounded_observation_detail, current_process_incarnation, ensure_plugin_deployment,
    ensure_plugin_instance, graphicaudio_config_from_pairs, import_instance_config_if_absent,
    list_present_deployments_for_host, load_instance_config, load_observation,
    load_plugin_instance, resolve_instance_bindings, upsert_observation, ConfigActor,
    DeploymentObservation, DeploymentStatus, InstanceBindingGrant, InstancePackagePolicy,
    GRAPHICAUDIO_IMPORT_KEYS, GRAPHICAUDIO_MANIFEST_ID,
};
use bookclerk_library::{LibraryError, LibraryStore};
use bookclerk_plugin_catalog::{
    InstallLedger, InstallOptions, Installer, PluginKey, PluginMutationLock, TrustPolicy,
};
use bookclerk_plugin_sdk::ExtensibleConfig;
use chrono::Utc;

use crate::consent::PluginGrantStore;
use crate::instance_bindings::graphicaudio_plugin_key;
use crate::Result;

/// Local archive the reconciler may install when the ledger has no row.
#[derive(Debug, Clone)]
pub struct LocalPackage {
    /// Archive path (`tar.gz` or `zip`).
    pub archive: PathBuf,
    /// Install-grade package manifest for `archive`.
    pub manifest: bookclerk_plugin_catalog::BookclerkPackageManifest,
}

/// Spawn request after config resolution.
#[derive(Debug, Clone)]
pub struct DeploymentSpawn {
    /// Canonical plugin key.
    pub plugin_key: String,
    /// Installed tree, when discovery or install produced one.
    pub plugin_root: Option<PathBuf>,
    /// Granted `CONFIG` payload.
    pub config: ExtensibleConfig,
    /// Granted `SECRETS` payload.
    pub secrets: ExtensibleConfig,
    /// Document revision the spawn applies.
    pub config_revision: i64,
}

/// Result of spawn plus the health RPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnHealth {
    /// Health succeeded.
    Healthy,
    /// The guest did not start. No config revision was applied.
    SpawnFailed {
        /// Bounded later by the observation writer.
        detail: String,
    },
    /// The guest started and health failed. The install tree stays.
    HealthFailed {
        /// Bounded later by the observation writer.
        detail: String,
    },
}

/// Install health and spawn for one deployment row.
#[async_trait]
pub trait DeploymentRuntime: Send + Sync {
    /// Health of a tree that is not committed yet.
    ///
    /// Failure rolls the install back. The lock is still held.
    async fn health_before_commit(&self, plugin_root: &Path) -> std::result::Result<(), String>;

    /// Spawn the guest and run health.
    async fn spawn_and_health(&self, request: &DeploymentSpawn) -> SpawnHealth;
}

/// Ensures the GraphicAudio instance, its imported document, and a local deployment.
///
/// A second call returns the same instance id. After the document exists this
/// function does not read `[sources.graphicaudio]` or `BOOKCLERK_GA_ACCESS`.
/// Invalid `access` fails the import and does not write the document.
///
/// # Errors
///
/// Returns an error when the ledger cannot be read, the import is invalid, or
/// the deployment row cannot be written.
pub async fn enroll_graphicaudio_instance(
    store: &LibraryStore,
    config: &Config,
    host_id: &str,
) -> Result<()> {
    let Some(plugin_key) = graphicaudio_plugin_key(config)? else {
        return Ok(());
    };
    let actor = ConfigActor::Bootstrap;
    let instance = ensure_plugin_instance(store, &actor, &plugin_key)
        .await
        .map_err(library_err)?;
    match load_instance_config(store, &instance.id).await {
        Ok(_) => {}
        Err(LibraryError::NotFound(_)) => {
            let body =
                graphicaudio_config_from_pairs(graphicaudio_pairs(config)).map_err(library_err)?;
            import_instance_config_if_absent(
                store,
                &actor,
                &instance.id,
                InstancePackagePolicy::GraphicAudio,
                &body,
                &format!("import-ga-{}", instance.id),
            )
            .await
            .map_err(library_err)?;
        }
        Err(err) => return Err(library_err(err)),
    }
    ensure_plugin_deployment(store, &actor, &instance.id, host_id)
        .await
        .map_err(library_err)?;
    Ok(())
}

/// Copies non-secret GraphicAudio keys that are present in TOML.
///
/// Absent keys are omitted. `BOOKCLERK_GA_ACCESS` is not consulted.
fn graphicaudio_pairs(config: &Config) -> Vec<(String, String)> {
    let mut pairs = Vec::new();
    for key in GRAPHICAUDIO_IMPORT_KEYS {
        let Some(value) = config.sources.get_string(GRAPHICAUDIO_MANIFEST_ID, key) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        pairs.push(((*key).to_string(), value.to_string()));
    }
    pairs
}

/// Reconciles every present deployment for `host_id`.
///
/// # Errors
///
/// Returns an error when the deployment list cannot be read. A single row's
/// install, spawn, or observation failure is stored on that row and does not
/// stop the others.
pub async fn reconcile_local_deployments(
    store: &LibraryStore,
    config: &Config,
    host_id: &str,
    packages: &HashMap<String, LocalPackage>,
    runtime: &dyn DeploymentRuntime,
) -> Result<()> {
    let deployments = list_present_deployments_for_host(store, host_id)
        .await
        .map_err(library_err)?;
    for deployment in deployments {
        if let Err(err) =
            reconcile_one(store, config, host_id, &deployment, packages, runtime).await
        {
            tracing::warn!(
                deployment_id = %deployment.deployment_id,
                error = %err,
                "deployment reconcile failed before an observation write"
            );
        }
    }
    Ok(())
}

/// Installs or spawns one deployment and records the observation.
async fn reconcile_one(
    store: &LibraryStore,
    config: &Config,
    host_id: &str,
    deployment: &bookclerk_library::control_plane::PluginDeployment,
    packages: &HashMap<String, LocalPackage>,
    runtime: &dyn DeploymentRuntime,
) -> Result<()> {
    let incarnation = current_process_incarnation().to_string();
    let deployment_id = deployment.deployment_id.clone();
    let note = |status, detail: String, revision| {
        let deployment_id = deployment_id.clone();
        let incarnation = incarnation.clone();
        let host_id = host_id.to_string();
        async move {
            record_observation(
                store,
                &deployment_id,
                &host_id,
                &incarnation,
                status,
                &detail,
                revision,
            )
            .await;
        }
    };

    let Some(instance) = load_plugin_instance(store, &deployment.plugin_instance_id)
        .await
        .map_err(library_err)?
    else {
        note(
            DeploymentStatus::Error,
            "plugin instance is missing".into(),
            None,
        )
        .await;
        return Ok(());
    };

    let grants = PluginGrantStore::load(&config.paths().files_dir)?;
    let Some(grant) = grants.get_by_plugin_key(&instance.plugin_key).cloned() else {
        note(
            DeploymentStatus::Error,
            "plugin grant is missing; deployment was not spawned".into(),
            None,
        )
        .await;
        return Ok(());
    };
    let flags = InstanceBindingGrant {
        config: crate::grant_has_binding(&grant, "config"),
        secrets: crate::grant_has_binding(&grant, "secrets"),
    };
    let bindings = match resolve_instance_bindings(store, &instance.id, &flags).await {
        Ok(bindings) => bindings,
        Err(err) => {
            note(DeploymentStatus::Error, err.to_string(), None).await;
            return Ok(());
        }
    };

    let plugin_key = match PluginKey::parse(&instance.plugin_key) {
        Ok(key) => key,
        Err(err) => {
            note(DeploymentStatus::Error, err.to_string(), None).await;
            return Ok(());
        }
    };
    let files_dir = config.paths().files_dir.clone();
    let ledger = InstallLedger::load(&files_dir)
        .map_err(|err| crate::PluginError::message(err.to_string()))?;
    let in_ledger = ledger.get(&plugin_key).is_some();
    let discovered = discovered_root(config, plugin_key.canonical());

    if !in_ledger && discovered.is_none() {
        let Some(package) = packages.get(&instance.plugin_key) else {
            note(DeploymentStatus::Error, "not installed".into(), None).await;
            return Ok(());
        };
        let plugins_root = files_dir.join("plugins");
        std::fs::create_dir_all(&plugins_root)?;
        let lock = PluginMutationLock::acquire(&files_dir)
            .map_err(|err| crate::PluginError::message(err.to_string()))?;
        let opts = InstallOptions {
            plugins_root,
            offline: true,
            trust: TrustPolicy::allow_unverified_publisher(),
            skip_health: true,
            ..InstallOptions::default()
        };
        let outcome = match Installer::install_local_archive_with_lock(
            &lock,
            &package.archive,
            &package.manifest,
            &opts,
        ) {
            Ok(outcome) => outcome,
            Err(err) => {
                drop(lock);
                note(DeploymentStatus::Error, err.to_string(), None).await;
                return Ok(());
            }
        };
        if let Err(err) = runtime.health_before_commit(&outcome.plugin_root).await {
            let mut detail = err;
            if let Err(rollback) = Installer::rollback(&outcome) {
                detail = format!("{detail}; rollback: {rollback}");
            }
            drop(lock);
            note(DeploymentStatus::Error, detail, None).await;
            return Ok(());
        }
        if let Err(err) = Installer::commit(&outcome) {
            drop(lock);
            note(DeploymentStatus::Error, err.to_string(), None).await;
            return Ok(());
        }
        drop(lock);
    }

    let existing = load_observation(store, &deployment.deployment_id, host_id)
        .await
        .map_err(library_err)?;
    if let Some(existing) = &existing {
        if existing.status == DeploymentStatus::Healthy
            && existing.incarnation == incarnation
            && existing.applied_config_revision == Some(bindings.config_revision)
        {
            return Ok(());
        }
    }

    note(DeploymentStatus::Installed, String::new(), None).await;
    let request = DeploymentSpawn {
        plugin_key: instance.plugin_key.clone(),
        plugin_root: discovered.or_else(|| installed_root(&files_dir, &plugin_key)),
        config: bindings.config,
        secrets: bindings.secrets,
        config_revision: bindings.config_revision,
    };
    note(
        DeploymentStatus::Running,
        String::new(),
        Some(bindings.config_revision),
    )
    .await;
    match runtime.spawn_and_health(&request).await {
        SpawnHealth::Healthy => {
            note(
                DeploymentStatus::Healthy,
                String::new(),
                Some(request.config_revision),
            )
            .await;
        }
        SpawnHealth::SpawnFailed { detail } => {
            note(DeploymentStatus::Error, detail, None).await;
        }
        SpawnHealth::HealthFailed { detail } => {
            note(
                DeploymentStatus::Error,
                detail,
                Some(request.config_revision),
            )
            .await;
        }
    }
    Ok(())
}

/// Install directory when discovery already sees `canonical`.
fn discovered_root(config: &Config, canonical: &str) -> Option<PathBuf> {
    let plugins = crate::discover_plugins(config).ok()?;
    plugins
        .into_iter()
        .find_map(|plugin| (plugin.plugin_key().canonical() == canonical).then_some(plugin.root))
}

/// `plugins/<fs-id>` after a committed install.
fn installed_root(files_dir: &Path, plugin_key: &PluginKey) -> Option<PathBuf> {
    let root = files_dir.join("plugins").join(plugin_key.fs_id());
    root.is_dir().then_some(root)
}

/// Upserts this host's observation. A failed write is retried next tick.
async fn record_observation(
    store: &LibraryStore,
    deployment_id: &str,
    host_id: &str,
    incarnation: &str,
    status: DeploymentStatus,
    detail: &str,
    applied_config_revision: Option<i64>,
) {
    let detail = if status == DeploymentStatus::Healthy {
        String::new()
    } else {
        bounded_observation_detail(detail)
    };
    let observation = DeploymentObservation {
        deployment_id: deployment_id.to_string(),
        host_id: host_id.to_string(),
        incarnation: incarnation.to_string(),
        status,
        detail,
        applied_config_revision,
        observed_at: Utc::now().to_rfc3339(),
    };
    if let Err(err) = upsert_observation(store, &observation).await {
        tracing::warn!(
            deployment_id,
            error = %err,
            "deployment observation upsert failed; retrying next tick"
        );
    }
}

/// Maps a library error onto the host error type.
fn library_err(err: LibraryError) -> crate::PluginError {
    crate::PluginError::message(err.to_string())
}

/// Production runtime: spawn through the existing host adapters and register
/// the session on the live registry.
pub struct LiveDeploymentRuntime {
    /// Live daemon config.
    pub config: Arc<tokio::sync::RwLock<Config>>,
    /// Open library, used for instance bindings.
    pub store: Arc<tokio::sync::RwLock<LibraryStore>>,
    /// Storefront sessions the reconciler owns.
    pub sources: Arc<tokio::sync::RwLock<bookclerk_source::SourceRegistry>>,
    /// Integration sessions the reconciler owns.
    pub integrations: Arc<tokio::sync::RwLock<bookclerk_integrations::IntegrationRegistry>>,
    /// Destination sessions the reconciler owns.
    pub destinations: Arc<tokio::sync::RwLock<crate::DestinationRegistry>>,
}

#[async_trait]
impl DeploymentRuntime for LiveDeploymentRuntime {
    async fn health_before_commit(&self, plugin_root: &Path) -> std::result::Result<(), String> {
        if plugin_root.join("plugin.toml").is_file() {
            Ok(())
        } else {
            Err("installed tree has no plugin.toml".into())
        }
    }

    async fn spawn_and_health(&self, request: &DeploymentSpawn) -> SpawnHealth {
        let config = self.config.read().await.clone();
        let store = self.store.read().await.clone();
        let services = crate::SessionServices::with_event_outbox(store.clone());
        let plugins = match crate::discover_plugins(&config) {
            Ok(plugins) => plugins,
            Err(err) => {
                return SpawnHealth::SpawnFailed {
                    detail: err.to_string(),
                }
            }
        };
        let Some(plugin) = plugins
            .iter()
            .find(|plugin| plugin.plugin_key().canonical() == request.plugin_key)
        else {
            return SpawnHealth::SpawnFailed {
                detail: "installed plugin was not discovered".into(),
            };
        };
        if plugin
            .manifest
            .has_entrypoint(crate::Entrypoint::Storefront)
        {
            return spawn_storefront(self, plugin, &config, services).await;
        }
        if plugin
            .manifest
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
            || plugin
                .manifest
                .families()
                .contains(&crate::PluginFamily::Integration)
        {
            return spawn_integration(self, plugin, &config, services).await;
        }
        if plugin.manifest.has_entrypoint(crate::Entrypoint::Storage) {
            return spawn_storage(self, plugin, &config, &store).await;
        }
        if plugin
            .manifest
            .has_entrypoint(crate::Entrypoint::DatabaseAdapter)
        {
            return SpawnHealth::SpawnFailed {
                detail: "database connect bootstrap is not spawned by the deployment loop".into(),
            };
        }
        SpawnHealth::SpawnFailed {
            detail: "plugin has no entrypoint this host reconciles".into(),
        }
    }
}

/// Spawns a storefront session and records health.
async fn spawn_storefront(
    runtime: &LiveDeploymentRuntime,
    plugin: &crate::DiscoveredPlugin,
    config: &Config,
    services: crate::SessionServices,
) -> SpawnHealth {
    let source = match crate::ExternalSource::spawn_with(plugin, config, services).await {
        Ok(source) => source,
        Err(err) => {
            return SpawnHealth::SpawnFailed {
                detail: err.to_string(),
            }
        }
    };
    match source.check_health().await {
        Ok(()) => {
            runtime.sources.write().await.register(Arc::new(source));
            SpawnHealth::Healthy
        }
        Err(err) => SpawnHealth::HealthFailed {
            detail: err.to_string(),
        },
    }
}

/// Spawns an integration session and records health.
async fn spawn_integration(
    runtime: &LiveDeploymentRuntime,
    plugin: &crate::DiscoveredPlugin,
    config: &Config,
    services: crate::SessionServices,
) -> SpawnHealth {
    let integration = match crate::ExternalIntegration::spawn_with(plugin, config, services).await {
        Ok(integration) => integration,
        Err(err) => {
            return SpawnHealth::SpawnFailed {
                detail: err.to_string(),
            }
        }
    };
    match integration.check_health().await {
        Ok(()) => {
            runtime
                .integrations
                .write()
                .await
                .register(Arc::new(integration));
            SpawnHealth::Healthy
        }
        Err(err) => SpawnHealth::HealthFailed {
            detail: err.to_string(),
        },
    }
}

/// Spawns a storage session and records health.
async fn spawn_storage(
    runtime: &LiveDeploymentRuntime,
    plugin: &crate::DiscoveredPlugin,
    config: &Config,
    store: &LibraryStore,
) -> SpawnHealth {
    let mut registry = runtime.destinations.write().await;
    match crate::host::spawn_deployed_storage(plugin, config, Some(store), &mut registry).await {
        Ok(()) => SpawnHealth::Healthy,
        Err(err) => SpawnHealth::SpawnFailed {
            detail: err.to_string(),
        },
    }
}
