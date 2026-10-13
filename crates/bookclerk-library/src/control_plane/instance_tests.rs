//! Plugin instance identity, configuration, and deployment tests.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bookclerk_config::EventsConfig;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, EntityTrait, TransactionTrait};
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

/// Schema the fixture plugin declares: `mode` enum, `retries` integer with a
/// default, and a `token` secret.
fn fixture_schema() -> bookclerk_plugin_abi::SettingsSchema {
    use bookclerk_plugin_abi::{SettingKind, SettingSpec, SettingValueSpec, SettingsSchema};
    let spec = |key: &str, kind: SettingKind| SettingSpec {
        key: key.into(),
        label: String::new(),
        kind,
        values: Vec::new(),
        default: None,
        required: false,
        description: None,
    };
    SettingsSchema::new(vec![
        SettingSpec {
            values: ["device", "zip", "web"]
                .into_iter()
                .map(|id| SettingValueSpec {
                    id: id.into(),
                    label: String::new(),
                })
                .collect(),
            ..spec("mode", SettingKind::Enum)
        },
        SettingSpec {
            default: Some(serde_json::json!(2)),
            ..spec("retries", SettingKind::Integer)
        },
        spec("token", SettingKind::Secret),
    ])
}

async fn bootstrap(store: &LibraryStore, files: &std::path::Path) -> ControlPlaneSession {
    let scratch = files.join("cache");
    bootstrap_control_plane(store, files, &scratch, None, &EventsConfig::default())
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
    let first = create_plugin_instance(&store, &operator(), key, "")
        .await
        .expect("first");
    let second = create_plugin_instance(&store, &operator(), key, "")
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

    let race_key = "platform:bookclerk/enrollment-race";
    let (left, right) = tokio::join!(
        ensure_plugin_instance(&store, &ConfigActor::Bootstrap, race_key),
        ensure_plugin_instance(&store, &ConfigActor::Bootstrap, race_key),
    );
    let left = left.expect("left enrollment");
    let right = right.expect("right enrollment");
    assert_eq!(left.id, right.id);
    let explicit = create_plugin_instance(&store, &operator(), race_key, "")
        .await
        .expect("explicit same-key instance");
    assert_ne!(explicit.id, left.id);
    let enrolled = ensure_plugin_instance(&store, &ConfigActor::Bootstrap, race_key)
        .await
        .expect("enrolled default");
    assert_eq!(enrolled.id, left.id);
    let listed = list_plugin_instances_for_key(&store, race_key)
        .await
        .expect("list");
    assert_eq!(listed.len(), 2);
}

#[tokio::test]
async fn ensure_shares_one_id_across_two_sqlite_connections() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    drop(file_store(&path).await);
    let left_store = file_store(&path).await;
    let right_store = file_store(&path).await;
    let holder = file_store(&path).await;
    let txn = holder.db().begin().await.expect("hold the file lock");
    txn.execute_unprepared("CREATE TABLE IF NOT EXISTS enrollment_hold (id INTEGER PRIMARY KEY)")
        .await
        .expect("write while holding the lock");
    let key = "platform:bookclerk/cross-connection";
    let left_task = tokio::spawn(async move {
        ensure_plugin_instance(&left_store, &ConfigActor::Bootstrap, key).await
    });
    let right_task = tokio::spawn(async move {
        ensure_plugin_instance(&right_store, &ConfigActor::Bootstrap, key).await
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    txn.commit().await.expect("release the file lock");
    let left = left_task
        .await
        .expect("left task")
        .expect("left enrollment");
    let right = right_task
        .await
        .expect("right task")
        .expect("right enrollment");
    assert_eq!(left.id, right.id);

    let explicit = create_plugin_instance(&holder, &operator(), key, "")
        .await
        .expect("explicit same-key instance");
    assert_ne!(explicit.id, left.id);
    let enrolled = ensure_plugin_instance(&holder, &ConfigActor::Bootstrap, key)
        .await
        .expect("enrolled default");
    assert_eq!(enrolled.id, left.id);
    let listed = list_plugin_instances_for_key(&holder, key)
        .await
        .expect("list");
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().any(|row| row.id == left.id));
    assert!(listed.iter().any(|row| row.id == explicit.id));
}

