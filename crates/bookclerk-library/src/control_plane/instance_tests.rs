//! Plugin instance identity, configuration, and deployment tests.

use bookclerk_config::EventsConfig;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
use uuid::Uuid;

use super::*;
use crate::entities::{configuration_audit, configuration_documents};
use crate::master_key::master_key_test_lock_async;
use crate::store::LibraryStore;

async fn file_store(path: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    LibraryStore::from_connection(db)
}

fn operator() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

fn mode_device() -> PluginInstanceConfigV1 {
    let mut settings = std::collections::BTreeMap::new();
    settings.insert("mode".into(), SettingValue::String("device".into()));
    PluginInstanceConfigV1 {
        settings,
        secret_refs: Vec::new(),
    }
}

async fn bootstrap(store: &LibraryStore, files: &std::path::Path) -> ControlPlaneSession {
    bootstrap_control_plane(store, files, None, &EventsConfig::default())
        .await
        .expect("bootstrap")
}

#[tokio::test]
async fn plugin_instance_ids_are_stable_uuids_independent_of_key() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let session = bootstrap(&store, dir.path()).await;
    let key = "platform:bookclerk/fixture";
    let alias = "fixture";
    let first = create_plugin_instance(&store, &operator(), key)
        .await
        .expect("first");
    let second = create_plugin_instance(&store, &operator(), key)
        .await
        .expect("second");
    assert_ne!(first.id, second.id);
    for id in [&first.id, &second.id] {
        assert!(Uuid::parse_str(id.as_str()).is_ok());
        assert_ne!(id.as_str(), key);
        assert_ne!(id.as_str(), alias);
        assert_ne!(id.as_str(), session.host.host_id);
        assert!(
            !id.as_str().contains(':'),
            "a plugin key has a scheme; the instance id does not"
        );
    }
    let reloaded = load_plugin_instance(&store, &first.id)
        .await
        .expect("reload")
        .expect("row");
    assert_eq!(reloaded.id, first.id);
    assert_eq!(reloaded.plugin_key, key);

    let ensured = ensure_plugin_instance(&store, &ConfigActor::Bootstrap, key)
        .await
        .expect("ensure");
    let again = ensure_plugin_instance(&store, &ConfigActor::Bootstrap, key)
        .await
        .expect("ensure again");
    assert_eq!(ensured.id, again.id);
    assert_eq!(ensured.id, first.id);
}

#[tokio::test]
async fn instance_config_cas_keeps_secret_values_out_of_the_document() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let _session = bootstrap(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture")
        .await
        .expect("instance");
    let secret = "s3cret-value-should-not-leak";
    seal_instance_secret(
        &store,
        &instance.id,
        &instance.plugin_key,
        "device_token",
        secret,
    )
    .await
    .expect("seal");
    let mut body = PluginInstanceConfigV1::empty();
    body.secret_refs.push(InstanceSecretRefV1 {
        key: "token".into(),
        name: "device_token".into(),
    });
    body.settings
        .insert("mode".into(), SettingValue::String("device".into()));
    let imported = import_instance_config_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        InstancePackagePolicy::Generic,
        &body,
        "import-instance-config",
    )
    .await
    .expect("import");
    let stored = configuration_documents::Entity::find_by_id((
        PLUGIN_INSTANCE_SCOPE_TYPE.to_string(),
        instance.id.as_str().to_string(),
        PLUGIN_INSTANCE_CONFIG_NAMESPACE.to_string(),
    ))
    .one(store.db())
    .await
    .unwrap()
    .unwrap();
    assert!(stored.document_json.contains("device_token"));
    assert!(!stored.document_json.contains(secret));
    let audits = configuration_audit::Entity::find()
        .all(store.db())
        .await
        .unwrap();
    let audit_dump = format!("{audits:?}");
    assert!(!audit_dump.contains(secret));
    assert!(audit_dump.contains("config") || !audits.is_empty());

    let before = audit_count(&store, PLUGIN_INSTANCE_CONFIG_NAMESPACE)
        .await
        .unwrap();
    let stale = replace_instance_config(
        &store,
        &operator(),
        &instance.id,
        InstancePackagePolicy::Generic,
        imported.revision + 5,
        &body,
        "stale-instance-config",
    )
    .await
    .expect("stale");
    assert_eq!(
        stale,
        InstanceConfigReplace::Conflict {
            current_revision: imported.revision,
        }
    );
    let after = audit_count(&store, PLUGIN_INSTANCE_CONFIG_NAMESPACE)
        .await
        .unwrap();
    assert_eq!(before, after);

    let mut bumped = body.clone();
    bumped
        .settings
        .insert("mode".into(), SettingValue::String("zip".into()));
    let applied = replace_instance_config(
        &store,
        &operator(),
        &instance.id,
        InstancePackagePolicy::Generic,
        imported.revision,
        &bumped,
        "replace-instance-config",
    )
    .await
    .expect("replace");
    let InstanceConfigReplace::Applied(doc) = applied else {
        panic!("expected apply, got {applied:?}");
    };
    assert_eq!(doc.revision, imported.revision + 1);
    let replay = replace_instance_config(
        &store,
        &operator(),
        &instance.id,
        InstancePackagePolicy::Generic,
        imported.revision,
        &bumped,
        "replace-instance-config",
    )
    .await
    .expect("replay");
    assert_eq!(
        replay,
        InstanceConfigReplace::Replayed {
            revision: doc.revision,
        }
    );
    let still = load_instance_config(&store, &instance.id).await.unwrap();
    assert_eq!(still.revision, doc.revision);

    let mut other = bumped.clone();
    other
        .settings
        .insert("mode".into(), SettingValue::String("web".into()));
    let conflict = replace_instance_config(
        &store,
        &operator(),
        &instance.id,
        InstancePackagePolicy::Generic,
        imported.revision,
        &other,
        "replace-instance-config",
    )
    .await
    .expect_err("different body");
    assert!(conflict.to_string().contains("idempotency conflict"));
    let unchanged = load_instance_config(&store, &instance.id).await.unwrap();
    assert_eq!(unchanged.body.settings_json()["mode"], "zip");
    assert!(!unchanged.body.to_json().unwrap().contains(secret));
}

