//! Current-host deployment reconcile: operator packages published as
//! releases, materialization by digest (also on a second host), release
//! pointer moves, a host-local ledger hit, revision apply, foreign host,
//! secrets, desired `absent`, and instance document resolution against the
//! installed `[[settings]]` schema.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use std::sync::Arc;

use crate::consent::{consent_request, PluginGrant, PluginGrantStore};
use crate::discover::DiscoveredPlugin;
use crate::instance_bindings::prepare_open_for_instance;
use crate::{
    discover_plugins, load_authorized_local_packages, reconcile_local_deployments,
    AuthorizedLocalPackages, DeploymentRuntime, DeploymentSpawn, LiveDeploymentRuntime,
    LocalPackage, SpawnHealth, AUTHORIZED_PACKAGE_DIR,
};
use async_trait::async_trait;
use bookclerk_config::{Config, EventsConfig, Isolation};
use bookclerk_library::control_plane::{
    activate_release, load_active_release, RELEASE_BLOB_SCHEME,
};
use bookclerk_library::control_plane::{
    create_plugin_instance, ensure_plugin_deployment, import_instance_config_if_absent,
    load_deployment, load_instance_config, load_observation, replace_instance_config,
    replace_plugin_deployment, seal_instance_secret, ConfigActor, DeploymentReplace,
    DeploymentStatus, InstanceConfigReplace, InstanceSecretRefV1, PluginInstanceConfigV1,
    SettingValue, DESIRED_ABSENT, DESIRED_PRESENT,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_catalog::{
    host_bookclerk_target, InstallLedger, InstallOptions, Installer, PluginKey, TrustPolicy,
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
    fail_health: bool,
    spawns: Mutex<Vec<DeploymentSpawn>>,
    retired: Mutex<Vec<String>>,
}

impl Probe {
    fn new(_files: PathBuf, fail_health: bool) -> Self {
        Self {
            fail_health,
            spawns: Mutex::new(Vec::new()),
            retired: Mutex::new(Vec::new()),
        }
    }

    fn last_root(&self) -> PathBuf {
        self.spawns
            .lock()
            .expect("spawns")
            .last()
            .and_then(|spawn| spawn.plugin_root.clone())
            .expect("a spawn with a root")
    }
}

#[async_trait]
impl DeploymentRuntime for Probe {
    async fn spawn_and_health(&self, request: &DeploymentSpawn) -> SpawnHealth {
        let root = request.plugin_root.as_deref().expect("spawn root");
        assert!(
            root.join("plugin.toml").is_file(),
            "tree missing plugin.toml"
        );
        self.spawns.lock().expect("spawns").push(request.clone());
        if self.fail_health {
            SpawnHealth::HealthFailed {
                detail: "forced health failure".into(),
            }
        } else {
            SpawnHealth::Healthy
        }
    }

    async fn guest_root(&self, plugin_instance_id: &str) -> Option<PathBuf> {
        self.spawns
            .lock()
            .expect("spawns")
            .iter()
            .rev()
            .find(|spawn| spawn.plugin_instance_id == plugin_instance_id)
            .and_then(|spawn| spawn.plugin_root.clone())
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

/// `plugin.toml` for the fixture storefront: a `mode` enum setting and,
/// with `secrets`, a `token` secret setting behind the `[secrets]` binding.
fn fixture_manifest(id: &str, network_mode: &str, secrets: bool) -> String {
    let mut text = format!(
        "api_version = 3\nid = \"{id}\"\nversion = \"0.1.0\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
         entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"{network_mode}\"\n\n\
         [vars]\n\n[[settings]]\nkey = \"mode\"\nlabel = \"Mode\"\ntype = \"enum\"\n\
         values = [{{ id = \"device\", label = \"Device\" }}, {{ id = \"zip\", label = \"ZIP\" }}, \
         {{ id = \"web\", label = \"Web\" }}]\ndefault = \"web\"\n"
    );
    if secrets {
        text.push_str(
            "\n[secrets]\n\n[[settings]]\nkey = \"token\"\nlabel = \"Token\"\ntype = \"secret\"\n",
        );
    }
    text
}

/// Settings schema of the fixture manifest with secrets declared (a superset
/// of the secret-less variant, so documents validate either way).
fn fixture_schema() -> bookclerk_plugin_abi::SettingsSchema {
    bookclerk_plugin_manifest::parse(&fixture_manifest("fxdep", "deny", true))
        .expect("fixture manifest")
        .settings_schema()
}

fn write_archive(dir: &Path, id: &str, secrets: bool) -> PathBuf {
    let staging = dir.join(format!("{id}-stage"));
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(
        staging.join("plugin.toml"),
        fixture_manifest(id, "outbound", secrets),
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
    let archive = write_archive(&files, "fxdep", secrets);
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
    let scratch = files.join("cache");
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        &files,
        &scratch,
        None,
        &EventsConfig::default(),
    )
    .await
    .expect("bootstrap");
    let actor = ConfigActor::Bootstrap;
    let instance = create_plugin_instance(&store, &actor, &key, "")
        .await
        .expect("instance");
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        &fixture_schema(),
        &mode_body("device"),
        "import-fxdep",
    )
    .await
    .expect("import");
    let deployment = ensure_plugin_deployment(
        &store,
        &actor,
        &instance.id,
        &session.host.host_id,
        DESIRED_PRESENT,
    )
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
        DESIRED_PRESENT,
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
async fn an_operator_package_becomes_an_active_release_and_reaches_healthy() {
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
    let release = load_active_release(&world.store, &world.key)
        .await
        .unwrap()
        .expect("the package was published as the active release");
    assert_eq!(release.version, "0.1.0");
    assert!(
        release.grant_json.contains("fxdep"),
        "the approved grant rides on the release"
    );
    let artifact = crate::host_artifact(&release).unwrap();
    assert!(
        artifact.url.starts_with(RELEASE_BLOB_SCHEME),
        "a small archive is stored in the library: {}",
        artifact.url
    );
    let cache = crate::release_cache_path(&world.files, &artifact.payload_root_sha256);
    assert!(
        cache.join("plugin.toml").is_file(),
        "materialized by digest"
    );
    let spawns = probe.spawns.lock().expect("spawns").clone();
    assert_eq!(spawns.len(), 1);
    assert_eq!(spawns[0].plugin_root.as_deref(), Some(cache.as_path()));
    assert_eq!(
        spawns[0].release.as_ref().map(|r| r.version.as_str()),
        Some("0.1.0")
    );
    // Nothing host-local was written: no ledger row, no receipt, no
    // `plugins/<fs-id>` tree.
    let key = PluginKey::parse(&world.key).unwrap();
    assert!(InstallLedger::load(&world.files)
        .unwrap()
        .get(&key)
        .is_none());
    assert!(!world.files.join("plugins").join(key.fs_id()).exists());
    let obs = observation(&world).await;
    assert_eq!(obs.status, DeploymentStatus::Healthy);
    assert!(obs.detail.is_empty());
    assert_eq!(obs.applied_config_revision, Some(1));

    // Health is an observation, not a gate: a failing guest leaves the
    // release active and the digest cached, and nothing is rolled back.
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
    let obs = observation(&failed).await;
    assert_eq!(obs.status, DeploymentStatus::Error);
    assert!(
        obs.detail.contains("forced health failure"),
        "{}",
        obs.detail
    );
    let release = load_active_release(&failed.store, &failed.key)
        .await
        .unwrap()
        .expect("still active");
    let artifact = crate::host_artifact(&release).unwrap();
    assert!(crate::release_cache_path(&failed.files, &artifact.payload_root_sha256).is_dir());
    assert_eq!(
        load_deployment(&failed.store, &failed.deployment_id)
            .await
            .unwrap()
            .unwrap()
            .desired,
        DESIRED_PRESENT
    );
}

/// Publishes `version` of the fixture plugin as a release of `world.key`.
async fn publish_version(world: &World, version: &str, activate: crate::Activate) {
    let stage = world.files.join(format!("v{version}-stage"));
    std::fs::create_dir_all(&stage).unwrap();
    std::fs::write(
        stage.join("plugin.toml"),
        fixture_manifest("fxdep", "outbound", false).replace("0.1.0", version),
    )
    .unwrap();
    std::fs::write(stage.join("echo"), format!("#!/bin/sh\necho {version}\n")).unwrap();
    let archive = world.files.join(format!("fxdep-{version}.tar.gz"));
    let status = std::process::Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&stage)
        .args(["plugin.toml", "echo"])
        .status()
        .expect("tar");
    assert!(status.success());
    let grant = PluginGrantStore::load(&world.files)
        .unwrap()
        .get_by_plugin_key(&world.key)
        .cloned();
    crate::publish_package_release(
        &world.store,
        &ConfigActor::Operator { id: "cli".into() },
        &world.files,
        &world.host_id,
        crate::ReleaseRequest {
            plugin_key: world.key.clone(),
            package: package_manifest("fxdep"),
            archive: Some(archive),
            target: None,
            coordinate: String::new(),
            offline: true,
            publisher_verified: false,
            grant,
            activate,
        },
    )
    .await
    .expect("publish");
}

#[tokio::test]
async fn moving_the_release_pointer_respawns_from_the_new_digest() {
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    let probe = Probe::new(world.files.clone(), false);
    let pass = || async {
        reconcile_local_deployments(
            &world.store,
            &world.config,
            &world.host_id,
            &packages(&world),
            &probe,
        )
        .await
        .expect("reconcile");
    };
    pass().await;
    let first = probe.last_root();
    pass().await;
    assert_eq!(
        probe.spawns.lock().unwrap().len(),
        1,
        "an idle pass does not respawn"
    );

    publish_version(&world, "0.2.0", crate::Activate::Always).await;
    pass().await;
    let second = probe.last_root();
    assert_eq!(probe.spawns.lock().unwrap().len(), 2, "update respawns");
    assert_ne!(first, second, "a new version is a new digest");
    assert_eq!(
        probe
            .spawns
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .release
            .as_ref()
            .unwrap()
            .version,
        "0.2.0"
    );

    // Rollback is a pointer move; the old digest is still cached.
    activate_release(
        &world.store,
        &ConfigActor::Operator { id: "cli".into() },
        &world.key,
        "0.1.0",
        &world.host_id,
    )
    .await
    .expect("rollback");
    pass().await;
    assert_eq!(probe.spawns.lock().unwrap().len(), 3);
    assert_eq!(probe.last_root(), first);
    assert_eq!(observation(&world).await.status, DeploymentStatus::Healthy);
}

#[tokio::test]
async fn a_release_published_on_one_host_is_materialized_and_spawned_on_another() {
    let _guard = TEST_LOCK.lock().await;
    // Host A publishes the operator package as a release and runs it.
    let world = open_world(false).await;
    let probe_a = Probe::new(world.files.clone(), false);
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe_a,
    )
    .await
    .expect("host a");
    let release = load_active_release(&world.store, &world.key)
        .await
        .unwrap()
        .expect("published");
    let digest = crate::host_artifact(&release)
        .unwrap()
        .payload_root_sha256
        .clone();

    // Host B: its own files dir, its own connection to the same library,
    // no package and no grant file of its own.
    let files_b = tempfile::tempdir().unwrap();
    let files_b = files_b.keep();
    std::fs::copy(
        bookclerk_library::master_key_path(&world.files),
        bookclerk_library::master_key_path(&files_b),
    )
    .unwrap();
    std::fs::write(files_b.join("config.toml"), "").unwrap();
    let config_b =
        Config::load(Some(files_b.clone()), Some(files_b.join("config.toml"))).expect("config b");
    let store_b = file_store(&world.files.join("library.db")).await;
    let session_b = bookclerk_library::control_plane::bootstrap_control_plane(
        &store_b,
        &files_b,
        &files_b.join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .expect("bootstrap b");
    assert_ne!(session_b.host.host_id, world.host_id);
    let deployment_b = ensure_plugin_deployment(
        &store_b,
        &ConfigActor::Operator { id: "cli".into() },
        &world.instance_id,
        &session_b.host.host_id,
        DESIRED_PRESENT,
    )
    .await
    .expect("deployment on b");
    let probe_b = Probe::new(files_b.clone(), false);
    reconcile_local_deployments(
        &store_b,
        &config_b,
        &session_b.host.host_id,
        &AuthorizedLocalPackages::default(),
        &probe_b,
    )
    .await
    .expect("host b");
    let cache_b = crate::release_cache_path(&files_b, &digest);
    assert!(
        cache_b.join("plugin.toml").is_file(),
        "host b materialized the digest"
    );
    assert_eq!(probe_b.last_root(), cache_b);
    let spawned = probe_b.spawns.lock().unwrap().last().cloned().unwrap();
    assert_eq!(spawned.plugin_key, world.key);
    assert_eq!(
        config_mode(&spawned),
        "device",
        "host b reads the instance document"
    );
    let obs = load_observation(
        &store_b,
        &deployment_b.deployment_id,
        &session_b.host.host_id,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
    assert!(
        !files_b.join("plugin-grants.json").exists(),
        "host b trusts the release grant, not a host file"
    );
}

#[tokio::test]
async fn absent_retires_running_guest_and_present_brings_it_back() {
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
    .expect("present reconcile");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert!(probe.retired.lock().expect("retired").is_empty());

    let operator = ConfigActor::Operator {
        id: "operator".into(),
    };
    let current = load_deployment(&world.store, &world.deployment_id)
        .await
        .unwrap()
        .unwrap();
    let replaced = replace_plugin_deployment(
        &world.store,
        &operator,
        &world.deployment_id,
        current.revision,
        DESIRED_ABSENT,
        "disable-fxdep",
    )
    .await
    .expect("set absent");
    let DeploymentReplace::Applied(_) = replaced else {
        panic!("expected apply, got {replaced:?}");
    };
    let (absent, discover_calls) = with_discover_count(reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &AuthorizedLocalPackages::default(),
        &probe,
    ))
    .await;
    absent.expect("absent reconcile");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert_eq!(
        probe.retired.lock().expect("retired").as_slice(),
        &[world.instance_id.as_str().to_string()]
    );
    assert_eq!(
        discover_calls, 0,
        "an absent row never discovers or installs"
    );
    let obs = observation(&world).await;
    assert_eq!(obs.status, DeploymentStatus::Retired, "{}", obs.detail);

    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &AuthorizedLocalPackages::default(),
        &probe,
    )
    .await
    .expect("idle absent reconcile");
    assert_eq!(
        probe.retired.lock().expect("retired").len(),
        1,
        "a retired guest is not retired again"
    );

    let current = load_deployment(&world.store, &world.deployment_id)
        .await
        .unwrap()
        .unwrap();
    replace_plugin_deployment(
        &world.store,
        &operator,
        &world.deployment_id,
        current.revision,
        DESIRED_PRESENT,
        "enable-fxdep",
    )
    .await
    .expect("set present");
    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("present again");
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 2);
    assert_eq!(observation(&world).await.status, DeploymentStatus::Healthy);
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
        let archive = write_archive(&world.files, alias, false);
        let manifest = package_manifest(alias);
        let plugins = world.files.join("plugins");
        let coordinate = Installer::local_archive_coordinate(&archive, &manifest);
        let key = Installer::plugin_key_for(&coordinate, alias, &plugins)
            .expect("plugin key")
            .canonical()
            .to_string();
        let actor = ConfigActor::Bootstrap;
        let instance = create_plugin_instance(&world.store, &actor, &key, "")
            .await
            .expect("instance");
        import_instance_config_if_absent(
            &world.store,
            &actor,
            &instance.id,
            &fixture_schema(),
            &mode_body("device"),
            operation,
        )
        .await
        .expect("import");
        ensure_plugin_deployment(
            &world.store,
            &actor,
            &instance.id,
            &world.host_id,
            DESIRED_PRESENT,
        )
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
    assert!(
        load_active_release(&world.store, &world.key)
            .await
            .unwrap()
            .is_none(),
        "a host-local ledger hit publishes nothing"
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
        &fixture_schema(),
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
        &fixture_schema(),
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
        &fixture_schema(),
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
    let obs = observation(&dangling).await;
    assert_eq!(obs.status, DeploymentStatus::Error);
    // Observation detail is redacted; secret names may become `[REDACTED]`.
    assert!(
        obs.detail.contains("missing_secret")
            || obs.detail.contains("secret")
            || obs.detail.contains("REDACTED")
            || obs.detail.contains("redact"),
        "unexpected observation detail: {:?}",
        obs.detail
    );
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
async fn prepare_open_for_instance_resolves_each_document() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    let store = file_store(&files.join("library.db")).await;
    let scratch = files.join("cache");
    bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        &scratch,
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    let manifest =
        bookclerk_plugin_manifest::parse(&fixture_manifest("fixture", "deny", true)).unwrap();
    let root = files.join("plugin-src");
    std::fs::create_dir_all(&root).unwrap();
    let plugin = DiscoveredPlugin::for_test(manifest, root.clone(), root.join("echo"));
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(files).unwrap();
    let actor = ConfigActor::Bootstrap;
    let schema = plugin.manifest.settings_schema();
    let key = plugin.plugin_key().canonical();

    let first = create_plugin_instance(&store, &actor, key, "")
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &first.id,
        &schema,
        &mode_body("device"),
        "import-first",
    )
    .await
    .unwrap();
    let prepared = prepare_open_for_instance(&store, files, &plugin, &first.id)
        .await
        .unwrap();
    assert_eq!(prepared.granted_config["mode"], "device");
    assert_eq!(
        prepared.bindings.config.json_value().unwrap()["mode"],
        "device"
    );
    assert_eq!(prepared.config_revision, 1);

    let second = create_plugin_instance(&store, &actor, key, "second")
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &second.id,
        &schema,
        &PluginInstanceConfigV1::empty(),
        "import-second",
    )
    .await
    .unwrap();
    let prepared = prepare_open_for_instance(&store, files, &plugin, &second.id)
        .await
        .unwrap();
    assert_eq!(
        prepared.granted_config["mode"], "web",
        "an empty document resolves to the schema default"
    );

    let other_manifest =
        bookclerk_plugin_manifest::parse(&fixture_manifest("other", "deny", true)).unwrap();
    let other_root = files.join("other-src");
    std::fs::create_dir_all(&other_root).unwrap();
    let other =
        DiscoveredPlugin::for_test(other_manifest, other_root.clone(), other_root.join("echo"));
    let err = prepare_open_for_instance(&store, files, &other, &first.id)
        .await
        .expect_err("an instance of another plugin must not open");
    assert!(err.to_string().contains("belongs to"), "{err}");
}

