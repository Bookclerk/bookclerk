//! Current-host deployment reconcile: installer transaction, ledger hit,
//! revision apply, foreign host, secrets, and the transitional settings path.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use std::sync::Arc;

use crate::consent::{consent_request, PluginGrant, PluginGrantStore};
use crate::discover::{settings_table_for, DiscoveredPlugin};
use crate::instance_bindings::prepare_open_bindings;
use crate::{
    discover_plugins, load_authorized_local_packages, reconcile_local_deployments,
    AuthorizedLocalPackages, DeploymentRuntime, DeploymentSpawn, LiveDeploymentRuntime,
    LocalPackage, SpawnHealth, AUTHORIZED_PACKAGE_DIR,
};
use async_trait::async_trait;
use bookclerk_config::{Config, EventsConfig, Isolation};
use bookclerk_library::control_plane::{
    create_plugin_instance, ensure_plugin_deployment, import_instance_config_if_absent,
    load_deployment, load_instance_config, load_observation, replace_instance_config,
    seal_instance_secret, ConfigActor, DeploymentStatus, InstanceConfigReplace,
    InstancePackagePolicy, InstanceSecretRefV1, PluginInstanceConfigV1, SettingValue,
    DESIRED_PRESENT,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_catalog::{
    host_bookclerk_target, InstallLedger, InstallOptions, InstallReceipt, Installer, PluginKey,
    TrustPolicy, PLUGIN_MUTATION_LOCK_FILE, RECEIPT_FILE,
};
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Runs `fut` while counting [`discover_plugins`](crate::discover_plugins) calls on this task.
async fn with_discover_count<F, T>(fut: F) -> (T, usize)
where
    F: std::future::Future<Output = T>,
{
    let calls = Arc::new(AtomicUsize::new(0));
    let value = crate::discover::DISCOVER_CALLS
        .scope(Arc::clone(&calls), fut)
        .await;
    (value, calls.load(Ordering::Relaxed))
}

/// Stops `pid` without the `kill` binary, which Windows images do not ship.
#[allow(unsafe_code)]
fn kill_pid(pid: u32) {
    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        assert_eq!(rc, 0, "kill {pid}: {}", std::io::Error::last_os_error());
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, TerminateProcess, PROCESS_TERMINATE,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            assert!(!handle.is_null(), "OpenProcess {pid}");
            let rc = TerminateProcess(handle, 1);
            let _ = CloseHandle(handle);
            assert_ne!(rc, 0, "TerminateProcess {pid}");
        }
    }
}

struct Probe {
    files: PathBuf,
    fail_health: bool,
    health_calls: AtomicUsize,
    spawns: Mutex<Vec<DeploymentSpawn>>,
    retired: Mutex<Vec<String>>,
}

impl Probe {
    fn new(files: PathBuf, fail_health: bool) -> Self {
        Self {
            files,
            fail_health,
            health_calls: AtomicUsize::new(0),
            spawns: Mutex::new(Vec::new()),
            retired: Mutex::new(Vec::new()),
        }
    }

    fn saw_lock(&self) -> bool {
        self.files.join(PLUGIN_MUTATION_LOCK_FILE).is_file()
    }
}

#[async_trait]
impl DeploymentRuntime for Probe {
    async fn health_before_commit(
        &self,
        plugin_root: &Path,
        _request: &DeploymentSpawn,
    ) -> Result<(), String> {
        assert!(
            plugin_root.join("plugin.toml").is_file(),
            "install tree missing plugin.toml"
        );
        assert!(
            self.saw_lock(),
            "PluginMutationLock must be held during health"
        );
        self.health_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_health {
            Err("forced health failure before commit".into())
        } else {
            Ok(())
        }
    }

    async fn spawn_and_health(&self, request: &DeploymentSpawn) -> SpawnHealth {
        self.spawns.lock().expect("spawns").push(request.clone());
        SpawnHealth::Healthy
    }

    async fn retire_instance(&self, plugin_instance_id: &str) {
        self.retired
            .lock()
            .expect("retired")
            .push(plugin_instance_id.to_string());
    }

