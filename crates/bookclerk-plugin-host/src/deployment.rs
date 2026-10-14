//! Reconcile plugin deployments onto this host.
//!
//! The loop selects `plugin_deployments` for the local [`HostId`] only. Other
//! hosts are not installed and do not receive observation writes. A row whose
//! desired state is `absent` retires the running guest and records `retired`;
//! a `present` row is installed, spawned, and health-checked. Startup does not
//! download plugins: a missing ledger row is installed only when the caller
//! supplies a local package. A plugin already discovered on disk is left in
//! place.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    bounded_observation_detail, current_process_incarnation, default_instance_for_key,
    list_deployments_for_host, load_active_release, load_instance_config, load_observation,
    load_plugin_instance, resolve_instance_bindings, stale_against_pin, upsert_observation,
    ConfigActor, DeploymentObservation, DeploymentStatus, PluginRelease, DESIRED_PRESENT,
};
use bookclerk_library::{LibraryError, LibraryStore};
use bookclerk_plugin_catalog::{InstallLedger, Installer, PluginKey};
use bookclerk_plugin_sdk::ExtensibleConfig;
use chrono::Utc;

use crate::consent::PluginGrantStore;
use crate::instance_bindings::binding_grant_flags;
use crate::Result;

/// Local archive the reconciler may install when the ledger has no row.
#[derive(Debug, Clone)]
pub struct LocalPackage {
    /// Archive path (`tar.gz` or `zip`).
    pub archive: PathBuf,
    /// Install-grade package manifest for `archive`.
    pub manifest: bookclerk_plugin_catalog::BookclerkPackageManifest,
}

/// Operator-placed archives keyed by canonical plugin key.
///
/// A key that two packages both resolve to is removed from the map and
/// recorded in [`Self::is_conflicted`]. Neither package is installed.
#[derive(Clone, Default)]
pub struct AuthorizedLocalPackages {
    /// Archives that resolved to exactly one canonical key.
    packages: HashMap<String, LocalPackage>,
    /// Keys that two archives both claimed. Neither archive is kept.
    conflicted: BTreeSet<String>,
}

impl std::fmt::Debug for AuthorizedLocalPackages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AuthorizedLocalPackages")
            .field("packages", &self.packages.keys().collect::<Vec<_>>())
            .field("conflicted", &self.conflicted)
            .finish()
    }
}

impl AuthorizedLocalPackages {
    /// Package for `key`, when exactly one authorized archive resolved to it.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&LocalPackage> {
        self.packages.get(key)
    }

    /// True when two archives resolved to `key` and neither was kept.
    #[must_use]
    pub fn is_conflicted(&self, key: &str) -> bool {
        self.conflicted.contains(key)
    }

    /// True when no archive was loaded and no key collided.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.packages.is_empty() && self.conflicted.is_empty()
    }

    /// Canonical keys that have exactly one package.
    pub fn keys(&self) -> std::collections::hash_map::Keys<'_, String, LocalPackage> {
        self.packages.keys()
    }

    /// Inserts `package` under `key`, clearing a previous conflict on that key.
    pub fn insert(&mut self, key: String, package: LocalPackage) {
        self.conflicted.remove(&key);
        self.packages.insert(key, package);
    }
}

/// Directory under the files dir that holds operator-placed package archives.
pub const AUTHORIZED_PACKAGE_DIR: &str = "plugin-packages";

/// Spawn request after config resolution.
#[derive(Debug, Clone)]
pub struct DeploymentSpawn {
    /// Instance this deployment runs. Not derived from the plugin key.
    pub plugin_instance_id: String,
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
    /// Cluster release the tree was materialized from; `None` for a
    /// host-local tree.
    pub release: Option<Arc<PluginRelease>>,
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
    /// Spawn the guest and run health. Health is the deployment observation;
    /// there is no pre-commit gate and nothing to roll back.
    async fn spawn_and_health(&self, request: &DeploymentSpawn) -> SpawnHealth;

    /// True when the guest spawned for `plugin_instance_id` is still running.
    ///
    /// The default is true so a runtime that does not track processes keeps
    /// the healthy-observation skip. [`LiveDeploymentRuntime`] checks the
    /// session.
    async fn guest_still_running(&self, plugin_instance_id: &str) -> bool {
        let _ = plugin_instance_id;
        true
    }

    /// Tree the running guest for `plugin_instance_id` was spawned from.
    ///
    /// `None` (the default) means unknown and never forces a respawn.
    async fn guest_root(&self, plugin_instance_id: &str) -> Option<PathBuf> {
        let _ = plugin_instance_id;
        None
    }

    /// Stops a guest this process is running for `plugin_instance_id`.
    ///
    /// Used when a storefront is disabled. The default does nothing so a
    /// runtime that does not track processes still compiles. The deployment
    /// row is left in place so a later enable can start it again.
    async fn retire_instance(&self, plugin_instance_id: &str) {
        let _ = plugin_instance_id;
    }

