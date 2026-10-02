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
    /// Failure rolls the install back. The lock is still held. `request`
    /// carries the deployment's instance id and resolved bindings.
    async fn health_before_commit(
        &self,
        plugin_root: &Path,
        request: &DeploymentSpawn,
    ) -> std::result::Result<(), String>;

    /// Spawn the guest and run health.
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

    /// Stops a guest this process is running for `plugin_instance_id`.
    ///
    /// Used when a storefront is disabled. The default does nothing so a
    /// runtime that does not track processes still compiles. The deployment
    /// row is left in place so a later enable can start it again.
    async fn retire_instance(&self, plugin_instance_id: &str) {
        let _ = plugin_instance_id;
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
}

impl<'a> SharedPlugins<'a> {
    /// Discovery cache for one reconcile tick.
    fn new(config: &'a Config) -> Self {
        Self {
            config,
            cached: std::sync::Mutex::new(None),
        }
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

/// Ensures the GraphicAudio instance, its imported document, and a local deployment.
///
/// A second call returns the same instance id. After the document exists this
/// function does not read `[sources.graphicaudio]` or `BOOKCLERK_GA_ACCESS`.
/// `sources.graphicaudio.enabled = false` returns before creating a deployment.
/// An existing deployment stays so a later enable can start it. Invalid
/// `access` fails the import and does not write the document.
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
    if !config.sources.is_enabled(GRAPHICAUDIO_MANIFEST_ID) {
        return Ok(());
    }
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

    let mut request = DeploymentSpawn {
        plugin_instance_id: instance.id.to_string(),
        plugin_key: instance.plugin_key.clone(),
        plugin_root: None,
        config: bindings.config.clone(),
        secrets: bindings.secrets.clone(),
        config_revision: bindings.config_revision,
    };
    let plugin_key = match PluginKey::parse(&instance.plugin_key) {
        Ok(key) => key,
        Err(err) => {
            note(DeploymentStatus::Error, err.to_string(), None).await;
            return Ok(());
        }
    };
    let files_dir = config.paths().files_dir.clone();
    if let Some(alias) = storefront_alias(&plugin_key, packages, &files_dir) {
        if !config.sources.is_enabled(&alias) {
            runtime.retire_instance(instance.id.as_str()).await;
            return Ok(());
        }
    }

    let existing = load_observation(store, &deployment.deployment_id, host_id)
        .await
        .map_err(library_err)?;
    if let Some(existing) = &existing {
        if existing.status == DeploymentStatus::Healthy
            && existing.incarnation == incarnation
            && existing.applied_config_revision == Some(bindings.config_revision)
            && runtime
                .guest_still_running(deployment.plugin_instance_id.as_str())
                .await
        {
            return Ok(());
        }
    }

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
        if let Err(err) = runtime
            .health_before_commit(&outcome.plugin_root, &request)
            .await
        {
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
        root = Some(outcome.plugin_root);
    }

    let Some(root) = root else {
        note(DeploymentStatus::Error, "not installed".into(), None).await;
        return Ok(());
    };
    note(DeploymentStatus::Installed, String::new(), None).await;
    request.plugin_root = Some(root);
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

/// Display alias when this deployment is a content source.
///
/// Package kind answers first. A GraphicAudio ledger row covers a guest that
/// was installed earlier and is no longer sitting in `plugin-packages/`.
/// This does not hash plugin binaries.
fn storefront_alias(
    plugin_key: &PluginKey,
    packages: &HashMap<String, LocalPackage>,
    files_dir: &Path,
) -> Option<String> {
    if let Some(package) = packages.get(plugin_key.canonical()) {
        if package.manifest.kind == bookclerk_plugin_catalog::PluginKind::Source {
            return Some(package.manifest.id.clone());
        }
        return None;
    }
    let ledger = InstallLedger::load(files_dir).ok()?;
    let row = ledger.get(plugin_key)?;
    row.manifest_id
        .eq_ignore_ascii_case(GRAPHICAUDIO_MANIFEST_ID)
        .then(|| GRAPHICAUDIO_MANIFEST_ID.to_string())
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

/// Guest this process spawned for one instance, kept alive across registry
/// replacement when two instances share a plugin key.
struct TrackedGuest {
    /// Session whose process liveness is the skip check.
    session: Arc<crate::PluginSession>,
    /// `CONFIG` passed to `open`.
    config: ExtensibleConfig,
    /// `SECRETS` passed to `open`.
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
        }
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

    /// Drops the tracked guest and the source registered under `plugin_instance_id`.
    ///
    /// Other instances of the same plugin key stay. Dropping the guest map
    /// entry is what stops the process.
    pub async fn retire_plugin_instance(&self, plugin_instance_id: &str) {
        self.forget_guest(plugin_instance_id);
        self.sources
            .write()
            .await
            .remove_instance(plugin_instance_id);
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
/// Returns an error when a package directory is incomplete, escapes the
/// authorized root, names a remote artifact, or does not parse.
pub fn load_authorized_local_packages(files_dir: &Path) -> Result<HashMap<String, LocalPackage>> {
    let root = files_dir.join(AUTHORIZED_PACKAGE_DIR);
    if !root.exists() {
        return Ok(HashMap::new());
    }
    let root = root
        .canonicalize()
        .map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?;
    if !root.is_dir() {
        return Err(crate::PluginError::message(
            "plugin-packages is not a directory",
        ));
    }
    let mut packages = HashMap::new();
    for entry in std::fs::read_dir(&root)
        .map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?
    {
        let entry =
            entry.map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let dir = dir
            .canonicalize()
            .map_err(|err| crate::PluginError::message(format!("plugin-packages: {err}")))?;
        if !dir.starts_with(&root) {
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
        let text = std::fs::read_to_string(&manifest_path).map_err(|err| {
            crate::PluginError::message(format!("plugin package manifest: {err}"))
        })?;
        let manifest: bookclerk_plugin_catalog::BookclerkPackageManifest =
            serde_json::from_str(&text).map_err(|err| {
                crate::PluginError::message(format!("plugin package manifest: {err}"))
            })?;
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
        let canonical = key.canonical().to_string();
        if packages.contains_key(&canonical) {
            return Err(crate::PluginError::message(format!(
                "two authorized packages resolve to `{canonical}`"
            )));
        }
        packages.insert(canonical, LocalPackage { archive, manifest });
    }
    Ok(packages)
}

/// True when `url` is a `file:` path that stays inside `package_dir`.
fn local_artifact_url(url: &str, package_dir: &Path) -> bool {
    let Some(rest) = url.trim().strip_prefix("file:") else {
        return false;
    };
    let path = if let Some(stripped) = rest.strip_prefix("//") {
        if let Some(after_host) = stripped.strip_prefix("localhost") {
            after_host
        } else if stripped.starts_with('/') {
            stripped
        } else {
            return false;
        }
    } else {
        rest
    };
    let path = Path::new(path);
    path.canonicalize()
        .is_ok_and(|canon| canon.starts_with(package_dir))
}

#[async_trait]
impl DeploymentRuntime for LiveDeploymentRuntime {
    async fn health_before_commit(
        &self,
        plugin_root: &Path,
        request: &DeploymentSpawn,
    ) -> std::result::Result<(), String> {
        let config = self.config.read().await.clone();
        let plugin = open_installed_plugin(plugin_root, &config.paths().files_dir)?;
        let prepared = crate::instance_bindings::prepared_open_from_resolved(
            request.config.clone(),
            request.secrets.clone(),
            request.config_revision,
        );
        probe_guest_health(&plugin, &config, prepared).await
    }

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
        let plugin = match open_installed_plugin(root, &config.paths().files_dir) {
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

/// Starts the staged guest, calls health, and drops the temporary session.
///
/// The install lock is still held. This function does not acquire it.
async fn probe_guest_health(
    plugin: &crate::DiscoveredPlugin,
    config: &Config,
    prepared: crate::PreparedOpen,
) -> std::result::Result<(), String> {
    let services = crate::SessionServices::default();
    if plugin
        .manifest
        .has_entrypoint(crate::Entrypoint::Storefront)
    {
        let source = crate::ExternalSource::spawn_prepared(plugin, config, services, prepared)
            .await
            .map_err(|err| err.to_string())?;
        source.check_health().await.map_err(|err| err.to_string())?;
        drop(source);
        return Ok(());
    }
    if plugin
        .manifest
        .has_entrypoint(crate::Entrypoint::RemoteLibrary)
        || plugin
            .manifest
            .families()
            .contains(&crate::PluginFamily::Integration)
    {
        let integration =
            crate::ExternalIntegration::spawn_prepared(plugin, config, services, prepared, true)
                .await
                .map_err(|err| err.to_string())?;
        integration
            .check_health()
            .await
            .map_err(|err| err.to_string())?;
        drop(integration);
        return Ok(());
    }
    if plugin.manifest.has_entrypoint(crate::Entrypoint::Storage) {
        let session = Arc::new(
            crate::PluginSession::spawn_with(
                plugin,
                config,
                prepared.spawn_config_table,
                crate::OPERATOR_ACCOUNT,
                &[],
                services,
            )
            .await
            .map_err(|err| err.to_string())?,
        );
        session
            .open(prepared.bindings)
            .await
            .map_err(|err| err.to_string())?;
        drop(session);
        return Ok(());
    }
    Err("plugin has no entrypoint this host can health-check".into())
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
        config: source.opened_config().clone(),
        secrets: source.opened_secrets().clone(),
    };
    match source.check_health().await {
        Ok(()) => {
            runtime.remember(&request.plugin_instance_id, tracked);
            runtime.sources.write().await.register(Arc::new(source));
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
    let allow_credential_login = crate::settings_table(config, plugin)
        .get("allow_credential_login")
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
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
        config: integration.opened_config().clone(),
        secrets: integration.opened_secrets().clone(),
    };
    match integration.check_health().await {
        Ok(()) => {
            if let Err(err) = bookclerk_integrations::Integration::start(
                &integration,
                runtime.integration_context(),
            )
            .await
            {
                return SpawnHealth::HealthFailed {
                    detail: err.to_string(),
                };
            }
            let retired = runtime
                .integrations
                .write()
                .await
                .take_instance(&request.plugin_instance_id);
            for previous in retired {
                if let Err(err) = bookclerk_integrations::Integration::stop(previous.as_ref()).await
                {
                    tracing::warn!(
                        plugin_instance_id = %request.plugin_instance_id,
                        error = %err,
                        "retired integration stop failed"
                    );
                }
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
    let mut registry = runtime.destinations.write().await;
    match crate::host::spawn_deployed_storage(plugin, config, Some(store), &mut registry, prepared)
        .await
    {
        Ok(session) => {
            registry.note_deployed_instance(session.instance_key(), &request.plugin_instance_id);
            runtime.remember(
                &request.plugin_instance_id,
                TrackedGuest {
                    session,
                    config: tracked_config,
                    secrets: tracked_secrets,
                },
            );
            SpawnHealth::Healthy
        }
        Err(err) => SpawnHealth::SpawnFailed {
            detail: err.to_string(),
        },
    }
}