    async fn tracked_instance_ids(&self) -> Vec<String> {
        let retired = self.retired.lock().expect("retired").clone();
        let mut ids: Vec<String> = self
            .spawns
            .lock()
            .expect("spawns")
            .iter()
            .map(|spawn| spawn.plugin_instance_id.clone())
            .filter(|id| !retired.iter().any(|retired_id| retired_id == id))
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

struct World {
    files: PathBuf,
    store: LibraryStore,
    config: Config,
    host_id: String,
    key: String,
    archive: PathBuf,
    manifest: bookclerk_plugin_catalog::BookclerkPackageManifest,
    instance_id: bookclerk_library::control_plane::PluginInstanceId,
    deployment_id: String,
}

async fn file_store(path: &Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("open sqlite");
    bookclerk_library::apply_host_schema(&db)
        .await
        .expect("schema");
    LibraryStore::from_connection(db)
}

fn mode_body(mode: &str) -> PluginInstanceConfigV1 {
    let mut settings = BTreeMap::new();
    settings.insert("mode".into(), SettingValue::String(mode.into()));
    PluginInstanceConfigV1 {
        settings,
        secret_refs: Vec::new(),
    }
}

fn write_archive(dir: &Path, id: &str) -> PathBuf {
    let staging = dir.join(format!("{id}-stage"));
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        staging.join("plugin.toml"),
        format!(
            "api_version = 3\nid = \"{id}\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
             entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"outbound\"\n"
        ),
    )
    .unwrap();
    std::fs::write(staging.join("echo"), b"#!/bin/sh\necho ok\n").unwrap();
    let archive = dir.join(format!("{id}.tar.gz"));
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .args(["plugin.toml", "echo"])
        .status()
        .expect("tar");
    assert!(status.success(), "tar czf failed");
    archive
}

fn package_manifest(id: &str) -> bookclerk_plugin_catalog::BookclerkPackageManifest {
    use bookclerk_plugin_catalog::{ArtifactTarget, BookclerkPackageManifest, PluginKind};
    BookclerkPackageManifest {
        schema_version: 1,
        protocol: None,
        api_version: 1,
        api_version_max: None,
        min_bookclerk: None,
        kind: PluginKind::Source,
        id: id.into(),
        display_name: Some("Fixture".into()),
        description: None,
        coordinate: None,
        artifacts: vec![ArtifactTarget {
            target: host_bookclerk_target().to_string(),
            url: "file://fixture.tar.gz".into(),
            archive_sha256: "ab".repeat(32),
            archive_root: ".".into(),
            executable: "echo".into(),
            executable_sha256: None,
        }],
        sandbox: Default::default(),
        links: Default::default(),
        yanked: false,
        released_at: None,
        publisher: None,
    }
}

fn save_grant(files: &Path, plugin_key: &str, secrets: bool) {
    let mut grant = PluginGrant::empty();
    grant.schema_version = crate::GRANT_SCHEMA_VERSION;
    grant.plugin_key = plugin_key.to_string();
    grant.plugin_id = "fxdep".into();
    grant.bindings.insert("config".into());
    if secrets {
        grant.bindings.insert("secrets".into());
    }
    grant.network_mode = "deny".into();
    grant.approved_at = "2026-01-01T00:00:00Z".into();
    let mut store = PluginGrantStore::default();
    store.upsert(grant);
    store.save(files).expect("save grant");
}

async fn open_world(secrets: bool) -> World {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().to_path_buf();
    let archive = write_archive(&files, "fxdep");
    let manifest = package_manifest("fxdep");
    let plugins = files.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    let coordinate = Installer::local_archive_coordinate(&archive, &manifest);
    let plugin_key = Installer::plugin_key_for(&coordinate, "fxdep", &plugins).expect("plugin key");
    let key = plugin_key.canonical().to_string();
    std::fs::write(files.join("config.toml"), "").unwrap();
    let config =
        Config::load(Some(files.clone()), Some(files.join("config.toml"))).expect("config");
    let store = file_store(&files.join("library.db")).await;
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        &files,
        None,
        &EventsConfig::default(),
    )
    .await
    .expect("bootstrap");
    let actor = ConfigActor::Bootstrap;
    let instance = create_plugin_instance(&store, &actor, &key)
        .await
        .expect("instance");
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        InstancePackagePolicy::Generic,
        &mode_body("device"),
        "import-fxdep",
    )
    .await
    .expect("import");
    let deployment = ensure_plugin_deployment(&store, &actor, &instance.id, &session.host.host_id)
        .await
        .expect("deployment");
    save_grant(&files, &key, secrets);
    let world = World {
        files,
        store,
        config,
        host_id: session.host.host_id,
        key,
        archive,
        manifest,
        instance_id: instance.id,
        deployment_id: deployment.deployment_id,
    };
    std::mem::forget(dir);
    world
}

fn packages(world: &World) -> AuthorizedLocalPackages {
    let mut packages = AuthorizedLocalPackages::default();
    packages.insert(
        world.key.clone(),
        LocalPackage {
            archive: world.archive.clone(),
            manifest: world.manifest.clone(),
        },
    );
    packages
}

