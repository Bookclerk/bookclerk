//! Generic enrollment: idempotent across restarts, host-scoped deployments,
//! and the database-adapter bootstrap exception.

#![allow(clippy::missing_docs_in_private_items)]

use std::path::Path;

use bookclerk_config::{Config, EventsConfig};
use bookclerk_library::control_plane::{
    find_plugin_instance_by_key, list_plugin_schemas, load_deployment_for_host,
    load_instance_config, DESIRED_ABSENT, DESIRED_PRESENT,
};
use bookclerk_library::LibraryStore;

use crate::consent::{consent_request, PluginGrantStore};
use crate::discover_plugins;
use crate::enrollment::{enroll_local_plugins, enroll_one, is_bootstrap_only};

static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn storefront_manifest(id: &str) -> String {
    format!(
        "api_version = 3\nid = \"{id}\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
         entrypoints = [\"storefront\"]\n\n[capabilities.network]\nmode = \"deny\"\n\n\
         [vars]\n\n[[settings]]\nkey = \"mode\"\nlabel = \"Mode\"\ntype = \"enum\"\n\
         values = [{{ id = \"zip\", label = \"ZIP\" }}, {{ id = \"web\", label = \"Web\" }}]\n\
         default = \"web\"\n"
    )
}

fn database_manifest(id: &str) -> String {
    format!(
        "api_version = 3\nid = \"{id}\"\nruntime = \"native\"\ncommand = \"./echo\"\n\
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
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    (config, store, session.host.host_id)
}

#[tokio::test]
async fn enrollment_is_idempotent_across_restarts() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_tree(files, "fxenroll", &storefront_manifest("fxenroll"));
    let (config, store, host_id) = open_host(files).await;

    let first = enroll_local_plugins(&store, &config, &host_id)
        .await
        .unwrap();
    assert_eq!(first.len(), 1, "one storefront enrolled");
    let one = &first[0];
    assert_eq!(one.plugin.manifest.id, "fxenroll");
    assert_eq!(one.deployment.host_id, host_id);
    assert_eq!(
        one.deployment.desired, DESIRED_ABSENT,
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
    assert_eq!(host_again, host_id);
    let second = enroll_local_plugins(&store, &config, &host_id)
        .await
        .unwrap();
    assert_eq!(second.len(), 1);
    let two = &second[0];
    assert_eq!(two.instance.id, one.instance.id);
    assert_eq!(two.deployment.deployment_id, one.deployment.deployment_id);
    assert_eq!(two.deployment.revision, one.deployment.revision);
    assert_eq!(two.schema.manifest_sha256, one.schema.manifest_sha256);
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
        &EventsConfig::default(),
    )
    .await
    .unwrap();
    let other_host = other_session.host.host_id;
    assert_ne!(other_host, host_id);
    let on_other = enroll_one(
        &store,
        &other_host,
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
    assert_eq!(on_other.deployment.host_id, other_host);
    let first_host_row = load_deployment_for_host(&store, &one.instance.id, &host_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first_host_row.revision, one.deployment.revision);
    assert_eq!(list_plugin_schemas(&store).await.unwrap().len(), 1);
}

#[tokio::test]
async fn granted_plugins_enroll_present_and_database_adapters_are_not_enrolled() {
    let _guard = TEST_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let files = dir.path();
    stage_tree(files, "fxgranted", &storefront_manifest("fxgranted"));
    stage_tree(files, "fxdbonly", &database_manifest("fxdbonly"));
    let (config, store, host_id) = open_host(files).await;

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

    let enrolled = enroll_local_plugins(&store, &config, &host_id)
        .await
        .unwrap();
    assert_eq!(enrolled.len(), 1, "only the storefront enrols");
    assert_eq!(enrolled[0].plugin.manifest.id, "fxgranted");
    assert_eq!(
        enrolled[0].deployment.desired, DESIRED_PRESENT,
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