#[test]
fn resolver_source_does_not_consult_family_settings() {
    let source = include_str!("instance_bindings.rs");
    let resolve = source
        .split("pub async fn prepare_open_for_instance")
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

async fn save_manifest_grant(files: &Path, plugin: &DiscoveredPlugin) {
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(files).unwrap();
}

#[tokio::test]
async fn live_two_instances_same_key_route_to_the_default() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_graphicaudio_tree(files);
    std::fs::write(files.join("config.toml"), "").unwrap();
    let mut config =
        Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
    config.plugins.isolation = Isolation::Off;
    let store = file_store(&files.join("library.db")).await;
    let scratch = files.join("cache");
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        &scratch,
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
    let first = create_plugin_instance(&store, &actor, key, "")
        .await
        .unwrap();
    let second = create_plugin_instance(&store, &actor, key, "")
        .await
        .unwrap();
    assert_ne!(first.id, second.id);
    import_instance_config_if_absent(
        &store,
        &actor,
        &first.id,
        &plugin.manifest.settings_schema(),
        &ga_settings("http://alpha.example"),
        "import-alpha",
    )
    .await
    .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &second.id,
        &plugin.manifest.settings_schema(),
        &ga_settings("http://beta.example"),
        "import-beta",
    )
    .await
    .unwrap();
    let deploy_a = ensure_plugin_deployment(
        &store,
        &actor,
        &first.id,
        &session.host.host_id,
        DESIRED_PRESENT,
    )
    .await
    .unwrap();
    let deploy_b = ensure_plugin_deployment(
        &store,
        &actor,
        &second.id,
        &session.host.host_id,
        DESIRED_PRESENT,
    )
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
    assert_eq!(config_a["access"], "device");
    assert_eq!(
        config_a["bitrate"], "hi",
        "schema defaults reach CONFIG beside the document's values"
    );
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
    let routed = sources
        .get(key)
        .expect("a bare plugin key routes to the default instance");
    assert_eq!(
        routed.plugin_instance_id(),
        Some(first.id.as_str()),
        "the oldest instance is the default until one is enrolled"
    );
    assert_eq!(
        sources
            .get("graphicaudio")
            .expect("alias routes")
            .plugin_instance_id(),
        Some(first.id.as_str())
    );
    let pinned = sources
        .route(key, Some(second.id.as_str()))
        .expect("a job pin selects the other instance");
    assert_eq!(pinned.plugin_instance_id(), Some(second.id.as_str()));
    assert!(
        sources.route(key, Some("not-an-instance")).is_none(),
        "an unknown pin does not fall back silently"
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
    let scratch = files.join("cache");
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        &scratch,
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
    let instance = create_plugin_instance(&store, &actor, plugin.plugin_key().canonical(), "")
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        &plugin.manifest.settings_schema(),
        &ga_settings("http://live.example"),
        "import-live",
    )
    .await
    .unwrap();
    let deployment = ensure_plugin_deployment(
        &store,
        &actor,
        &instance.id,
        &session.host.host_id,
        DESIRED_PRESENT,
    )
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
async fn broken_guest_health_is_an_error_observation_and_keeps_the_release() {
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
    let obs = observation(&world).await;
    assert_eq!(obs.status, DeploymentStatus::Error, "{}", obs.detail);
    assert!(
        load_active_release(&world.store, &world.key)
            .await
            .unwrap()
            .is_some(),
        "health never rolls a release back"
    );
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

#[tokio::test]
async fn a_host_local_tree_older_than_the_pinned_schema_is_retired_and_not_spawned() {
    use bookclerk_library::control_plane::{pin_plugin_schema, SchemaPin, SchemaPinChange};
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    preinstall_host_local(&world);
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
    assert_eq!(probe.spawns.lock().expect("spawns").len(), 1);
    assert_eq!(observation(&world).await.status, DeploymentStatus::Healthy);

    // Another host upgraded the plugin and re-pinned its schema at 0.9.0.
    let pinned = pin_plugin_schema(
        &world.store,
        &SchemaPin {
            plugin_key: &world.key,
            manifest_id: "fxdep",
            package_version: "0.9.0",
            schema: &fixture_schema(),
            host_id: "host-b",
        },
    )
    .await
    .expect("pin 0.9.0");
    assert!(matches!(
        pinned.change,
        SchemaPinChange::Pinned | SchemaPinChange::Upgraded { .. }
    ));

    reconcile_local_deployments(
        &world.store,
        &world.config,
        &world.host_id,
        &packages(&world),
        &probe,
    )
    .await
    .expect("second reconcile");
    assert_eq!(
        probe.spawns.lock().expect("spawns").len(),
        1,
        "the stale package is not respawned"
    );
    assert_eq!(
        probe.retired.lock().expect("retired").as_slice(),
        [world.instance_id.as_str().to_string()],
        "the running guest is retired"
    );
    let observed = observation(&world).await;
    assert_eq!(observed.status, DeploymentStatus::Error);
    assert!(
        observed
            .detail
            .contains("is older than 0.9.0 pinned by host host-b; not spawned"),
        "{}",
        observed.detail
    );
}

/// Installs the fixture as a host-local tree (ledger + receipt), the path
/// platform guests and development trees still take.
fn preinstall_host_local(world: &World) {
    let opts = InstallOptions {
        plugins_root: world.files.join("plugins"),
        offline: true,
        trust: TrustPolicy::allow_unverified_publisher(),
        skip_health: true,
        ..InstallOptions::default()
    };
    let outcome = Installer::install_local_archive(&world.archive, &world.manifest, &opts)
        .expect("preinstall");
    Installer::commit(&outcome).expect("commit");
}

#[tokio::test]
async fn an_operator_package_older_than_the_pin_is_not_activated() {
    use bookclerk_library::control_plane::{pin_plugin_schema, SchemaPin};
    let _guard = TEST_LOCK.lock().await;
    let world = open_world(false).await;
    pin_plugin_schema(
        &world.store,
        &SchemaPin {
            plugin_key: &world.key,
            manifest_id: "fxdep",
            package_version: "0.9.0",
            schema: &fixture_schema(),
            host_id: "host-b",
        },
    )
    .await
    .expect("pin 0.9.0");
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
    assert!(probe.spawns.lock().unwrap().is_empty());
    assert!(load_active_release(&world.store, &world.key)
        .await
        .unwrap()
        .is_none());
    let obs = observation(&world).await;
    assert_eq!(obs.status, DeploymentStatus::Error);
    assert!(
        obs.detail
            .contains("0.1.0 is older than 0.9.0 pinned by host host-b; not activated"),
        "{}",
        obs.detail
    );
}

#[tokio::test]
async fn a_verified_platform_guest_without_a_grant_file_persists_its_grant_and_spawns() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().to_path_buf();
    std::fs::write(files.join("config.toml"), "").unwrap();
    let config =
        Config::load(Some(files.clone()), Some(files.join("config.toml"))).expect("config");
    let store = file_store(&files.join("library.db")).await;
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        &files,
        &files.join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .expect("bootstrap");

    // A host-stamped `local` platform tree inside the installer envelope.
    let key = PluginKey::platform("bookclerk-plugin-destination-local", "local").unwrap();
    let root = files.join("plugins").join(key.fs_id());
    std::fs::create_dir_all(&root).unwrap();
    let text = "api_version = 3\nid = \"local\"\nversion = \"0.1.0\"\nruntime = \"native\"\n\
                command = \"./guest\"\nentrypoints = [\"storage\"]\n\n\
                [capabilities.network]\nmode = \"deny\"\n\n[vars]\n\n[work_fs]\n";
    std::fs::write(root.join("plugin.toml"), text).unwrap();
    std::fs::write(root.join("guest"), b"#!/bin/sh\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("guest"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let manifest = bookclerk_plugin_manifest::parse(text).unwrap();
    bookclerk_plugin_catalog::stamp_platform_receipt(
        &root,
        &files,
        "bookclerk-plugin-destination-local",
        &manifest,
        "0.1.0",
    )
    .expect("stamp");

    let actor = ConfigActor::Bootstrap;
    let instance = create_plugin_instance(&store, &actor, key.canonical(), "")
        .await
        .expect("instance");
    import_instance_config_if_absent(
        &store,
        &actor,
        &instance.id,
        &manifest.settings_schema(),
        &PluginInstanceConfigV1::empty(),
        "import-platform-local",
    )
    .await
    .expect("document");
    let deployment = ensure_plugin_deployment(
        &store,
        &actor,
        &instance.id,
        &session.host.host_id,
        DESIRED_PRESENT,
    )
    .await
    .expect("deployment");
    assert!(
        PluginGrantStore::load(&files)
            .unwrap()
            .get_by_plugin_key(key.canonical())
            .is_none(),
        "no grant file entry yet"
    );

    let probe = Probe::new(files.clone(), false);
    reconcile_local_deployments(
        &store,
        &config,
        &session.host.host_id,
        &AuthorizedLocalPackages::default(),
        &probe,
    )
    .await
    .expect("reconcile");
    let obs = load_observation(&store, &deployment.deployment_id, &session.host.host_id)
        .await
        .unwrap()
        .expect("observation");
    assert_eq!(obs.status, DeploymentStatus::Healthy, "{}", obs.detail);
    assert_eq!(probe.spawns.lock().unwrap().len(), 1);
    assert!(
        PluginGrantStore::load(&files)
            .unwrap()
            .get_by_plugin_key(key.canonical())
            .is_some(),
        "the platform grant is persisted before the first spawn"
    );
}