fn config_mode(spawn: &DeploymentSpawn) -> String {
    spawn
        .config
        .json_value()
        .expect("config json")
        .get("mode")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

fn secret_value(spawn: &DeploymentSpawn) -> String {
    spawn
        .secrets
        .json_value()
        .expect("secrets json")
        .get("token")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string()
}

async fn observation(world: &World) -> bookclerk_library::control_plane::DeploymentObservation {
    load_observation(&world.store, &world.deployment_id, &world.host_id)
        .await
        .expect("observation read")
        .expect("observation row")
}

#[tokio::test]
async fn foreign_host_deployment_does_not_install_or_observe() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let foreign = "foreign-host-not-local";
    let foreign_row = ensure_plugin_deployment(
        &world.store,
        &ConfigActor::Bootstrap,
        &world.instance_id,
        foreign,
    )
    .await
    .expect("foreign deployment");
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &AuthorizedLocalPackages::default(),
        &probe,
    )
    .await
    .expect("reconcile local");
    assert_eq!(probe.health_calls.load(Ordering::SeqCst), 0);
    assert!(probe.spawns.lock().expect("spawns").is_empty());
    assert!(
        load_observation(&world.store, &foreign_row.deployment_id, foreign)
            .await
            .expect("read")
            .is_none(),
        "this process must not upsert the other host's observation"
    );
    assert!(
        load_observation(&world.store, &foreign_row.deployment_id, &world.host_id)
            .await
            .expect("read")
            .is_none()
    );
}

#[tokio::test]
async fn installer_commit_reaches_healthy_and_rollback_clears_ledger() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("reconcile");
    assert_eq!(probe.health_calls.load(Ordering::SeqCst), 1);
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    let key = PluginKey::parse(&world.key).unwrap();
    let ledger = InstallLedger::load(&world.files).unwrap();
    assert!(ledger.get(&key).is_some(), "ledger records the install");
    let root = world.files.join("plugins").join(key.fs_id());
    assert!(root.join(RECEIPT_FILE).is_file(), "receipt present");
    InstallReceipt::load(&root).expect("receipt parses");
    let obs = observation(&world).await;
    assert_eq!(obs.status, DeploymentStatus::Healthy);
    assert!(obs.detail.is_empty());
    assert_eq!(obs.applied_config_revision, Some(1));
    let desired = load_deployment(&world.store, &world.deployment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(desired.desired, DESIRED_PRESENT);

    let failed = open_world(false).await;
    let probe = Probe::new(failed.files.clone(), true);
    reconcile_local_deployments(
        &failed.store,
        &failed.config,
        &failed.host_id,
        &packages(&failed),
        &probe,
    )
    .await
    .expect("reconcile failure");
    assert_eq!(probe.health_calls.load(Ordering::SeqCst), 1);
    assert!(probe.spawns.lock().expect("spawns").is_empty());
    let key = PluginKey::parse(&failed.key).unwrap();
    let ledger = InstallLedger::load(&failed.files).unwrap();
    assert!(
        ledger.get(&key).is_none(),
        "rollback must not leave the failed tree as the ledger occupant"
    );
    let obs = observation(&failed).await;
    assert_eq!(obs.status, DeploymentStatus::Error);
    assert!(obs.detail.contains("forced health failure"));
    assert!(obs.applied_config_revision.is_none());
    let desired = load_deployment(&failed.store, &failed.deployment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(desired.desired, DESIRED_PRESENT);
}

#[tokio::test]
async fn disabled_graphicaudio_retires_without_another_spawn() {
    let _guard = TEST_LOCK.lock().await;
    let mut world = open_world(false).await;
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("enabled reconcile");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert!(probe.retired.lock().expect("retired").is_empty());

    world.manifest.id = "graphicaudio".into();
    world.config.sources.set_enabled("graphicaudio", false);
    let (disabled, discover_calls) = with_discover_count(reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    ))
    .await;
    disabled.expect("disabled reconcile");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert_eq!(
        probe.retired.lock().expect("retired").as_slice(),
        &[world.instance_id.as_str().to_string()]
    );
    assert_eq!(discover_calls, 0);
}

#[tokio::test]
async fn disabled_storefront_without_package_dir_retires() {
    let _guard = TEST_LOCK.lock().await;
    let mut world = open_world(false).await;
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("enabled reconcile");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert!(probe.retired.lock().expect("retired").is_empty());

    world.config.sources.set_enabled("fxdep", false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &AuthorizedLocalPackages::default(),
        &probe,
    )
    .await
    .expect("disabled reconcile");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert_eq!(
        probe.retired.lock().expect("retired").as_slice(),
        &[world.instance_id.as_str().to_string()]
    );
    let obs = observation(&world).await;
    assert_eq!(obs.status, DeploymentStatus::Error, "{}", obs.detail);
    assert!(obs.detail.contains("source is disabled"), "{}", obs.detail);
}

