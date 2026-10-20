//! Generic enrollment: idempotent across restarts, host-scoped deployments,
//! and the database-adapter bootstrap exception.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::Path;

use bookclerk_config::Config;
use bookclerk_library::control_plane::{
    find_plugin_instance_by_key, list_plugin_schemas, load_deployment_for_node,
    load_instance_config, DesiredState,
};
use bookclerk_library::LibraryStore;

use crate::consent::{consent_request, PluginGrantStore};
use crate::discover_plugins;
use crate::enrollment::{enroll_local_plugins, enroll_one, is_bootstrap_only};

use crate::PROCESS_DEK_TEST_LOCK as TEST_LOCK;

fn storefront_manifest(id: &str) -> String {
    format!(
        "api_version = 3\nid = \"{id}\"\nversion = \"0.1.0\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
         entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"deny\"\n\n\
         [vars]\n\n[[settings]]\nkey = \"mode\"\nlabel = \"Mode\"\ntype = \"enum\"\n\
         values = [{{ id = \"zip\", label = \"ZIP\" }}, {{ id = \"web\", label = \"Web\" }}]\n\
         default = \"web\"\n"
    )
}

fn database_manifest(id: &str) -> String {
    format!(
        "api_version = 3\nid = \"{id}\"\nversion = \"0.1.0\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
         entrypoints = [\"databaseAdapter\"]\n\n[capabilities.network]\nmode = \"deny\"\n\n[vars]\n"
    )
}

fn stage_tree(files: &Path, id: &str, manifest: &str) {
    let install = files.join("plugins").join(id);
    std::fs::create_dir_all(&install).unwrap();
    std::fs::write(install.join("plugin.toml"), manifest).unwrap();
    let echo = install.join("echo");
    std::fs::write(&echo, b"#!/bin/sh\necho ok\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&echo, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

async fn file_store(path: &Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("open sqlite file");
    bookclerk_library::apply_host_schema(&db)
        .await
        .expect("schema");
    LibraryStore::from_connection(db)
}

async fn open_host(files: &Path) -> (Config, LibraryStore, String) {
    if !files.join("config.toml").is_file() {
        std::fs::write(files.join("config.toml"), "").unwrap();
    }
    let config = Config::load(Some(files.to_path_buf()), Some(files.join("config.toml"))).unwrap();
    let store = file_store(&files.join("library.db")).await;
    let session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        files,
        &files.join("cache"),
        None,
        &bookclerk_config::Config::default(),
    )
    .await
    .unwrap();
    (config, store, session.node.node_id)
}

#[tokio::test]
async fn enrollment_is_idempotent_across_restarts() {
    let _guard = crate::PROCESS_DEK_TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_tree(files, "fxenroll", &storefront_manifest("fxenroll"));
    let (config, store, node_id) = open_host(files).await;

    let first = enroll_local_plugins(&store, &config, &node_id)
        .await
        .unwrap();
    assert_eq!(first.len(), 1, "one storefront enrolled");
    let one = &first[0];
    assert_eq!(one.plugin.manifest.id, "fxenroll");
    assert_eq!(one.deployment.node_id, node_id);
    assert_eq!(
        one.deployment.desired,
        DesiredState::Absent,
        "no grant yet: a third-party plugin enrols absent"
    );
    let seeded = load_instance_config(&store, &one.instance.id)
        .await
        .unwrap();
    assert!(seeded.body.settings.is_empty(), "defaults are not stored");
    assert_eq!(one.schema.schema.settings.len(), 1);
    assert_eq!(one.schema.schema.settings[0].key, "mode");

    // A restart (new process, same files directory) re-enrols: same ids,
    // same revisions, still one pinned schema row.
    drop(store);
    let (config, store, host_again) = open_host(files).await;
    assert_eq!(host_again, node_id);
    let second = enroll_local_plugins(&store, &config, &node_id)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    let two = &second[0];
    assert_eq!(two.instance.id, one.instance.id);
    assert_eq!(two.deployment.deployment_id, one.deployment.deployment_id);
    assert_eq!(two.deployment.revision, one.deployment.revision);
    assert_eq!(two.schema.schema_sha256, one.schema.schema_sha256);
    let pinned = list_plugin_schemas(&store).await.unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].plugin_key, one.plugin.plugin_key().canonical());
    let doc = load_instance_config(&store, &one.instance.id)
        .await
        .unwrap();
    assert_eq!(doc.revision, seeded.revision);

    // A second host joining the same database (same package, so the same
    // PluginKey) gets its own deployment row and does not touch the first
    // host's.
    let other_files = tempfile::tempdir().unwrap();
    std::fs::copy(
        bookclerk_library::master_key_path(files),
        bookclerk_library::master_key_path(other_files.path()),
    )
    .unwrap();
    let other_session = bookclerk_library::control_plane::bootstrap_control_plane(
        &store,
        other_files.path(),
        &other_files.path().join("cache"),
        None,
        &bookclerk_config::Config::default(),
    )
    .await
    .unwrap();
    let other_node = other_session.node.node_id;
    assert_ne!(other_node, node_id);
    let on_other = enroll_one(
        &store,
        &other_node,
        &one.plugin,
        &PluginGrantStore::default(),
    )
    .await
    .unwrap();
    assert_eq!(
        on_other.instance.id, one.instance.id,
        "same default instance"
    );
    assert_ne!(
        on_other.deployment.deployment_id,
        one.deployment.deployment_id
    );
    assert_eq!(on_other.deployment.node_id, other_node);
    let first_host_row = load_deployment_for_node(&store, &one.instance.id, &node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_host_row.revision, one.deployment.revision);
    assert_eq!(list_plugin_schemas(&store).await.unwrap().len(), 1);
}