/// Applies a host batch on an open connection and never calls `BEGIN`.
struct OpenBatchExec {
    /// Same file the store already migrated.
    db: sea_orm::DatabaseConnection,
    /// How many batches enrollment sent.
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl crate::TypedAtomicExec for OpenBatchExec {
    async fn execute_typed(
        &self,
        envelope: bookclerk_db_exec::AdapterExecuteRequest,
    ) -> std::result::Result<bookclerk_plugin_abi::ExecuteReply, bookclerk_plugin_abi::PluginError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        bookclerk_db_exec::execute_typed_on_open_connection(
            &self.db,
            &envelope,
            bookclerk_db_exec::ExecCaps::from(8u32),
            bookclerk_db_exec::AtomicSession::from_deadline(None),
            None,
        )
        .await
        .map_err(|err| bookclerk_plugin_abi::PluginError::unavailable(err.to_string()))
    }
}

#[tokio::test]
async fn ensure_uses_a_host_batch_when_begin_is_rejected() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let store = file_store(&path).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let db = store.db().clone();
    let store = store.with_typed_exec(Arc::new(OpenBatchExec {
        db,
        calls: calls.clone(),
    }));
    crate::inject_begin_failures(8);
    let key = "platform:bookclerk/d1-batch";
    let enrolled = ensure_plugin_instance(&store, &ConfigActor::Bootstrap, key)
        .await
        .expect("batch enrollment");
    assert!(calls.load(Ordering::SeqCst) >= 1);
    let again = ensure_plugin_instance(&store, &ConfigActor::Bootstrap, key)
        .await
        .expect("second enrollment");
    assert_eq!(again.id, enrolled.id);
    assert_eq!(
        list_plugin_instances_for_key(&store, key)
            .await
            .expect("list")
            .len(),
        1
    );
}

#[tokio::test]
async fn instance_config_cas_keeps_secret_values_out_of_the_document() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let _session = bootstrap(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture", "")
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
        &fixture_schema(),
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
        &fixture_schema(),
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
        &fixture_schema(),
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
        &fixture_schema(),
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
        &fixture_schema(),
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
            "",
        )
        .await
        .expect("instance");
        let doc = import_instance_config_if_absent(
            &store,
            &ConfigActor::Bootstrap,
            &instance.id,
            &fixture_schema(),
            &mode_device(),
            &format!("import-fixture-{index}"),
        )
        .await
        .expect("import");
        assert_eq!(doc.instance_id, instance.id.as_str());
        let resolved = resolve_instance_bindings(&store, &instance.id, &grant, &fixture_schema())
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
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture", "")
        .await
        .unwrap();
    import_instance_config_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        &fixture_schema(),
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
        &fixture_schema(),
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
async fn documents_validate_against_the_pinned_schema_not_the_host() {
    let schema = fixture_schema();
    let mut settings = std::collections::BTreeMap::new();
    settings.insert("mode".into(), SettingValue::String("device".into()));
    settings.insert("password".into(), SettingValue::String("nope".into()));
    let unknown = PluginInstanceConfigV1 {
        settings: settings.clone(),
        secret_refs: Vec::new(),
    };
    assert!(unknown.validate(&schema).is_err(), "undeclared key");
    settings.remove("password");
    settings.insert("token".into(), SettingValue::String("plain".into()));
    let plain_secret = PluginInstanceConfigV1 {
        settings: settings.clone(),
        secret_refs: Vec::new(),
    };
    assert!(
        plain_secret.validate(&schema).is_err(),
        "secret as plain value"
    );
    settings.remove("token");
    settings.insert("mode".into(), SettingValue::String("tape".into()));
    let bad_enum = PluginInstanceConfigV1 {
        settings: settings.clone(),
        secret_refs: Vec::new(),
    };
    assert!(bad_enum.validate(&schema).is_err(), "enum outside values");
    settings.insert("mode".into(), SettingValue::String("zip".into()));
    settings.insert("retries".into(), SettingValue::String("3".into()));
    let wrong_type = PluginInstanceConfigV1 {
        settings: settings.clone(),
        secret_refs: Vec::new(),
    };
    assert!(wrong_type.validate(&schema).is_err(), "string for integer");
    settings.insert("retries".into(), SettingValue::Number(3.into()));
    let ok = PluginInstanceConfigV1 {
        settings,
        secret_refs: vec![InstanceSecretRefV1 {
            key: "token".into(),
            name: "device_token".into(),
        }],
    };
    ok.validate(&schema)
        .expect("declared keys with fitting values");
    assert_eq!(
        parse_setting_text(&schema, "retries", "7").unwrap(),
        SettingValue::Number(7.into())
    );
    assert!(parse_setting_text(&schema, "token", "x").is_err());
    assert!(parse_setting_text(&schema, "mode", "tape").is_err());
}