#[tokio::test]
async fn deleted_deployment_prunes_the_tracked_guest() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("install");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);

    use sea_orm::ConnectionTrait;
    let db = world.store.db();
    db.execute_unprepared(&format!(
        "DELETE FROM plugin_deployment_observations WHERE deployment_id = '{}'",
        world.deployment_id
    ))
    .await
    .expect("delete observations");
    db.execute_unprepared(&format!(
        "DELETE FROM plugin_deployments WHERE deployment_id = '{}'",
        world.deployment_id
    ))
    .await
    .expect("delete deployment");
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &AuthorizedLocalPackages::default(),
        &probe,
    )
    .await
    .expect("prune");
    assert_eq!(
        probe.retired.lock().expect("retired").as_slice(),
        &[world.instance_id.as_str().to_string()]
    );
}

#[tokio::test]
async fn idle_healthy_tick_does_not_hash_and_two_installs_share_discovery() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("first reconcile");
    let (idle, discover_calls) = with_discover_count(reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    ))
    .await;
    idle.expect("idle reconcile");
    assert_eq!(discover_calls, 0);
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);

    let mut grants = PluginGrantStore::load(&world.files).expect("grants");
    let mut both = packages(&world);
    for (alias, operation) in [("fxleft", "import-left"), ("fxright", "import-right")] {
        let archive = write_archive(&world.files, alias);
        let manifest = package_manifest(alias);
        let plugins = world.files.join("plugins");
        let coordinate = Installer::local_archive_coordinate(&archive, &manifest);
        let key = Installer::plugin_key_for(&coordinate, alias, &plugins)
            .expect("plugin key")
            .canonical()
            .to_string();
        let actor = ConfigActor::Bootstrap;
        let instance = create_plugin_instance(&world.store, &actor, &key)
            .await
            .expect("instance");
        import_instance_config_if_absent(
            &world.store,
            &actor,
            &instance.id,
            InstancePackagePolicy::Generic,
            &mode_body("device"),
            operation,
        )
        .await
        .expect("import");
        ensure_plugin_deployment(&world.store, &actor, &instance.id, &world.host_id)
            .await
            .expect("deployment");
        let mut grant = PluginGrant::empty();
        grant.schema_version = crate::GRANT_SCHEMA_VERSION;
        grant.plugin_key = key.clone();
        grant.plugin_id = alias.into();
        grant.bindings.insert("config".into());
        grant.network_mode = "deny".into();
        grant.approved_at = "2026-01-01T00:00:00Z".into();
        grants.upsert(grant);
        both.insert(key, LocalPackage { archive, manifest });
    }
    grants.save(&world.files).expect("save grants");
    let fresh = Probe::new(world.files.clone(), false);
    let (two, discover_calls) = with_discover_count(reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &both,
        &fresh,
    ))
    .await;
    two.expect("two deployments");
    assert_eq!(fresh.spawns.lock().expect("spawns").len(), 2);
    assert_eq!(discover_calls, 1);
}

#[tokio::test]
async fn ledger_hit_skips_install_and_still_reaches_healthy() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let plugins = world.files.join("plugins");
    let opts = InstallOptions {
        plugins_root: plugins,
        offline: true,
        trust: TrustPolicy::allow_unverified_publisher(),
        skip_health: true,
        ..InstallOptions::default()
    };
    let outcome = Installer::install_local_archive(&world.archive, &world.manifest, &opts)
        .expect("preinstall");
    Installer::commit(&outcome).expect("commit");
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("reconcile");
    assert_eq!(
        probe.health_calls.load(Ordering::SeqCst),
        0,
        "ledger hit skips the installer"
    );
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert_eq!(observation(&world).await.status, DeploymentStatus::Healthy);
}

#[tokio::test]
async fn newer_config_revision_respawns_with_the_new_value() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let probe = Probe::new(world.files.clone(), false);
    let packages = packages(&world);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages,
        &probe,
    )
    .await
    .unwrap();
    assert_eq!(config_mode(&probe.spawns.lock().unwrap()[0]), "device");
    let current = load_instance_config(&world.store, &world.instance_id)
        .await
        .unwrap();
    let applied = replace_instance_config(
        &world.store,
        &ConfigActor::Operator {
            id: "operator".into(),
        },
        &world.instance_id,
        InstancePackagePolicy::Generic,
        current.revision,
        &mode_body("zip"),
        "bump-mode",
    )
    .await
    .unwrap();
    let InstanceConfigReplace::Applied(_) = applied else {
        panic!("expected apply, got {applied:?}");
    };
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages,
        &probe,
    )
    .await
    .unwrap();
    {
        let spawns = probe.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 2);
        assert_eq!(config_mode(&spawns[1]), "zip");
    }
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages,
        &probe,
    )
    .await
    .unwrap();
    assert_eq!(
        probe.spawns.lock().unwrap().len(),
        2,
        "a healthy tick does not reapply the previous body"
    );
    assert_eq!(
        observation(&world).await.applied_config_revision,
        Some(current.revision + 1)
    );
}