#[tokio::test]
async fn granted_plugins_enroll_present_and_database_adapters_are_not_enrolled() {
    let _guard = crate::PROCESS_DEK_TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_tree(files, "fxgranted", &storefront_manifest("fxgranted"));
    stage_tree(files, "fxdbonly", &database_manifest("fxdbonly"));
    let (config, store, node_id) = open_host(files).await;

    let discovered = discover_plugins(&config).unwrap();
    let granted = discovered
        .iter()
        .find(|plugin| plugin.manifest.id == "fxgranted")
        .expect("storefront discovered");
    let db_only = discovered
        .iter()
        .find(|plugin| plugin.manifest.id == "fxdbonly")
        .expect("database adapter discovered");
    assert!(!is_bootstrap_only(granted));
    assert!(is_bootstrap_only(db_only));
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&granted.manifest, granted.plugin_key()));
    grants.save(files).unwrap();

    let enrolled = enroll_local_plugins(&store, &config, &node_id)
        .await
        .unwrap();
    assert_eq!(enrolled.len(), 1, "only the storefront enrols");
    assert_eq!(enrolled[0].plugin.manifest.id, "fxgranted");
    assert_eq!(
        enrolled[0].deployment.desired,
        DesiredState::Present,
        "a covering grant enrols the deployment present"
    );

    let db_key = db_only.plugin_key().canonical();
    assert!(
        find_plugin_instance_by_key(&store, db_key)
            .await
            .unwrap()
            .is_none(),
        "database adapters stay bootstrap: no instance"
    );
    assert!(
        list_plugin_schemas(&store)
            .await
            .unwrap()
            .iter()
            .all(|row| row.plugin_key != db_key),
        "database adapters pin no schema"
    );
}

#[tokio::test]
async fn a_scheduler_placed_instance_enrolls_unplaced_even_with_a_grant() {
    use bookclerk_library::control_plane::{
        ensure_plugin_instance, put_plugin_placement, ConfigActor, PlacementSpec,
    };

    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_tree(files, "fxplaced", &storefront_manifest("fxplaced"));
    let (config, store, node_id) = open_host(files).await;
    let discovered = discover_plugins(&config).unwrap();
    let plugin = discovered
        .iter()
        .find(|plugin| plugin.manifest.id == "fxplaced")
        .expect("discovered");
    let mut grants = PluginGrantStore::default();
    grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
    grants.save(files).unwrap();
    let instance = ensure_plugin_instance(
        &store,
        &ConfigActor::Bootstrap,
        plugin.plugin_key().canonical(),
    )
    .await
    .unwrap();
    put_plugin_placement(
        &store,
        &ConfigActor::Operator { id: "cli".into() },
        &instance.id,
        &PlacementSpec {
            replicas: bookclerk_library::control_plane::Replicas::Count(1),
            constraints: vec!["nas".into()],
        },
        None,
    )
    .await
    .unwrap();

    let enrolled = enroll_local_plugins(&store, &config, &node_id)
        .await
        .unwrap();
    assert_eq!(enrolled.len(), 1);
    assert_eq!(enrolled[0].instance.id, instance.id);
    assert_eq!(
        enrolled[0].deployment.desired,
        DesiredState::Unplaced,
        "the scheduler, not enrollment, deploys a placed instance"
    );
    assert_eq!(
        load_deployment_for_node(&store, &instance.id, &node_id)
            .await
            .unwrap()
            .unwrap()
            .desired,
        DesiredState::Unplaced
    );
}

