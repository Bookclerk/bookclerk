//! First-party storefront PluginInstance cutover: code-default enroll, no
//! `[sources.*]` seed, empty transitional settings, and fail-closed spawn.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::Path;

use bookclerk_config::{Config, EventsConfig};
use bookclerk_library::control_plane::{
    bootstrap_control_plane, find_plugin_instance_by_key, list_present_deployments_for_host,
    load_instance_config, replace_instance_config, ConfigActor, InstanceConfigReplace,
    InstancePackagePolicy, PluginInstanceConfigV1, SettingValue,
};
use bookclerk_library::LibraryStore;
use bookclerk_plugin_catalog::{
    InstallLedger, InstallLedgerEntry, PluginKey, PluginProvenance, FIRST_PARTY_STOREFRONTS,
};

use crate::consent::{consent_request, PluginGrantStore};
use crate::discover::{settings_table_for, DiscoveredPlugin};
use crate::{
    enroll_audible_instance, enroll_storefront_instances, is_first_party_storefront,
    ExternalSource, SessionServices,
};

async fn file_store(path: &Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("open sqlite");
    bookclerk_library::apply_host_schema(&db)
        .await
        .expect("schema");
    LibraryStore::from_connection(db)
}

fn platform_key(manifest_id: &str) -> PluginKey {
    let artifact = FIRST_PARTY_STOREFRONTS
        .iter()
        .find(|artifact| artifact.manifest_id == manifest_id)
        .expect("storefront artifact");
    PluginKey::platform(artifact.package_name, manifest_id).expect("platform key")
}

fn write_ledger(files: &Path) {
    let mut ledger = InstallLedger::default();
    for artifact in FIRST_PARTY_STOREFRONTS {
        ledger.upsert(InstallLedgerEntry {
            plugin_key: platform_key(artifact.manifest_id).canonical().to_string(),
            package_name: artifact.package_name.to_string(),
            manifest_id: artifact.manifest_id.to_string(),
            manifest_sha256: "00".repeat(32),
            payload_root_sha256: "11".repeat(32),
            provenance: PluginProvenance::PlatformBundled,
            recorded_at: chrono::Utc::now(),
        });
    }
    ledger.store(files).expect("store ledger");
}

struct World {
    _dir: tempfile::TempDir,
    files: std::path::PathBuf,
    store: LibraryStore,
    host_id: String,
}