#[tokio::test]
async fn sealed_secret_reaches_spawn_and_a_dangling_ref_does_not() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(true).await;
    let secret = "sealed-token-value";
    seal_instance_secret(
        &world.store,
        &world.instance_id,
        &world.key,
        "device_token",
        secret,
    )
    .await
    .unwrap();
    let mut body = mode_body("device");
    body.secret_refs.push(InstanceSecretRefV1 {
        key: "token".into(),
        name: "device_token".into(),
    });
    let current = load_instance_config(&world.store, &world.instance_id)
        .await
        .unwrap();
    replace_instance_config(
        &world.store,
        &ConfigActor::Operator {
            id: "operator".into(),
        },
        &world.instance_id,
        InstancePackagePolicy::Generic,
        current.revision,
        &body,
        "add-secret-ref",
    )
    .await
    .unwrap();
    let probe = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .unwrap();
    {
        let spawns = probe.spawns.lock().unwrap();
        assert_eq!(spawns.len(), 1);
        assert_eq!(secret_value(&spawns[0]), secret);
    }

    let dangling = open_world(true).await;
    let mut body = mode_body("device");
    body.secret_refs.push(InstanceSecretRefV1 {
        key: "token".into(),
        name: "missing_secret".into(),
    });
    let current = load_instance_config(&dangling.store, &dangling.instance_id)
        .await
        .unwrap();
    replace_instance_config(
        &dangling.store,
        &ConfigActor::Operator {
            id: "operator".into(),
        },
        &dangling.instance_id,
        InstancePackagePolicy::Generic,
        current.revision,
        &body,
        "dangling-ref",
    )
    .await
    .unwrap();
    let probe = Probe::new(dangling.files.clone(), false);
    reconcile_local_deployments(
        &dangling.store,
        &dangling.config,
        &dangling.host_id,
        &packages(&dangling),
        &probe,
    )
    .await
    .unwrap();
    assert!(
        probe.spawns.lock().unwrap().is_empty(),
        "dangling ref must not spawn"
    );
    assert_eq!(probe.health_calls.load(Ordering::SeqCst), 0);
    let obs = observation(&dangling).await;
    assert_eq!(obs.status, DeploymentStatus::Error);
    assert!(obs.detail.contains("missing_secret") || obs.detail.contains("secret"));
    assert_eq!(
        load_deployment(&dangling.store, &dangling.deployment_id)
            .await
            .unwrap()
            .unwrap()
            .desired,
        DESIRED_PRESENT
    );
}

#[tokio::test]
async fn plugin_without_an_instance_keeps_transitional_settings() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    std::fs::write(
        files.join("config.toml"),
        "[sources.fixture]\nmode = \"from-toml\"\n",
    )
    .unwrap();
    let config = Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
    let store = file_store(&files.join("library.db")).await;
    bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    let manifest = bookclerk_plugin_manifest::parse(
        "api_version = 3\nid = \"fixture\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
         entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"deny\"\n\n[vars]\n",
    )
    .unwrap();
    let root = files.join("plugin-src");
    std::fs::create_dir_all(&root).unwrap();
    let plugin = DiscoveredPlugin::for_test(manifest, root.clone(), root.join("echo"));
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(files).unwrap();
    let table = settings_table_for(&config, &plugin, plugin.manifest.primary_family());
    assert_eq!(
        table.get("mode").and_then(|value| value.as_str()),
        Some("from-toml")
    );
    let transitional = serde_json::json!({"mode": "from-toml"});
    let prepared = prepare_open_bindings(Some(&store), files, &plugin, transitional.clone())
        .await
        .unwrap();
    assert!(!prepared.from_instance);
    assert_eq!(prepared.granted_config, transitional);

    let actor = ConfigActor::Bootstrap;
    let instance = create_plugin_instance(&store, &actor, plugin.plugin_key().canonical())
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        InstancePackagePolicy::Generic,
        &mode_body("device"),
        "import-fixture",
    )
    .await
    .unwrap();
    let prepared = prepare_open_bindings(Some(&store), files, &plugin, transitional.clone())
        .await
        .unwrap();
    assert!(prepared.from_instance);
    assert_eq!(prepared.granted_config["mode"], "device");
    let bindings = prepared.bindings.config.json_value().unwrap();
    assert_eq!(bindings["mode"], "device");

    ensure_plugin_deployment(&store, &actor, &instance.id, "host-1")
        .await
        .unwrap();
    let prepared = prepare_open_bindings(Some(&store), files, &plugin, transitional.clone())
        .await
        .unwrap();
    assert!(
        prepared.from_instance,
        "a deployment must not restore transitional settings"
    );
    assert_eq!(prepared.granted_config["mode"], "device");

    let second = create_plugin_instance(&store, &actor, plugin.plugin_key().canonical())
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &second.id,
        InstancePackagePolicy::Generic,
        &mode_body("zip"),
        "import-fixture-2",
    )
    .await
    .unwrap();
    let ambiguous = prepare_open_bindings(Some(&store), files, &plugin, transitional.clone())
        .await
        .expect_err("two documents need an instance id");
    assert!(
        ambiguous.to_string().contains("pass a plugin instance id"),
        "{ambiguous}"
    );
    let selected = crate::instance_bindings::prepare_open_bindings_selecting(
        Some(&store),
        files,
        &plugin,
        transitional,
        Some(second.id.as_str()),
    )
    .await
    .unwrap();
    assert!(selected.from_instance);
    assert_eq!(selected.granted_config["mode"], "zip");
}