#[tokio::test]
async fn capability_fixtures_share_one_config_address() {
    let source = include_str!("instance_config.rs");
    let resolver = source
        .split("pub async fn resolve_instance_bindings")
        .nth(1)
        .expect("resolver");
    let resolver = resolver
        .split("pub async fn seal_instance_secret")
        .next()
        .unwrap();
    assert!(
        !resolver.contains("settings_table_for"),
        "resolver must not call settings_table_for"
    );
    assert!(
        !resolver.contains("primary_family"),
        "resolver must not call primary_family"
    );

    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let _session = bootstrap(&store, dir.path()).await;
    let families = [
        "storefront",
        "remote_library",
        "storage",
        "database_adapter",
        "storefront_and_storage",
    ];
    let grant = InstanceBindingGrant {
        config: true,
        secrets: false,
    };
    let mut payloads = Vec::new();
    let mut dual_id = None;
    for (index, family) in families.iter().enumerate() {
        let instance = create_plugin_instance(
            &store,
            &operator(),
            &format!("platform:bookclerk/fixture-{family}"),
        )
        .await
        .expect("instance");
        let doc = import_instance_config_if_absent(
            &store,
            &ConfigActor::Bootstrap,
            &instance.id,
            InstancePackagePolicy::Generic,
            &mode_device(),
            &format!("import-fixture-{index}"),
        )
        .await
        .expect("import");
        assert_eq!(doc.instance_id, instance.id.as_str());
        let resolved = resolve_instance_bindings(&store, &instance.id, &grant)
            .await
            .expect("resolve");
        payloads.push(resolved.config.payload);
        if *family == "storefront_and_storage" {
            dual_id = Some(instance.id);
        }
    }
    let first = &payloads[0];
    assert!(payloads.iter().all(|payload| payload == first));
    let text = String::from_utf8(first.clone()).unwrap();
    assert!(text.contains("device"));
    let dual = dual_id.unwrap();
    let rows = configuration_documents::Entity::find()
        .all(store.db())
        .await
        .unwrap();
    let dual_rows = rows
        .iter()
        .filter(|row| row.scope_type == PLUGIN_INSTANCE_SCOPE_TYPE && row.scope_id == dual.as_str())
        .count();
    assert_eq!(dual_rows, 1);
}