    /// Instance ids this process is still tracking.
    ///
    /// The default is empty. [`LiveDeploymentRuntime`] returns the guest map
    /// so a deployment row that disappeared can be retired.
    async fn tracked_instance_ids(&self) -> Vec<String> {
        Vec::new()
    }
}

/// One discovery pass shared by every deployment that still needs install or spawn.
///
/// Idle healthy deployments never call [`SharedPlugins::root`].
struct SharedPlugins<'a> {
    /// Live config whose plugin directories are hashed at most once.
    config: &'a Config,
    /// `None` until the first deployment on this tick needs a root.
    cached: std::sync::Mutex<Option<Vec<crate::DiscoveredPlugin>>>,
    /// Package version per installed root, parsed at most once per tick.
    versions: std::sync::Mutex<HashMap<PathBuf, String>>,
}

impl<'a> SharedPlugins<'a> {
    /// Discovery cache for one reconcile tick.
    fn new(config: &'a Config) -> Self {
        Self {
            config,
            cached: std::sync::Mutex::new(None),
            versions: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Semver `version` of the manifest installed at `root`.
    ///
    /// # Panics
    ///
    /// Panics when the version cache lock is poisoned.
    fn installed_version(&self, root: &Path) -> std::result::Result<String, String> {
        if let Some(version) = self
            .versions
            .lock()
            .expect("installed version cache")
            .get(root)
        {
            return Ok(version.clone());
        }
        let version = installed_manifest(root)?
            .package_version()
            .map_err(|err| err.to_string())?;
        self.versions
            .lock()
            .expect("installed version cache")
            .insert(root.to_path_buf(), version.clone());
        Ok(version)
    }

    /// Install directory for `canonical`, hashing plugin trees only on the first call.
    ///
    /// # Panics
    ///
    /// Panics when the discovery cache lock is poisoned.
    fn root(&self, canonical: &str) -> Option<PathBuf> {
        let mut cached = self.cached.lock().expect("shared plugin discovery");
        if cached.is_none() {
            *cached = Some(crate::discover_plugins(self.config).unwrap_or_default());
        }
        cached.as_ref().and_then(|plugins| {
            plugins.iter().find_map(|plugin| {
                (plugin.plugin_key().canonical() == canonical).then(|| plugin.root.clone())
            })
        })
    }
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
    packages: &AuthorizedLocalPackages,
    runtime: &dyn DeploymentRuntime,
) -> Result<()> {
    let rows = list_deployments_for_host(store, host_id)
        .await
        .map_err(library_err)?;
    let (deployments, absent): (Vec<_>, Vec<_>) = rows
        .into_iter()
        .partition(|deployment| deployment.desired == DESIRED_PRESENT);
    let present: std::collections::BTreeSet<String> = deployments
        .iter()
        .map(|deployment| deployment.plugin_instance_id.as_str().to_string())
        .collect();
    let incarnation = current_process_incarnation().to_string();
    let tracked = runtime.tracked_instance_ids().await;
    for deployment in absent {
        let instance_id = deployment.plugin_instance_id.as_str();
        let was_tracked = tracked.iter().any(|id| id == instance_id);
        if was_tracked {
            runtime.retire_instance(instance_id).await;
        }
        let already_retired = load_observation(store, &deployment.deployment_id, host_id)
            .await
            .map_err(library_err)?
            .is_some_and(|existing| existing.status == DeploymentStatus::Retired);
        if was_tracked || !already_retired {
            record_observation(
                store,
                &deployment.deployment_id,
                host_id,
                &incarnation,
                DeploymentStatus::Retired,
                "desired absent",
                None,
            )
            .await;
        }
    }
    let discovered = SharedPlugins::new(config);
    for deployment in deployments {
        if let Err(err) = reconcile_one(
            store,
            config,
            host_id,
            &deployment,
            packages,
            runtime,
            &discovered,
        )
        .await
        {
            tracing::warn!(
                deployment_id = %deployment.deployment_id,
                error = %err,
                "deployment reconcile failed before an observation write"
            );
        }
    }
    for instance_id in runtime.tracked_instance_ids().await {
        if !present.contains(&instance_id) {
            runtime.retire_instance(&instance_id).await;
        }
    }
    sweep_unreferenced_releases(store, config, &present).await;
    Ok(())
}

/// Removes cached release digests no present deployment on this host
/// still runs (after [`crate::RELEASE_CACHE_GRACE`]).
async fn sweep_unreferenced_releases(
    store: &LibraryStore,
    config: &Config,
    present: &std::collections::BTreeSet<String>,
) {
    let files_dir = config.paths().files_dir.clone();
    if !crate::release_cache_root(&files_dir).is_dir() {
        return;
    }
    let mut keep = std::collections::BTreeSet::new();
    for instance_id in present {
        let Ok(id) = bookclerk_library::control_plane::PluginInstanceId::parse(instance_id) else {
            continue;
        };
        let Ok(Some(instance)) = load_plugin_instance(store, &id).await else {
            continue;
        };
        if let Ok(Some(release)) = load_active_release(store, &instance.plugin_key).await {
            if let Ok(artifact) = crate::host_artifact(&release) {
                keep.insert(artifact.payload_root_sha256.clone());
            }
        }
    }
    let swept = tokio::task::spawn_blocking(move || {
        crate::sweep_release_cache(&files_dir, &keep, crate::RELEASE_CACHE_GRACE)
    })
    .await;
    match swept {
        Ok(Ok(0)) => {}
        Ok(Ok(removed)) => tracing::info!(removed, "swept unreferenced release cache entries"),
        Ok(Err(err)) => tracing::warn!(error = %err, "release cache sweep failed"),
        Err(err) => tracing::warn!(error = %err, "release cache sweep task failed"),
    }
}

/// Installs or spawns one deployment and records the observation.
async fn reconcile_one(
    store: &LibraryStore,
    config: &Config,
    host_id: &str,
    deployment: &bookclerk_library::control_plane::PluginDeployment,
    packages: &AuthorizedLocalPackages,
    runtime: &dyn DeploymentRuntime,
    discovered: &SharedPlugins<'_>,
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

    let plugin_key = match PluginKey::parse(&instance.plugin_key) {
        Ok(key) => key,
        Err(err) => {
            note(DeploymentStatus::Error, err.to_string(), None).await;
            return Ok(());
        }
    };
    if packages.is_conflicted(plugin_key.canonical()) {
        note(
            DeploymentStatus::Error,
            format!(
                "two authorized packages resolve to `{}`",
                plugin_key.canonical()
            ),
            None,
        )
        .await;
        return Ok(());
    }
    let files_dir = config.paths().files_dir.clone();

    let existing = load_observation(store, &deployment.deployment_id, host_id)
        .await
        .map_err(library_err)?;
    if let Some(existing) = &existing {
        if error_backoff_active(existing) {
            return Ok(());
        }
    }

    // The active cluster release names what runs; a host-local tree (a
    // platform guest, a development tree) is used only for a plugin with no
    // release.
    let mut active = match load_active_release(store, &instance.plugin_key).await {
        Ok(active) => active,
        Err(err) => {
            note(DeploymentStatus::Error, err.to_string(), None).await;
            return Ok(());
        }
    };
    let mut local_root = None;
    if active.is_none() {
        let ledger = InstallLedger::load(&files_dir)
            .map_err(|err| crate::PluginError::message(err.to_string()))?;
        let in_ledger = ledger.get(&plugin_key).is_some();
        let mut root = installed_root(&files_dir, &plugin_key);
        if root.is_none() {
            root = discovered.root(plugin_key.canonical());
        }
        if !in_ledger && root.is_none() {
            let Some(package) = packages.get(&instance.plugin_key) else {
                note(DeploymentStatus::Error, "not installed".into(), None).await;
                return Ok(());
            };
            // An operator-placed package becomes a cluster release this host
            // publishes; every host placed on it then materializes the same
            // digest. The consent the operator approved for the key rides on
            // the release.
            let grant = PluginGrantStore::load(&files_dir)?
                .get_by_plugin_key(&instance.plugin_key)
                .cloned();
            let request = crate::ReleaseRequest {
                plugin_key: instance.plugin_key.clone(),
                package: package.manifest.clone(),
                archive: Some(package.archive.clone()),
                publisher_verified: false,
                grant,
                activate: crate::Activate::IfNone,
            };
            if let Err(err) = crate::publish_package_release(
                store,
                &ConfigActor::Bootstrap,
                &files_dir,
                host_id,
                request,
            )
            .await
            {
                note(DeploymentStatus::Error, err.to_string(), None).await;
                return Ok(());
            }
            active = match load_active_release(store, &instance.plugin_key).await {
                Ok(active) => active,
                Err(err) => {
                    note(DeploymentStatus::Error, err.to_string(), None).await;
                    return Ok(());
                }
            };
        } else {
            local_root = root;
        }
    }

    let (grant, root, release) = if let Some(release) = active {
        let grant = match crate::release_grant(&release) {
            Ok(grant) => grant,
            Err(err) => {
                note(DeploymentStatus::Error, err.to_string(), None).await;
                return Ok(());
            }
        };
        let root = match crate::materialize_release(store, &files_dir, &release).await {
            Ok(root) => root,
            Err(err) => {
                note(DeploymentStatus::Error, err.to_string(), None).await;
                return Ok(());
            }
        };
        (grant, root, Some(Arc::new(release)))
    } else {
        let Some(root) = local_root else {
            note(DeploymentStatus::Error, "not installed".into(), None).await;
            return Ok(());
        };
        let grants = PluginGrantStore::load(&files_dir)?;
        let Some(grant) = grants.get_by_plugin_key(&instance.plugin_key).cloned() else {
            note(
                DeploymentStatus::Error,
                "plugin grant is missing; deployment was not spawned".into(),
                None,
            )
            .await;
            return Ok(());
        };
        // Cheap checks first. A host-local package older than the pinned
        // schema version is not spawned (its documents may already carry the
        // newer shape). A released plugin's pin is its active release.
        let local_version = match discovered.installed_version(&root) {
            Ok(version) => version,
            Err(err) => {
                note(DeploymentStatus::Error, err, None).await;
                return Ok(());
            }
        };
        match stale_against_pin(store, plugin_key.canonical(), &local_version).await {
            Ok(None) => {}
            Ok(Some(pinned)) => {
                let instance_id = deployment.plugin_instance_id.as_str();
                if runtime
                    .tracked_instance_ids()
                    .await
                    .iter()
                    .any(|id| id == instance_id)
                {
                    runtime.retire_instance(instance_id).await;
                }
                note(
                    DeploymentStatus::Error,
                    format!(
                        "installed version {local_version} is older than {} pinned by host {}; not spawned",
                        pinned.package_version, pinned.recorded_by_host
                    ),
                    None,
                )
                .await;
                return Ok(());
            }
            Err(err) => {
                note(DeploymentStatus::Error, err.to_string(), None).await;
                return Ok(());
            }
        }
        (grant, root, None)
    };
    let document_revision = match load_instance_config(store, &instance.id).await {
        Ok(document) => document.revision,
        Err(err) => {
            note(DeploymentStatus::Error, err.to_string(), None).await;
            return Ok(());
        }
    };
    if let Some(existing) = &existing {
        if existing.status == DeploymentStatus::Healthy
            && existing.incarnation == incarnation
            && existing.applied_config_revision == Some(document_revision)
            && runtime
                .guest_still_running(deployment.plugin_instance_id.as_str())
                .await
            // A moved release pointer changes the tree; the guest is
            // respawned from the new digest.
            && runtime
                .guest_root(deployment.plugin_instance_id.as_str())
                .await
                .is_none_or(|running| running == root)
        {
            return Ok(());
        }
    }
    // The installed manifest's `[[settings]]` is the schema this spawn
    // validates against; keys the operator wrote under an older manifest
    // are dropped and logged by the resolver.
    let schema = match installed_settings_schema(&root) {
        Ok(schema) => schema,
        Err(err) => {
            note(DeploymentStatus::Error, err, None).await;
            return Ok(());
        }
    };
    let mut request = match resolve_spawn(store, &instance, &grant, &schema).await {
        Ok(request) => request,
        Err(err) => {
            note(DeploymentStatus::Error, err, None).await;
            return Ok(());
        }
    };
    note(DeploymentStatus::Installed, String::new(), None).await;
    request.plugin_root = Some(root);
    request.release = release;
    note(
        DeploymentStatus::Running,
        String::new(),
        Some(request.config_revision),
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

/// Resolves `CONFIG` / `SECRETS` for `instance` against `schema` into a spawn request.
async fn resolve_spawn(
    store: &LibraryStore,
    instance: &bookclerk_library::control_plane::PluginInstance,
    grant: &crate::PluginGrant,
    schema: &bookclerk_plugin_abi::SettingsSchema,
) -> std::result::Result<DeploymentSpawn, String> {
    let flags = binding_grant_flags(grant);
    let bindings = resolve_instance_bindings(store, &instance.id, &flags, schema)
        .await
        .map_err(|err| err.to_string())?;
    if !bindings.dropped_keys.is_empty() {
        tracing::warn!(
            plugin_key = %instance.plugin_key,
            plugin_instance_id = %instance.id,
            dropped = ?bindings.dropped_keys,
            "instance document keys not declared by the installed manifest were dropped"
        );
    }
    Ok(DeploymentSpawn {
        plugin_instance_id: instance.id.to_string(),
        plugin_key: instance.plugin_key.clone(),
        plugin_root: None,
        config: bindings.config,
        secrets: bindings.secrets,
        config_revision: bindings.config_revision,
        release: None,
    })
}

/// The manifest installed at `root`.
fn installed_manifest(
    root: &Path,
) -> std::result::Result<bookclerk_plugin_manifest::PluginManifest, String> {
    let text = std::fs::read_to_string(root.join("plugin.toml"))
        .map_err(|err| format!("read plugin.toml: {err}"))?;
    bookclerk_plugin_manifest::parse(&text).map_err(|err| format!("plugin.toml: {err}"))
}

/// `[[settings]]` of the manifest installed at `root`.
fn installed_settings_schema(
    root: &Path,
) -> std::result::Result<bookclerk_plugin_abi::SettingsSchema, String> {
    installed_manifest(root).map(|manifest| manifest.settings_schema())
}

/// Skip respawn while a recent error observation is still inside the backoff.
fn error_backoff_active(existing: &DeploymentObservation) -> bool {
    if existing.status != DeploymentStatus::Error {
        return false;
    }
    let Ok(observed) = chrono::DateTime::parse_from_rfc3339(&existing.observed_at) else {
        return false;
    };
    Utc::now().signed_duration_since(observed.with_timezone(&Utc)) < chrono::TimeDelta::seconds(30)
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
        bounded_observation_detail(&bookclerk_config::redact_str(detail))
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

/// Guest this process spawned for one instance, kept alive across registry
/// replacement when two instances share a plugin key.
struct TrackedGuest {
    /// Session whose process liveness is the skip check.
    session: Arc<crate::PluginSession>,
    /// Tree the guest was spawned from.
    root: Option<PathBuf>,
    /// `CONFIG` passed to `open`.
    config: ExtensibleConfig,
    /// `SECRETS` passed to `open`. Tests read this; production keeps the payload
    /// beside the session that was opened with it.
    #[cfg_attr(not(test), allow(dead_code))]
    secrets: ExtensibleConfig,
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
    /// Sessions keyed by plugin instance id.
    guests: std::sync::Mutex<HashMap<String, TrackedGuest>>,
    /// Lifecycle context for integrations this process starts.
    integration_context: std::sync::Mutex<bookclerk_integrations::IntegrationContext>,
    /// Serializes reconcile passes: the periodic tick and an on-demand pass
    /// after a config reload must not install or spawn the same deployment
    /// twice.
    reconcile_lock: tokio::sync::Mutex<()>,
}

impl LiveDeploymentRuntime {
    /// Runtime bound to the daemon's live registries.
    #[must_use]
    pub fn new(
        config: Arc<tokio::sync::RwLock<Config>>,
        store: Arc<tokio::sync::RwLock<LibraryStore>>,
        sources: Arc<tokio::sync::RwLock<bookclerk_source::SourceRegistry>>,
        integrations: Arc<tokio::sync::RwLock<bookclerk_integrations::IntegrationRegistry>>,
        destinations: Arc<tokio::sync::RwLock<crate::DestinationRegistry>>,
    ) -> Self {
        Self {
            config,
            store,
            sources,
            integrations,
            destinations,
            guests: std::sync::Mutex::new(HashMap::new()),
            integration_context: std::sync::Mutex::new(
                bookclerk_integrations::IntegrationContext::default(),
            ),
            reconcile_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Lock a caller holds for the duration of one reconcile pass.
    #[must_use]
    pub fn reconcile_lock(&self) -> &tokio::sync::Mutex<()> {
        &self.reconcile_lock
    }

    /// Stores the daemon lifecycle context used when a deployment starts an integration.
    ///
    /// # Panics
    ///
    /// Panics when the integration-context lock is poisoned.
    pub fn set_integration_context(&self, ctx: bookclerk_integrations::IntegrationContext) {
        *self
            .integration_context
            .lock()
            .expect("deployment integration context") = ctx;
    }

    /// Context passed to [`bookclerk_integrations::Integration::start`].
    ///
    /// # Panics
    ///
    /// Panics when the integration-context lock is poisoned.
    fn integration_context(&self) -> bookclerk_integrations::IntegrationContext {
        self.integration_context
            .lock()
            .expect("deployment integration context")
            .clone()
    }

    /// Stops every registry entry for `plugin_instance_id` and drops the guest.
    ///
    /// Sources, integrations, and storage slots all hold the session. Removing
    /// only the source leaves an integration claiming events or a destination
    /// still accepting writes. Other instances of the same plugin key stay.
    pub async fn retire_plugin_instance(&self, plugin_instance_id: &str) {
        let retired = self
            .integrations
            .write()
            .await
            .take_instance(plugin_instance_id);
        for integration in retired {
            if let Err(err) = bookclerk_integrations::Integration::stop(integration.as_ref()).await
            {
                tracing::warn!(
                    plugin_instance_id,
                    error = %err,
                    "retired integration stop failed"
                );
            }
        }
        self.destinations
            .write()
            .await
            .remove_deployed(plugin_instance_id);
        self.sources
            .write()
            .await
            .remove_instance(plugin_instance_id);
        self.forget_guest(plugin_instance_id);
    }

    /// Drops the tracked guest for `plugin_instance_id` without taking the source registry.
    ///
    /// Config reload already holds that registry. Dropping this entry stops the
    /// process once the registry no longer keeps the session.
    ///
    /// # Panics
    ///
    /// Panics when the deployment guest lock is poisoned.
    pub fn forget_guest(&self, plugin_instance_id: &str) {
        self.guests
            .lock()
            .expect("deployment guests")
            .remove(plugin_instance_id);
    }

    /// `CONFIG` JSON the live `open` call delivered for `plugin_instance_id`.
    ///
    /// # Panics
    ///
    /// Panics when the deployment guest lock is poisoned.
    #[must_use]
    pub fn opened_config_json(&self, plugin_instance_id: &str) -> Option<serde_json::Value> {
        self.guests
            .lock()
            .expect("deployment guests")
            .get(plugin_instance_id)
            .and_then(|guest| guest.config.json_value().ok())
    }

    /// `SECRETS` JSON the live `open` call delivered for `plugin_instance_id`.
    ///
    /// # Panics
    ///
    /// Panics when the deployment guest lock is poisoned.
    #[cfg(test)]
    #[must_use]
    pub fn opened_secrets_json(&self, plugin_instance_id: &str) -> Option<serde_json::Value> {
        self.guests
            .lock()
            .expect("deployment guests")
            .get(plugin_instance_id)
            .and_then(|guest| guest.secrets.json_value().ok())
    }

    /// Native guest pid for `plugin_instance_id`, when this process spawned it.
    ///
    /// # Panics
    ///
    /// Panics when the deployment guest lock is poisoned.
    #[must_use]
    pub fn tracked_guest_pid(&self, plugin_instance_id: &str) -> Option<u32> {
        self.guests
            .lock()
            .expect("deployment guests")
            .get(plugin_instance_id)
            .and_then(|guest| guest.session.guest_pid())
    }

    /// Replaces the tracked session for one instance.
    ///
    /// # Panics
    ///
    /// Panics when the deployment guest lock is poisoned.
    fn remember(&self, plugin_instance_id: &str, guest: TrackedGuest) {
        self.guests
            .lock()
            .expect("deployment guests")
            .insert(plugin_instance_id.to_string(), guest);
    }
}

/// Loads operator-placed archives under `$FILES_DIR/plugin-packages/<name>/`.
///
/// Each directory holds `package.json` (a [`BookclerkPackageManifest`](bookclerk_plugin_catalog::BookclerkPackageManifest))
/// and `archive.tar.gz`. Artifact URLs must be `file:` paths inside that
/// directory. The reconciler does not download plugins. A missing directory
/// is an empty map.
///
/// # Errors
///
/// Returns an error when `plugin-packages` itself cannot be read. One bad
/// package is skipped and does not abort the rest of the directory.
pub fn load_authorized_local_packages(files_dir: &Path) -> Result<AuthorizedLocalPackages> {
    let root = files_dir.join(AUTHORIZED_PACKAGE_DIR);
    if !root.exists() {
        return Ok(AuthorizedLocalPackages::default());
    }
    let root = root
        .canonicalize()
        .map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?;
    if !root.is_dir() {
        return Err(crate::PluginError::message(
            "plugin-packages is not a directory",
        ));
    }
    let mut loaded = AuthorizedLocalPackages::default();
    for entry in std::fs::read_dir(&root)
        .map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?
    {
        let entry =
            entry.map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Err(err) = load_one_authorized_package(files_dir, &root, &dir, &mut loaded) {
            tracing::warn!(
                path = %dir.display(),
                error = %err,
                "skipping authorized plugin package"
            );
        }
    }
    Ok(loaded)
}

/// Loads one package directory. A bad package is skipped by the caller.
fn load_one_authorized_package(
    files_dir: &Path,
    root: &Path,
    dir: &Path,
    packages: &mut AuthorizedLocalPackages,
) -> Result<()> {
    let dir = dir
        .canonicalize()
        .map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?;
    if !dir.starts_with(root) {
        return Err(crate::PluginError::message(
            "plugin package directory escapes plugin-packages",
        ));
    }
    let manifest_path = dir.join("package.json");
    let archive = dir.join("archive.tar.gz");
    if !manifest_path.is_file() || !archive.is_file() {
        return Err(crate::PluginError::message(format!(
            "plugin package {} needs package.json and archive.tar.gz",
            dir.display()
        )));
    }
    let archive = archive
        .canonicalize()
        .map_err(|err| crate::PluginError::message(format!("plugin package archive: {err}")))?;
    if !archive.starts_with(&dir) {
        return Err(crate::PluginError::message(
            "plugin package archive escapes its directory",
        ));
    }
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|err| crate::PluginError::message(format!("plugin package manifest: {err}")))?;
    let manifest: bookclerk_plugin_catalog::BookclerkPackageManifest = serde_json::from_str(&text)
        .map_err(|err| crate::PluginError::message(format!("plugin package manifest: {err}")))?;
    manifest
        .validate_for_install()
        .map_err(|err| crate::PluginError::message(err.to_string()))?;
    for artifact in &manifest.artifacts {
        if !local_artifact_url(&artifact.url, &dir) {
            return Err(crate::PluginError::message(format!(
                "plugin package `{}` artifact URL is not a file inside {}",
                manifest.id,
                dir.display()
            )));
        }
    }
    let plugins_root = files_dir.join("plugins");
    let coordinate = Installer::local_archive_coordinate(&archive, &manifest);
    let key = Installer::plugin_key_for(&coordinate, &manifest.id, &plugins_root)
        .map_err(|err| crate::PluginError::message(err.to_string()))?;
    insert_authorized_package(
        packages,
        key.canonical(),
        LocalPackage { archive, manifest },
    )
}

/// Records one resolved package. A second package for the same key drops both.
pub(crate) fn insert_authorized_package(
    packages: &mut AuthorizedLocalPackages,
    canonical: &str,
    package: LocalPackage,
) -> Result<()> {
    if packages.packages.contains_key(canonical) || packages.conflicted.contains(canonical) {
        packages.packages.remove(canonical);
        packages.conflicted.insert(canonical.to_string());
        return Err(crate::PluginError::message(format!(
            "two authorized packages resolve to `{canonical}`"
        )));
    }
    packages.packages.insert(canonical.to_string(), package);
    Ok(())
}

/// True when `url` is a `file:` path that stays inside `package_dir`.
fn local_artifact_url(url: &str, package_dir: &Path) -> bool {
    let Ok(parsed) = url::Url::parse(url.trim()) else {
        return false;
    };
    if parsed.scheme() != "file" {
        return false;
    }
    let Ok(path) = parsed.to_file_path() else {
        return false;
    };
    path.canonicalize()
        .is_ok_and(|canon| canon.starts_with(package_dir))
}

#[async_trait]
impl DeploymentRuntime for LiveDeploymentRuntime {
    async fn retire_instance(&self, plugin_instance_id: &str) {
        self.retire_plugin_instance(plugin_instance_id).await;
    }

    async fn spawn_and_health(&self, request: &DeploymentSpawn) -> SpawnHealth {
        let config = self.config.read().await.clone();
        let store = self.store.read().await.clone();
        let services = crate::SessionServices::with_event_outbox(store.clone());
        let Some(root) = request.plugin_root.as_deref() else {
            return SpawnHealth::SpawnFailed {
                detail: "installed plugin was not discovered".into(),
            };
        };
        let opened = match &request.release {
            Some(release) => {
                crate::open_released_plugin(root, release).map_err(|err| err.to_string())
            }
            None => open_installed_plugin(root, &config.paths().files_dir),
        };
        let plugin = match opened {
            Ok(plugin) => plugin,
            Err(err) => {
                return SpawnHealth::SpawnFailed { detail: err };
            }
        };
        let prepared = crate::instance_bindings::prepared_open_from_resolved(
            request.config.clone(),
            request.secrets.clone(),
            request.config_revision,
        );
        if plugin
            .manifest
            .has_entrypoint(crate::Entrypoint::Storefront)
        {
            return spawn_storefront(self, &plugin, &config, services, request, prepared).await;
        }
        if plugin
            .manifest
            .has_entrypoint(crate::Entrypoint::RemoteLibrary)
            || plugin
                .manifest
                .families()
                .contains(&crate::PluginFamily::Integration)
        {
            return spawn_integration(self, &plugin, &config, services, request, prepared).await;
        }
        if plugin.manifest.has_entrypoint(crate::Entrypoint::Storage) {
            return spawn_storage(self, &plugin, &config, &store, request, prepared).await;
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

    async fn guest_still_running(&self, plugin_instance_id: &str) -> bool {
        self.guests
            .lock()
            .expect("deployment guests")
            .get(plugin_instance_id)
            .is_some_and(|guest| guest.session.guest_running())
    }

    async fn guest_root(&self, plugin_instance_id: &str) -> Option<PathBuf> {
        self.guests
            .lock()
            .expect("deployment guests")
            .get(plugin_instance_id)
            .and_then(|guest| guest.root.clone())
    }

    async fn tracked_instance_ids(&self) -> Vec<String> {
        self.guests
            .lock()
            .expect("deployment guests")
            .keys()
            .cloned()
            .collect()
    }
}

/// Builds a discovered plugin from an install tree that is not committed yet.
fn open_installed_plugin(
    plugin_root: &Path,
    files_dir: &Path,
) -> std::result::Result<crate::DiscoveredPlugin, String> {
    let text = std::fs::read_to_string(plugin_root.join("plugin.toml"))
        .map_err(|err| format!("read plugin.toml: {err}"))?;
    let manifest =
        bookclerk_plugin_manifest::parse(&text).map_err(|err| format!("plugin.toml: {err}"))?;
    let command = manifest
        .command
        .clone()
        .ok_or_else(|| "plugin.toml missing command".to_string())?;
    let command = if command.is_absolute() {
        command
    } else {
        plugin_root.join(command)
    };
    if !command.is_file() {
        return Err(format!("staged guest is missing {}", command.display()));
    }
    crate::DiscoveredPlugin::try_new(
        manifest,
        plugin_root.to_path_buf(),
        command,
        Some(files_dir),
    )
    .map_err(|err| err.to_string())
}

/// Spawns a storefront session with the deployment's resolved bindings.
async fn spawn_storefront(
    runtime: &LiveDeploymentRuntime,
    plugin: &crate::DiscoveredPlugin,
    config: &Config,
    services: crate::SessionServices,
    request: &DeploymentSpawn,
    prepared: crate::PreparedOpen,
) -> SpawnHealth {
    let mut source =
        match crate::ExternalSource::spawn_prepared(plugin, config, services, prepared).await {
            Ok(source) => source,
            Err(err) => {
                return SpawnHealth::SpawnFailed {
                    detail: err.to_string(),
                }
            }
        };
    source.bind_plugin_instance(&request.plugin_instance_id);
    let tracked = TrackedGuest {
        session: Arc::clone(source.session()),
        root: request.plugin_root.clone(),
        config: source.opened_config().clone(),
        secrets: source.opened_secrets().clone(),
    };
    match source.check_health().await {
        Ok(()) => {
            runtime.remember(&request.plugin_instance_id, tracked);
            let default_instance = {
                let store = runtime.store.read().await.clone();
                default_instance_for_key(&store, &request.plugin_key)
                    .await
                    .ok()
                    .flatten()
                    .map(|instance| instance.id.to_string())
            };
            let mut sources = runtime.sources.write().await;
            sources.register(Arc::new(source));
            if let Some(default_instance) = default_instance {
                sources.set_default_instance(&request.plugin_key, &default_instance);
            }
            SpawnHealth::Healthy
        }
        Err(err) => SpawnHealth::HealthFailed {
            detail: err.to_string(),
        },
    }
}

/// Spawns an integration session with the deployment's resolved bindings.
async fn spawn_integration(
    runtime: &LiveDeploymentRuntime,
    plugin: &crate::DiscoveredPlugin,
    config: &Config,
    services: crate::SessionServices,
    request: &DeploymentSpawn,
    prepared: crate::PreparedOpen,
) -> SpawnHealth {
    let allow_credential_login =
        crate::host_control_plane::allow_credential_login(&prepared.granted_config);
    let mut integration = match crate::ExternalIntegration::spawn_prepared(
        plugin,
        config,
        services,
        prepared,
        allow_credential_login,
    )
    .await
    {
        Ok(integration) => integration,
        Err(err) => {
            return SpawnHealth::SpawnFailed {
                detail: err.to_string(),
            }
        }
    };
    integration.bind_plugin_instance(&request.plugin_instance_id);
    let tracked = TrackedGuest {
        session: Arc::clone(integration.session()),
        root: request.plugin_root.clone(),
        config: integration.opened_config().clone(),
        secrets: integration.opened_secrets().clone(),
    };
    match integration.check_health().await {
        Ok(()) => {
            let context = runtime.integration_context();
            let retired = runtime
                .integrations
                .write()
                .await
                .take_instance(&request.plugin_instance_id);
            for previous in &retired {
                if let Err(err) = bookclerk_integrations::Integration::stop(previous.as_ref()).await
                {
                    tracing::warn!(
                        plugin_instance_id = %request.plugin_instance_id,
                        error = %err,
                        "retired integration stop failed"
                    );
                }
            }
            if let Err(err) =
                bookclerk_integrations::Integration::start(&integration, context.clone()).await
            {
                for previous in retired {
                    if let Err(restart) = bookclerk_integrations::Integration::start(
                        previous.as_ref(),
                        context.clone(),
                    )
                    .await
                    {
                        tracing::warn!(
                            plugin_instance_id = %request.plugin_instance_id,
                            error = %restart,
                            "could not restart the previous integration"
                        );
                    }
                    runtime.integrations.write().await.register(previous);
                }
                return SpawnHealth::HealthFailed {
                    detail: err.to_string(),
                };
            }
            runtime
                .integrations
                .write()
                .await
                .register(Arc::new(integration));
            runtime.remember(&request.plugin_instance_id, tracked);
            SpawnHealth::Healthy
        }
        Err(err) => SpawnHealth::HealthFailed {
            detail: err.to_string(),
        },
    }
}

/// Spawns a storage session with the deployment's resolved bindings.
async fn spawn_storage(
    runtime: &LiveDeploymentRuntime,
    plugin: &crate::DiscoveredPlugin,
    config: &Config,
    store: &LibraryStore,
    request: &DeploymentSpawn,
    prepared: crate::PreparedOpen,
) -> SpawnHealth {
    let tracked_config = prepared.bindings.config.clone();
    let tracked_secrets = prepared.bindings.secrets.clone();
    let spawned =
        match crate::host::spawn_deployed_storage(plugin, config, Some(store), prepared).await {
            Ok(spawned) => spawned,
            Err(err) => {
                return SpawnHealth::SpawnFailed {
                    detail: err.to_string(),
                };
            }
        };
    let session = {
        let mut registry = runtime.destinations.write().await;
        match registry.publish_spawned(&request.plugin_instance_id, spawned) {
            Ok(session) => session,
            Err(detail) => return SpawnHealth::SpawnFailed { detail },
        }
    };
    runtime.remember(
        &request.plugin_instance_id,
        TrackedGuest {
            session,
            root: request.plugin_root.clone(),
            config: tracked_config,
            secrets: tracked_secrets,
        },
    );
    SpawnHealth::Healthy
}