async fn world(config_toml: &str) -> (World, Config) {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path().to_path_buf();
    std::fs::write(files.join("config.toml"), config_toml).unwrap();
    write_ledger(&files);
    let config =
        Config::load(Some(files.clone()), Some(files.join("config.toml"))).expect("config");
    let store = file_store(&files.join("library.db")).await;
    let session = bootstrap_control_plane(
        &store,
        &files,
        &files.join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .expect("bootstrap");
    (
        World {
            _dir: dir,
            files,
            store,
            host_id: session.host.host_id,
        },
        config,
    )
}

fn reload(world: &World) -> Config {
    Config::load(
        Some(world.files.clone()),
        Some(world.files.join("config.toml")),
    )
    .expect("reload config")
}

async fn document_for(world: &World, manifest_id: &str) -> PluginInstanceConfigV1 {
    let key = platform_key(manifest_id);
    let instance = find_plugin_instance_by_key(&world.store, key.canonical())
        .await
        .expect("find instance")
        .expect("instance exists");
    load_instance_config(&world.store, &instance.id)
        .await
        .expect("document")
        .body
}

fn string_setting(body: &PluginInstanceConfigV1, key: &str) -> Option<String> {
    match body.settings.get(key) {
        Some(SettingValue::String(text)) => Some(text.clone()),
        _ => None,
    }
}

const TOML_KNOBS: &str = "\
[sources.audible]\nbitrate = \"normal\"\n\
[sources.libro]\ncontainer = \"zip\"\nbase_url = \"http://127.0.0.1:9\"\n\
[sources.chirp]\ngraphql_url = \"http://127.0.0.1:9/graphql\"\n\
[sources.graphicaudio]\naccess = \"zip\"\nbase_url = \"http://127.0.0.1:9\"\n";

#[tokio::test]
async fn enroll_seeds_code_defaults_and_ignores_sources_toml() {
    let (world, config) = world(TOML_KNOBS).await;
    enroll_storefront_instances(&world.store, &config, &world.host_id)
        .await
        .expect("enroll");

    let audible = document_for(&world, "audible").await;
    assert_eq!(string_setting(&audible, "bitrate").as_deref(), Some("high"));
    assert_eq!(audible.settings.len(), 1);
    let libro = document_for(&world, "libro").await;
    assert_eq!(string_setting(&libro, "container").as_deref(), Some("m4b"));
    assert_eq!(libro.settings.len(), 1);
    let chirp = document_for(&world, "chirp").await;
    assert!(chirp.settings.is_empty());
    let ga = document_for(&world, "graphicaudio").await;
    assert_eq!(string_setting(&ga, "access").as_deref(), Some("web"));
    assert_eq!(ga.settings.len(), 1);

    let deployments = list_present_deployments_for_host(&world.store, &world.host_id)
        .await
        .expect("deployments");
    assert_eq!(deployments.len(), 4);
}

#[tokio::test]
async fn toml_mutation_never_changes_the_document_or_enrollment() {
    let (world, config) = world("").await;
    enroll_storefront_instances(&world.store, &config, &world.host_id)
        .await
        .expect("enroll");
    let before = document_for(&world, "audible").await;

    std::fs::write(world.files.join("config.toml"), TOML_KNOBS).unwrap();
    let mutated = reload(&world);
    enroll_storefront_instances(&world.store, &mutated, &world.host_id)
        .await
        .expect("re-enroll");
    assert_eq!(document_for(&world, "audible").await, before);
    assert_eq!(
        string_setting(&document_for(&world, "libro").await, "container").as_deref(),
        Some("m4b")
    );
    assert_eq!(
        string_setting(&document_for(&world, "graphicaudio").await, "access").as_deref(),
        Some("web")
    );
}

#[tokio::test]
async fn operator_put_survives_a_later_enroll() {
    let (world, config) = world("").await;
    enroll_audible_instance(&world.store, &config, &world.host_id)
        .await
        .expect("enroll");
    let key = platform_key("audible");
    let instance = find_plugin_instance_by_key(&world.store, key.canonical())
        .await
        .unwrap()
        .unwrap();
    let current = load_instance_config(&world.store, &instance.id)
        .await
        .unwrap();
    let mut body = current.body.clone();
    body.settings
        .insert("bitrate".into(), SettingValue::String("normal".into()));
    let outcome = replace_instance_config(
        &world.store,
        &ConfigActor::Operator {
            id: "operator".into(),
        },
        &instance.id,
        InstancePackagePolicy::StorefrontAudible,
        current.revision,
        &body,
        "put-audible-test",
    )
    .await
    .expect("replace");
    assert!(matches!(outcome, InstanceConfigReplace::Applied(_)));

    enroll_audible_instance(&world.store, &config, &world.host_id)
        .await
        .expect("re-enroll");
    assert_eq!(
        string_setting(&document_for(&world, "audible").await, "bitrate").as_deref(),
        Some("normal")
    );
}

#[tokio::test]
async fn disabled_storefront_is_not_enrolled() {
    let (world, config) = world("[sources.audible]\nenabled = false\n").await;
    enroll_storefront_instances(&world.store, &config, &world.host_id)
        .await
        .expect("enroll");
    let key = platform_key("audible");
    assert!(find_plugin_instance_by_key(&world.store, key.canonical())
        .await
        .unwrap()
        .is_none());
    let libro = platform_key("libro");
    assert!(find_plugin_instance_by_key(&world.store, libro.canonical())
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn missing_package_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    std::fs::write(files.join("config.toml"), "").unwrap();
    let config = Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
    let store = file_store(&files.join("library.db")).await;
    let session = bootstrap_control_plane(
        &store,
        files,
        &files.join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    enroll_storefront_instances(&store, &config, &session.host.host_id)
        .await
        .expect("enroll without packages");
    assert!(
        list_present_deployments_for_host(&store, &session.host.host_id)
            .await
            .unwrap()
            .is_empty()
    );
}

fn first_party_plugin(root: &Path, manifest_id: &str) -> DiscoveredPlugin {
    let manifest = bookclerk_plugin_manifest::parse(&format!(
        "api_version = 3\nid = \"{manifest_id}\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
         entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"deny\"\n\n[vars]\n"
    ))
    .unwrap();
    let dir = root.join(format!("{manifest_id}-src"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut plugin = DiscoveredPlugin::for_test(manifest, dir.clone(), dir.join("echo"));
    plugin.identity.plugin_key = platform_key(manifest_id);
    plugin.identity.provenance = PluginProvenance::PlatformBundled;
    plugin
}

#[test]
fn settings_table_is_empty_for_first_party_storefronts_only() {
    let tmp = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    for id in ["audible", "libro", "chirp", "graphicaudio"] {
        config.sources.set_string(id, "bitrate", "normal");
        config.sources.set_string(id, "container", "zip");
        config.sources.set_string(id, "access", "zip");
    }
    for id in ["audible", "libro", "chirp", "graphicaudio"] {
        let plugin = first_party_plugin(tmp.path(), id);
        assert!(is_first_party_storefront(&plugin), "{id}");
        let table = settings_table_for(&config, &plugin, plugin.manifest.primary_family());
        assert!(table.is_empty(), "{id} must not read [sources.{id}]");

        let mut impostor = first_party_plugin(tmp.path(), id);
        impostor.identity.provenance = PluginProvenance::LocalDevelopment;
        assert!(!is_first_party_storefront(&impostor), "{id} impostor");
        let table = settings_table_for(&config, &impostor, impostor.manifest.primary_family());
        assert!(
            !table.is_empty(),
            "{id} impostor keeps the transitional table"
        );
    }
}

#[tokio::test]
async fn first_party_spawn_fails_closed_without_instance_config() {
    let (world, mut config) = world("[sources.audible]\nbitrate = \"normal\"\n").await;
    config.plugins.isolation = bookclerk_config::Isolation::Off;
    let plugin = first_party_plugin(&world.files, "audible");
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(&world.files).unwrap();

    let services = SessionServices::with_event_outbox(world.store.clone());
    let err = ExternalSource::spawn_with(&plugin, &config, services)
        .await
        .err()
        .expect("no instance document must fail closed");
    assert!(
        err.to_string().contains("requires PluginInstance CONFIG"),
        "{err}"
    );

    let err = ExternalSource::spawn(&plugin, &config)
        .await
        .err()
        .expect("no store must fail closed");
    assert!(
        err.to_string().contains("requires PluginInstance CONFIG"),
        "{err}"
    );
}