#[test]
fn library_host_target_matches_the_catalog_target() {
    assert_eq!(
        bookclerk_library::control_plane::node_target(),
        bookclerk_plugin_catalog::host_bookclerk_target(),
        "hosts.target and release artifact targets must use one vocabulary"
    );
}

/// Storefront manifest at `version` whose `[[settings]]` are `keys`
/// (plain strings).
fn versioned_manifest(id: &str, version: &str, keys: &[&str]) -> String {
    let mut text = format!(
        "api_version = 3\nid = \"{id}\"\nversion = \"{version}\"\nruntime = \"native\"\n\
         command = \"./echo\"\nentrypoints = [\"storefront\"]\n\n[capabilities.network]\n\
         mode = \"deny\"\n\n[vars]\n"
    );
    for key in keys {
        text.push_str(&format!(
            "\n[[settings]]\nkey = \"{key}\"\nlabel = \"{key}\"\ntype = \"string\"\n"
        ));
    }
    text
}

#[tokio::test]
async fn a_newer_package_upgrades_the_pin_and_an_older_one_is_not_enrolled() {
    use bookclerk_library::control_plane::{
        load_observation, replace_instance_config, ConfigActor, DeploymentStatus,
        InstanceConfigReplace, PluginInstanceConfigV1, SettingValue,
    };
    let _guard = crate::PROCESS_DEK_TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_tree(
        files,
        "fxstale",
        &versioned_manifest("fxstale", "0.1.0", &["region"]),
    );
    let (config, store, node_id) = open_host(files).await;

    let first = enroll_local_plugins(&store, &config, &node_id)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    let one = &first[0];
    assert_eq!(one.schema.package_version, "0.1.0");
    let mut body = PluginInstanceConfigV1::empty();
    body.settings
        .insert("region".into(), SettingValue::String("eu".into()));
    let seeded = load_instance_config(&store, &one.instance.id)
        .await
        .unwrap();
    let written = replace_instance_config(
        &store,
        &ConfigActor::Operator { id: "test".into() },
        &one.instance.id,
        &one.schema.schema,
        seeded.revision,
        &body,
        "write-region",
    )
    .await
    .unwrap();
    assert!(matches!(written, InstanceConfigReplace::Applied(_)));

    // 0.2.0 renames the setting: the pin upgrades and the document loses
    // the key the new schema no longer declares.
    let manifest_path = files.join("plugins").join("fxstale").join("plugin.toml");
    std::fs::write(
        &manifest_path,
        versioned_manifest("fxstale", "0.2.0", &["locale"]),
    )
    .unwrap();
    let second = enroll_local_plugins(&store, &config, &node_id)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].schema.package_version, "0.2.0");
    assert_eq!(second[0].schema.schema.settings[0].key, "locale");
    let doc = load_instance_config(&store, &one.instance.id)
        .await
        .unwrap();
    assert_eq!(doc.revision, seeded.revision + 2);
    assert!(doc.body.settings.is_empty(), "`region` was pruned");

    // Rolling the install back to 0.1.0 (or another node still on it) does
    // not enrol: the pin stays at 0.2.0 and the deployment shows why.
    std::fs::write(
        &manifest_path,
        versioned_manifest("fxstale", "0.1.0", &["region"]),
    )
    .unwrap();
    let third = enroll_local_plugins(&store, &config, &node_id)
        .await
        .unwrap();
    assert!(third.is_empty(), "stale package is skipped");
    let pinned = list_plugin_schemas(&store).await.unwrap();
    assert_eq!(pinned.len(), 1);
    assert_eq!(pinned[0].package_version, "0.2.0");
    let observed = load_observation(&store, &one.deployment.deployment_id, &node_id)
        .await
        .unwrap()
        .expect("error observation on this node's deployment");
    assert_eq!(observed.status, DeploymentStatus::Error);
    assert!(
        observed
            .detail
            .contains("installed version 0.1.0 is older than 0.2.0 pinned by node"),
        "{}",
        observed.detail
    );

    // A manifest without a version is not enrolled either.
    std::fs::write(
        &manifest_path,
        versioned_manifest("fxstale", "0.2.0", &["locale"]).replace("version = \"0.2.0\"\n", ""),
    )
    .unwrap();
    let discovered = discover_plugins(&config).unwrap();
    let plugin = discovered
        .iter()
        .find(|plugin| plugin.manifest.id == "fxstale")
        .unwrap();
    let err = enroll_one(&store, &node_id, plugin, &PluginGrantStore::default())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`version` is required"), "{err}");
}
