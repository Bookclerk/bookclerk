//! Current-host deployment reconcile: installer transaction, ledger hit,
//! revision apply, foreign host, secrets, and the transitional settings path.

#![allow(clippy::missing_docs_in_private_items)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use async_trait::async_trait;
use bookclerk_config::{Config, EventsConfig};
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
use bookclerk_plugin_sdk::ExtensibleConfig;

use crate::consent::{consent_request, PluginGrant, PluginGrantStore};
use crate::discover::{settings_table_for, DiscoveredPlugin};
use crate::instance_bindings::prepare_open_bindings;
use crate::{
    reconcile_local_deployments, DeploymentRuntime, DeploymentSpawn, LocalPackage, SpawnHealth,
};

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Probe {
    files: PathBuf,
    fail_health: bool,
    health_calls: AtomicUsize,
    spawns: Mutex<Vec<DeploymentSpawn>>,
}

impl Probe {
    fn new(files: PathBuf, fail_health: bool) -> Self {
        Self {
            files,
            fail_health,
            health_calls: AtomicUsize::new(0),
            spawns: Mutex::new(Vec::new()),
        }
    }

    fn saw_lock(&self) -> bool {
        self.files.join(PLUGIN_MUTATION_LOCK_FILE).is_file()
    }
}

#[async_trait]
impl DeploymentRuntime for Probe {
    async fn health_before_commit(&self, plugin_root: &Path) -> Result<(), String> {
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
             entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"deny\"\n"
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

fn packages(world: &World) -> HashMap<String, LocalPackage> {
    let mut packages = HashMap::new();
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
        &HashMap::new(),
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
    let spawns = probe.spawns.lock().unwrap();
    assert_eq!(spawns.len(), 2);
    assert_eq!(config_mode(&spawns[1]), "zip");
    drop(spawns);
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
    let spawns = probe.spawns.lock().unwrap();
    assert_eq!(spawns.len(), 1);
    assert_eq!(secret_value(&spawns[0]), secret);
    drop(spawns);

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
    let prepared = prepare_open_bindings(Some(&store), files, &plugin, transitional)
        .await
        .unwrap();
    assert!(prepared.from_instance);
    assert_eq!(prepared.granted_config["mode"], "device");
    let bindings = prepared.bindings.config.json_value().unwrap();
    assert_eq!(bindings["mode"], "device");
    let _ = ExtensibleConfig::default();
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