#[tokio::test]
async fn resolve_merges_schema_defaults_and_drops_retired_keys() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let _session = bootstrap(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture", "")
        .await
        .unwrap();
    let mut body = PluginInstanceConfigV1::empty();
    body.settings
        .insert("mode".into(), SettingValue::String("zip".into()));
    body.settings
        .insert("legacy".into(), SettingValue::Bool(true));
    let mut wide = fixture_schema();
    wide.settings.push(bookclerk_plugin_abi::SettingSpec {
        key: "legacy".into(),
        label: String::new(),
        kind: bookclerk_plugin_abi::SettingKind::Bool,
        values: Vec::new(),
        default: None,
        required: false,
        description: None,
    });
    import_instance_config_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        &wide,
        &body,
        "import-defaults",
    )
    .await
    .unwrap();
    // The current manifest no longer declares `legacy`; spawn drops it and
    // fills `retries` from the schema default.
    let grant = InstanceBindingGrant {
        config: true,
        secrets: false,
    };
    let resolved = resolve_instance_bindings(&store, &instance.id, &grant, &fixture_schema())
        .await
        .unwrap();
    let config = resolved.config.json_value().unwrap();
    assert_eq!(config["mode"], "zip");
    assert_eq!(config["retries"], 2);
    assert!(config.get("legacy").is_none());
    assert_eq!(resolved.dropped_keys, vec!["legacy".to_string()]);
}

#[tokio::test]
async fn foreign_host_deployment_is_not_listed_for_the_local_host() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let session = bootstrap(&store, dir.path()).await;
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture", "")
        .await
        .unwrap();
    let local = ensure_plugin_deployment(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        &session.host.host_id,
        DESIRED_PRESENT,
    )
    .await
    .unwrap();
    let foreign_host = Uuid::new_v4().hyphenated().to_string();
    let foreign = ensure_plugin_deployment(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        &foreign_host,
        DESIRED_PRESENT,
    )
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
        DESIRED_PRESENT,
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
    let instance = create_plugin_instance(&store, &operator(), "platform:bookclerk/fixture", "")
        .await
        .unwrap();
    let deployment = ensure_plugin_deployment(
        &store,
        &operator(),
        &instance.id,
        &session.host.host_id,
        DESIRED_PRESENT,
    )
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
        DESIRED_PRESENT,
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
        DESIRED_PRESENT,
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
        DESIRED_PRESENT,
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