#[test]
fn resolver_source_does_not_consult_family_settings() {
    let source = include_str!("instance_bindings.rs");
    let resolve = source
        .split("pub async fn prepare_open_bindings")
        .nth(1)
        .unwrap();
    assert!(!resolve.contains("settings_table_for"));
    assert!(!resolve.contains("primary_family"));
}

fn graphicaudio_binary() -> PathBuf {
    let name = "bookclerk-plugin-source-graphicaudio";
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let candidate = target
        .join(profile)
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(
        candidate.is_file(),
        "bookclerk-plugin-source-graphicaudio is missing at {}",
        candidate.display()
    );
    candidate
}

fn place_guest_binary(binary: &Path, dest: &Path) {
    let _ = std::fs::remove_file(dest);
    if std::fs::hard_link(binary, dest).is_err() {
        std::fs::copy(binary, dest).expect("copy guest binary");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dest).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dest, perms).unwrap();
    }
}

fn stage_graphicaudio_tree(files: &Path) -> PathBuf {
    let install = files.join("plugins").join("graphicaudio");
    std::fs::create_dir_all(&install).unwrap();
    let toml = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/bookclerk-plugins/optional/source-graphicaudio/plugin.toml");
    std::fs::copy(toml, install.join("plugin.toml")).unwrap();
    let binary_name = format!(
        "bookclerk-plugin-source-graphicaudio{}",
        std::env::consts::EXE_SUFFIX
    );
    if !std::env::consts::EXE_SUFFIX.is_empty() {
        let text = std::fs::read_to_string(install.join("plugin.toml")).unwrap();
        std::fs::write(
            install.join("plugin.toml"),
            text.replace(
                "./bookclerk-plugin-source-graphicaudio",
                &format!("./{binary_name}"),
            ),
        )
        .unwrap();
    }
    place_guest_binary(&graphicaudio_binary(), &install.join(binary_name));
    install
}

fn live_runtime(store: &LibraryStore, config: &Config) -> LiveDeploymentRuntime {
    LiveDeploymentRuntime::new(
        Arc::new(tokio::sync::RwLock::new(config.clone())),
        Arc::new(tokio::sync::RwLock::new(store.clone())),
        Arc::new(tokio::sync::RwLock::new(
            bookclerk_source::SourceRegistry::new(),
        )),
        Arc::new(tokio::sync::RwLock::new(
            bookclerk_integrations::IntegrationRegistry::new(),
        )),
        Arc::new(tokio::sync::RwLock::new(
            crate::DestinationRegistry::default(),
        )),
    )
}

fn ga_settings(base_url: &str) -> PluginInstanceConfigV1 {
    let mut settings = BTreeMap::new();
    settings.insert("access".into(), SettingValue::String("device".into()));
    settings.insert("base_url".into(), SettingValue::String(base_url.into()));
    PluginInstanceConfigV1 {
        settings,
        secret_refs: Vec::new(),
    }
}

fn ga_body(base_url: &str, secret_name: &str) -> PluginInstanceConfigV1 {
    let mut settings = BTreeMap::new();
    settings.insert("access".into(), SettingValue::String("device".into()));
    settings.insert("base_url".into(), SettingValue::String(base_url.into()));
    PluginInstanceConfigV1 {
        settings,
        secret_refs: vec![InstanceSecretRefV1 {
            key: "token".into(),
            name: secret_name.into(),
        }],
    }
}

async fn save_manifest_grant(files: &Path, plugin: &DiscoveredPlugin) {
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(files).unwrap();
}