#[tokio::test]
async fn unknown_instance_schema_fails_read_and_write() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let _session = bootstrap(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture")
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        InstancePackagePolicy::Generic,
        &mode_device(),
        "import-schema",
    )
    .await
    .unwrap();
    let row = configuration_documents::Entity::find_by_id((
        PLUGIN_INSTANCE_SCOPE_TYPE.to_string(),
        instance.id.as_str().to_string(),
        PLUGIN_INSTANCE_CONFIG_NAMESPACE.to_string(),
    ))
    .one(store.db())
    .await
    .unwrap()
    .unwrap();
    let mut active: configuration_documents::ActiveModel = row.into();
    active.schema_version = Set(9);
    active.update(store.db()).await.unwrap();
    let read = load_instance_config(&store, &instance.id)
        .await
        .expect_err("read");
    assert!(read
        .to_string()
        .contains("unsupported configuration schema"));
    let write = replace_instance_config(
        &store,
        &operator(),
        &instance.id,
        InstancePackagePolicy::Generic,
        1,
        &mode_device(),
        "replace-unknown-schema",
    )
    .await
    .expect_err("write");
    assert!(write
        .to_string()
        .contains("unsupported configuration schema"));
}

#[tokio::test]
async fn graphicaudio_allowlist_rejects_unknown_keys() {
    let mut settings = std::collections::BTreeMap::new();
    settings.insert("access".into(), SettingValue::String("device".into()));
    settings.insert("password".into(), SettingValue::String("nope".into()));
    let body = PluginInstanceConfigV1 {
        settings,
        secret_refs: Vec::new(),
    };
    assert!(body.validate(InstancePackagePolicy::GraphicAudio).is_err());
    let imported = graphicaudio_config_from_pairs([
        ("access", "device"),
        ("base_url", "https://example.test"),
    ])
    .expect("pairs");
    imported
        .validate(InstancePackagePolicy::GraphicAudio)
        .unwrap();
    assert!(graphicaudio_config_from_pairs([("access", "nope")]).is_err());
}

#[tokio::test]
async fn foreign_host_deployment_is_not_listed_for_the_local_host() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let session = bootstrap(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture")
        .await
        .unwrap();
    let local = ensure_plugin_deployment(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        &session.host.host_id,
    )
    .await
    .unwrap();
    let foreign_host = Uuid::new_v4().hyphenated().to_string();
    let foreign =
        ensure_plugin_deployment(&store, &ConfigActor::Bootstrap, &instance.id, &foreign_host)
            .await
            .unwrap();
    let visible = list_present_deployments_for_host(&store, &session.host.host_id)
        .await
        .unwrap();
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].deployment_id, local.deployment_id);
    assert!(
        load_observation(&store, &foreign.deployment_id, &foreign_host)
            .await
            .unwrap()
            .is_none()
    );
    let again = ensure_plugin_deployment(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        &session.host.host_id,
    )
    .await
    .unwrap();
    assert_eq!(again.deployment_id, local.deployment_id);
    assert_eq!(again.revision, local.revision);
}

#[tokio::test]
async fn deployment_replace_conflicts_without_touching_observations() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let session = bootstrap(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture")
        .await
        .unwrap();
    let deployment =
        ensure_plugin_deployment(&store, &operator(), &instance.id, &session.host.host_id)
            .await
            .unwrap();
    upsert_observation(
        &store,
        &DeploymentObservation {
            deployment_id: deployment.deployment_id.clone(),
            host_id: session.host.host_id.clone(),
            incarnation: "inc-1".into(),
            status: DeploymentStatus::Healthy,
            detail: String::new(),
            applied_config_revision: Some(1),
            observed_at: "2026-01-01T00:00:00Z".into(),
        },
    )
    .await
    .unwrap();
    let stale = replace_plugin_deployment(
        &store,
        &operator(),
        &deployment.deployment_id,
        9,
        "stale-deployment",
    )
    .await
    .unwrap();
    assert_eq!(
        stale,
        DeploymentReplace::Conflict {
            current_revision: 1,
        }
    );
    let applied = replace_plugin_deployment(
        &store,
        &operator(),
        &deployment.deployment_id,
        1,
        "replace-deployment",
    )
    .await
    .unwrap();
    let DeploymentReplace::Applied(next) = applied else {
        panic!("expected apply");
    };
    assert_eq!(next.revision, 2);
    assert_eq!(next.desired, DESIRED_PRESENT);
    let replay = replace_plugin_deployment(
        &store,
        &operator(),
        &deployment.deployment_id,
        1,
        "replace-deployment",
    )
    .await
    .unwrap();
    assert_eq!(replay, DeploymentReplace::Replayed { revision: 2 });
    let observed = load_observation(&store, &deployment.deployment_id, &session.host.host_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.status, DeploymentStatus::Healthy);
    assert_eq!(observed.applied_config_revision, Some(1));
}