#[tokio::test]
async fn two_connections_observe_one_instance_document() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let files_a = tempfile::tempdir().unwrap();
    let files_b = tempfile::tempdir().unwrap();
    let store_a = file_store(&path).await;
    let session_a = bootstrap(&store_a, files_a.path()).await;
    std::fs::copy(
        crate::master_key::master_key_path(files_a.path()),
        crate::master_key::master_key_path(files_b.path()),
    )
    .unwrap();
    let store_b = file_store(&path).await;
    let session_b = bootstrap(&store_b, files_b.path()).await;
    assert_eq!(session_a.cluster_id, session_b.cluster_id);
    assert_ne!(session_a.host.host_id, session_b.host.host_id);

    // Host A enrols the plugin: one default instance, seeded from defaults.
    let key = "platform:bookclerk/shared-fixture";
    let instance = ensure_plugin_instance(&store_a, &ConfigActor::Bootstrap, key)
        .await
        .unwrap();
    let seeded = import_instance_config_if_absent(
        &store_a,
        &ConfigActor::Bootstrap,
        &instance.id,
        &fixture_schema(),
        &PluginInstanceConfigV1::empty(),
        "seed-shared-fixture",
    )
    .await
    .unwrap();

    // Host B enrols the same plugin key and lands on the same instance.
    let from_b = ensure_plugin_instance(&store_b, &ConfigActor::Bootstrap, key)
        .await
        .unwrap();
    assert_eq!(from_b.id, instance.id);
    let doc_b = load_instance_config(&store_b, &instance.id).await.unwrap();
    assert_eq!(doc_b.revision, seeded.revision);

    // An operator on host B writes; host A observes the new revision.
    let mut body = PluginInstanceConfigV1::empty();
    body.settings
        .insert("mode".into(), SettingValue::String("zip".into()));
    let applied = replace_instance_config(
        &store_b,
        &operator(),
        &instance.id,
        &fixture_schema(),
        seeded.revision,
        &body,
        "host-b-writes",
    )
    .await
    .unwrap();
    let InstanceConfigReplace::Applied(doc) = applied else {
        panic!("expected apply, got {applied:?}");
    };
    let seen_by_a = load_instance_config(&store_a, &instance.id).await.unwrap();
    assert_eq!(seen_by_a.revision, doc.revision);
    assert_eq!(seen_by_a.body.settings_json()["mode"], "zip");

    // Host A's write against the revision it last saw is rejected.
    let stale = replace_instance_config(
        &store_a,
        &operator(),
        &instance.id,
        &fixture_schema(),
        seeded.revision,
        &mode_device(),
        "host-a-stale",
    )
    .await
    .unwrap();
    assert_eq!(
        stale,
        InstanceConfigReplace::Conflict {
            current_revision: doc.revision,
        }
    );

    // A spawn on either host resolves the same CONFIG at the same revision.
    let grant = InstanceBindingGrant {
        config: true,
        secrets: false,
    };
    let on_a = resolve_instance_bindings(&store_a, &instance.id, &grant, &fixture_schema())
        .await
        .unwrap();
    let on_b = resolve_instance_bindings(&store_b, &instance.id, &grant, &fixture_schema())
        .await
        .unwrap();
    assert_eq!(on_a.config, on_b.config);
    assert_eq!(on_a.config_revision, doc.revision);
    assert_eq!(on_a.config.json_value().unwrap()["retries"], 2);

    // Deployments are host-scoped: the same instance is present on A and
    // absent on B, and each host reads the other's row without owning it.
    ensure_plugin_deployment(
        &store_a,
        &ConfigActor::Bootstrap,
        &instance.id,
        &session_a.host.host_id,
        DESIRED_PRESENT,
    )
    .await
    .unwrap();
    ensure_plugin_deployment(
        &store_b,
        &ConfigActor::Bootstrap,
        &instance.id,
        &session_b.host.host_id,
        DESIRED_ABSENT,
    )
    .await
    .unwrap();
    let present_on_a = list_present_deployments_for_host(&store_b, &session_a.host.host_id)
        .await
        .unwrap();
    assert_eq!(present_on_a.len(), 1);
    assert_eq!(present_on_a[0].plugin_instance_id, instance.id);
    assert!(
        list_present_deployments_for_host(&store_a, &session_b.host.host_id)
            .await
            .unwrap()
            .is_empty()
    );
    let b_row = load_deployment_for_host(&store_a, &instance.id, &session_b.host.host_id)
        .await
        .unwrap()
        .expect("host B's row is visible from host A");
    assert_eq!(b_row.desired, DESIRED_ABSENT);
}