#[tokio::test]
async fn key_lookup_uses_the_document_when_a_deployment_exists() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    let store = file_store(&files.join("library.db")).await;
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    let manifest = bookclerk_plugin_manifest::parse(
        "api_version = 3\nid = \"fixture\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
         entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"deny\"\n\n[vars]\n",
    )
    .unwrap();
    let root = files.join("plugin-src");
    std::fs::create_dir_all(&root).unwrap();
    let plugin = DiscoveredPlugin::for_test(manifest, root.clone(), root.join("echo"));
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(files).unwrap();
    let actor = ConfigActor::Bootstrap;
    let instance = create_plugin_instance(&store, &actor, plugin.plugin_key().canonical())
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        InstancePackagePolicy::Generic,
        &mode_body("device"),
        "import-lookup",
    )
    .await
    .unwrap();
    ensure_plugin_deployment(&store, &actor, &instance.id, &session.host.host_id)
        .await
        .unwrap();
    let prepared = prepare_open_bindings(
        Some(&store),
        files,
        &plugin,
        serde_json::json!({"mode": "toml"}),
    )
    .await
    .unwrap();
    assert!(
        prepared.from_instance,
        "a deployed instance document stays the authority"
    );
    assert_eq!(prepared.granted_config["mode"], "device");
}

#[tokio::test]
async fn live_two_instances_same_key_open_with_distinct_config_and_secrets() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_graphicaudio_tree(files);
    std::fs::write(files.join("config.toml"), "").unwrap();
    let mut config =
        Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
    config.plugins.isolation = Isolation::Off;
    let store = file_store(&files.join("library.db")).await;
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    let plugin = discover_plugins(&config)
        .unwrap()
        .into_iter()
        .find(|plugin| plugin.manifest.id == "graphicaudio")
        .expect("staged graphicaudio");
    save_manifest_grant(files, &plugin).await;
    let actor = ConfigActor::Bootstrap;
    let key = plugin.plugin_key().canonical();
    let first = create_plugin_instance(&store, &actor, key).await.unwrap();
    let second = create_plugin_instance(&store, &actor, key).await.unwrap();
    assert_ne!(first.id, second.id);
    seal_instance_secret(&store, &first.id, key, "alpha", "secret-alpha")
        .await
        .unwrap();
    seal_instance_secret(&store, &second.id, key, "beta", "secret-beta")
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &first.id,
        InstancePackagePolicy::GraphicAudio,
        &ga_body("http://alpha.example", "alpha"),
        "import-alpha",
    )
    .await
    .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &second.id,
        InstancePackagePolicy::GraphicAudio,
        &ga_body("http://beta.example", "beta"),
        "import-beta",
    )
    .await
    .unwrap();
    let deploy_a = ensure_plugin_deployment(&store, &actor, &first.id, &session.host.host_id)
        .await
        .unwrap();
    let deploy_b = ensure_plugin_deployment(&store, &actor, &second.id, &session.host.host_id)
        .await
        .unwrap();
    let runtime = live_runtime(&store, &config);
    reconcile_local_deployments(
        &store,
        &config,
        &session.host.host_id,
        &AuthorizedLocalPackages::default(),
        &runtime,
    )
    .await
    .unwrap();
    for deployment in [&deploy_a, &deploy_b] {
        let obs = load_observation(&store, &deployment.deployment_id, &session.host.host_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
    }
    let config_a = runtime
        .opened_config_json(first.id.as_str())
        .expect("first open");
    let config_b = runtime
        .opened_config_json(second.id.as_str())
        .expect("second open");
    assert_eq!(config_a["base_url"], "http://alpha.example");
    assert_eq!(config_b["base_url"], "http://beta.example");
    let secrets_a = runtime
        .opened_secrets_json(first.id.as_str())
        .expect("first secrets");
    let secrets_b = runtime
        .opened_secrets_json(second.id.as_str())
        .expect("second secrets");
    assert_eq!(secrets_a["token"], "secret-alpha");
    assert_eq!(secrets_b["token"], "secret-beta");
    let sources = runtime.sources.read().await;
    let registered_a = sources
        .get(first.id.as_str())
        .expect("registry reaches the first instance");
    let registered_b = sources
        .get(second.id.as_str())
        .expect("registry reaches the second instance");
    assert_eq!(registered_a.plugin_instance_id(), Some(first.id.as_str()));
    assert_eq!(registered_b.plugin_instance_id(), Some(second.id.as_str()));
    assert_eq!(
        registered_a.guest_pid(),
        runtime.tracked_guest_pid(first.id.as_str())
    );
    assert_eq!(
        registered_b.guest_pid(),
        runtime.tracked_guest_pid(second.id.as_str())
    );
    assert!(
        sources.get(key).is_none(),
        "plugin key is not a unique source address"
    );
}