#[tokio::test]
async fn schema_pin_follows_the_package_version_and_prunes_documents() {
    use bookclerk_plugin_abi::{SettingKind, SettingSpec, SettingsSchema};
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(dir.path().join("library.db").as_path()).await;
    let _session = bootstrap(&store, dir.path()).await;
    const KEY: &str = "platform:bookclerk/fxver";
    fn pin<'a>(
        version: &'a str,
        sha: &'a str,
        host: &'a str,
    ) -> impl Fn(&'a SettingsSchema) -> SchemaPin<'a> {
        move |schema| SchemaPin {
            plugin_key: KEY,
            manifest_id: "fxver",
            manifest_sha256: sha,
            package_version: version,
            schema,
            host_id: host,
        }
    }

    // A version is required.
    let v1 = fixture_schema();
    let err = pin_plugin_schema(&store, &pin("", "aa", "host-a")(&v1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("`version` is required"), "{err}");
    let err = pin_plugin_schema(&store, &pin("v1", "aa", "host-a")(&v1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not a semantic version"), "{err}");

    // First pin, then an unchanged re-run.
    let first = pin_plugin_schema(&store, &pin("0.1.0", "aa", "host-a")(&v1))
        .await
        .unwrap();
    assert_eq!(first.change, SchemaPinChange::Pinned);
    assert_eq!(first.schema.package_version, "0.1.0");
    let again = pin_plugin_schema(&store, &pin("0.1.0", "aa", "host-a")(&v1))
        .await
        .unwrap();
    assert_eq!(again.change, SchemaPinChange::Unchanged);
    assert_eq!(again.schema.recorded_at, first.schema.recorded_at);

    // A document written under 0.1.0 uses `retries` and a `token` secret ref.
    let instance = create_plugin_instance(&store, &operator(), KEY, "")
        .await
        .unwrap();
    let mut body = PluginInstanceConfigV1::empty();
    body.settings
        .insert("mode".into(), SettingValue::String("zip".into()));
    body.settings
        .insert("retries".into(), SettingValue::Number(3.into()));
    body.secret_refs.push(InstanceSecretRefV1 {
        key: "token".into(),
        name: "device_token".into(),
    });
    let seeded = import_instance_config_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &instance.id,
        &v1,
        &body,
        "seed-fxver",
    )
    .await
    .unwrap();

    // 0.2.0 drops `retries` and makes `token` a plain string: the newer
    // package re-pins and the document is pruned to what still validates.
    let v2 = SettingsSchema::new(vec![
        v1.get("mode").cloned().unwrap(),
        SettingSpec {
            key: "token".into(),
            label: String::new(),
            kind: SettingKind::String,
            values: Vec::new(),
            default: None,
            required: false,
            description: None,
        },
    ]);
    let upgraded = pin_plugin_schema(&store, &pin("0.2.0", "bb", "host-b")(&v2))
        .await
        .unwrap();
    assert_eq!(
        upgraded.change,
        SchemaPinChange::Upgraded {
            previous_version: "0.1.0".into()
        }
    );
    assert_eq!(upgraded.schema.recorded_by_host, "host-b");
    let pruned = prune_instance_configs_to_schema(&store, KEY, &v2)
        .await
        .unwrap();
    assert_eq!(pruned.len(), 1);
    assert_eq!(pruned[0].instance_id, instance.id.as_str());
    assert_eq!(pruned[0].removed_keys, vec!["retries", "token"]);
    assert_eq!(pruned[0].revision, seeded.revision + 1);
    let doc = load_instance_config(&store, &instance.id).await.unwrap();
    assert_eq!(doc.revision, seeded.revision + 1);
    assert_eq!(doc.body.settings.len(), 1);
    assert_eq!(
        doc.body.settings.get("mode"),
        Some(&SettingValue::String("zip".into()))
    );
    assert!(doc.body.secret_refs.is_empty());
    doc.body.validate(&v2).expect("pruned document fits 0.2.0");
    assert!(
        prune_instance_configs_to_schema(&store, KEY, &v2)
            .await
            .unwrap()
            .is_empty(),
        "a fitting document is not rewritten"
    );

    // The host still on 0.1.0 is stale: the row is untouched and the
    // reconciler-side check agrees.
    let stale = pin_plugin_schema(&store, &pin("0.1.0", "aa", "host-a")(&v1))
        .await
        .unwrap();
    assert_eq!(
        stale.change,
        SchemaPinChange::Stale {
            local_version: "0.1.0".into()
        }
    );
    assert_eq!(stale.schema.package_version, "0.2.0");
    assert_eq!(stale.schema.recorded_by_host, "host-b");
    assert_eq!(stale.schema.schema, v2);
    assert_eq!(
        stale_against_pin(&store, KEY, "0.1.0")
            .await
            .unwrap()
            .map(|row| row.package_version),
        Some("0.2.0".into())
    );
    assert!(stale_against_pin(&store, KEY, "0.2.0")
        .await
        .unwrap()
        .is_none());
    assert!(stale_against_pin(&store, KEY, "0.3.0")
        .await
        .unwrap()
        .is_none());
    assert!(
        stale_against_pin(&store, "platform:bookclerk/never", "0.1.0")
            .await
            .unwrap()
            .is_none()
    );

    // Same version, different manifest: re-pinned in place.
    let repinned = pin_plugin_schema(&store, &pin("0.2.0", "cc", "host-a")(&v2))
        .await
        .unwrap();
    assert_eq!(repinned.change, SchemaPinChange::Repinned);
    assert_eq!(repinned.schema.manifest_sha256, "cc");
    assert_eq!(repinned.schema.recorded_by_host, "host-a");
}