#[tokio::test]
async fn terminating_a_healthy_guest_is_replaced_on_the_next_tick() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_graphicaudio_tree(files);
    std::fs::write(files.join("config.toml"), "").unwrap();
    let mut config =
        Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
    config.plugins.isolation = Isolation::Off;
    let store = file_store(&files.join("library.db")).await;
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    let plugin = discover_plugins(&config)
        .unwrap()
        .into_iter()
        .find(|plugin| plugin.manifest.id == "graphicaudio")
        .expect("staged graphicaudio");
    save_manifest_grant(files, &plugin).await;
    let actor = ConfigActor::Bootstrap;
    let instance = create_plugin_instance(&store, &actor, plugin.plugin_key().canonical())
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        InstancePackagePolicy::GraphicAudio,
        &ga_settings("http://live.example"),
        "import-live",
    )
    .await
    .unwrap();
    let deployment = ensure_plugin_deployment(&store, &actor, &instance.id, &session.host.host_id)
        .await
        .unwrap();
    let runtime = live_runtime(&store, &config);
    reconcile_local_deployments(
        &store,
        &config,
        &session.host.host_id,
        &AuthorizedLocalPackages::default(),
        &runtime,
    )
    .await
    .unwrap();
    let first_pid = runtime
        .tracked_guest_pid(instance.id.as_str())
        .expect("guest pid");
    kill_pid(first_pid);
    let mut dead = false;
    for _ in 0..50 {
        if !DeploymentRuntime::guest_still_running(&runtime, instance.id.as_str()).await {
            dead = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(dead, "killed guest still looks alive");
    reconcile_local_deployments(
        &store,
        &config,
        &session.host.host_id,
        &AuthorizedLocalPackages::default(),
        &runtime,
    )
    .await
    .unwrap();
    let second_pid = runtime
        .tracked_guest_pid(instance.id.as_str())
        .expect("replacement pid");
    assert_ne!(first_pid, second_pid);
    assert!(DeploymentRuntime::guest_still_running(&runtime, instance.id.as_str()).await);
    let obs = load_observation(&store, &deployment.deployment_id, &session.host.host_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
}

#[tokio::test]
async fn broken_guest_health_rolls_the_install_back() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let mut config = world.config.clone();
    config.plugins.isolation = Isolation::Off;
    let runtime = live_runtime(&world.store, &config);
    reconcile_local_deployments(
        &world.store,
        &config,
        &world.host_id,
        &packages(&world),
        &runtime,
    )
    .await
    .unwrap();
    let ledger = InstallLedger::load(&world.files).unwrap();
    let key = PluginKey::parse(&world.key).unwrap();
    assert!(
        ledger.get(&key).is_none(),
        "broken guest health must roll the ledger back"
    );
    let obs = observation(&world).await;
    assert_eq!(obs.status, DeploymentStatus::Error, "{}", obs.detail);
    assert_eq!(
        load_deployment(&world.store, &world.deployment_id)
            .await
            .unwrap()
            .unwrap()
            .desired,
        DESIRED_PRESENT
    );
}

#[test]
fn authorized_package_loader_skips_a_remote_artifact_url() {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    let package = files.join(AUTHORIZED_PACKAGE_DIR).join("remote");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("archive.tar.gz"), b"not-a-tar").unwrap();
    let archive = package.join("archive.tar.gz").canonicalize().unwrap();
    let manifest = serde_json::json!({
        "schema_version": 1,
        "api_version": 1,
        "kind": "source",
        "id": "fxdep",
        "artifacts": [{
            "target": host_bookclerk_target(),
            "url": "https://example.invalid/fxdep.tar.gz",
            "archive_sha256": "ab".repeat(32),
            "executable": "echo"
        }]
    });
    let _ = archive;
    std::fs::write(
        package.join("package.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let loaded = load_authorized_local_packages(files).expect("bad package is skipped");
    assert!(
        loaded.is_empty(),
        "a remote artifact must not enter the package map"
    );
}

#[test]
fn duplicate_authorized_packages_reject_both() {
    let mut loaded = AuthorizedLocalPackages::default();
    let package = LocalPackage {
        archive: PathBuf::from("archive.tar.gz"),
        manifest: package_manifest("fxdep"),
    };
    crate::deployment::insert_authorized_package(&mut loaded, "path:same", package.clone())
        .expect("first package");
    let err = crate::deployment::insert_authorized_package(&mut loaded, "path:same", package)
        .expect_err("duplicate key");
    assert!(
        err.to_string()
            .contains("two authorized packages resolve to `path:same`"),
        "{err}"
    );
    assert!(loaded.get("path:same").is_none());
    assert!(loaded.is_conflicted("path:same"));
    assert!(loaded.keys().next().is_none());
}
