//! Control-plane identity, configuration, and secret-root tests.

use std::path::Path;
use std::sync::Arc;

use bookclerk_config::EventsConfig;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ConnectionTrait, EntityTrait};
use tempfile::tempdir;
use uuid::Uuid;

use super::secret::align_cluster_root;
use super::*;
use crate::entities::configuration_documents;
use crate::master_key::{master_key_fingerprint, master_key_path, master_key_test_lock_async};
use crate::require_master_key;
use crate::store::LibraryStore;

async fn memory_store() -> LibraryStore {
    LibraryStore::from_connection(
        bookclerk_plugin_database_sqlite::open_memory()
            .await
            .expect("sqlite memory"),
    )
}

async fn file_store(path: &Path) -> LibraryStore {
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

fn events(retention: u64) -> EventsSettingsV1 {
    EventsSettingsV1 {
        retention_days: retention,
        dead_letter_retention_days: 30,
        concurrency: 1,
    }
}

async fn bootstrap_pair(store: &LibraryStore, files: &Path) -> ControlPlaneSession {
    let scratch = files.join("cache");
    bootstrap_control_plane(store, files, &scratch, None, &EventsConfig::default())
        .await
        .expect("bootstrap")
}

#[tokio::test]
async fn host_identity_is_stable_and_distinct() {
    let dir = tempdir().unwrap();
    let first = load_or_create_host_identity(dir.path()).unwrap();
    let again = load_or_create_host_identity(dir.path()).unwrap();
    assert_eq!(first.host_id, again.host_id);
    assert!(Uuid::parse_str(&first.host_id).is_ok());
    let hostname = std::env::var("HOSTNAME").unwrap_or_default();
    assert_ne!(first.host_id, hostname);

    let other = tempdir().unwrap();
    let second = load_or_create_host_identity(other.path()).unwrap();
    assert_ne!(first.host_id, second.host_id);

    let copied = tempdir().unwrap();
    std::fs::copy(
        host_identity_path(dir.path()),
        host_identity_path(copied.path()),
    )
    .unwrap();
    let from_copy = load_or_create_host_identity(copied.path()).unwrap();
    assert_eq!(from_copy.host_id, first.host_id);
}

#[tokio::test]
async fn restart_keeps_host_id_and_distinct_hosts_stay_distinct() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let first = bootstrap_pair(&store, dir_a.path()).await;
    let restarted = bootstrap_pair(&store, dir_a.path()).await;
    assert_eq!(first.host.host_id, restarted.host.host_id);
    assert_eq!(first.host.created_at, restarted.host.created_at);
    assert_eq!(first.cluster_id, restarted.cluster_id);

    std::fs::copy(master_key_path(dir_a.path()), master_key_path(dir_b.path())).unwrap();
    let second = bootstrap_pair(&store, dir_b.path()).await;
    assert_ne!(first.host.host_id, second.host.host_id);
    assert_eq!(first.cluster_id, second.cluster_id);
    let ids = super::identity::list_host_ids(&store).await.unwrap();
    assert_eq!(ids.len(), 2);
}

#[tokio::test]
async fn copied_identity_rejects_a_different_cluster() {
    let _guard = master_key_test_lock_async().await;
    let store_a = memory_store().await;
    let store_b = memory_store().await;
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let session_a = bootstrap_pair(&store_a, dir_a.path()).await;
    let session_b = bootstrap_pair(&store_b, dir_b.path()).await;
    assert_ne!(session_a.cluster_id, session_b.cluster_id);
    let fingerprint = super::secret::load_cluster_row(&store_b)
        .await
        .unwrap()
        .unwrap()
        .secret_fingerprint;
    std::fs::copy(
        host_identity_path(dir_a.path()),
        host_identity_path(dir_b.path()),
    )
    .unwrap();
    let scratch_b = dir_b.path().join("cache");
    let err = bootstrap_control_plane(
        &store_b,
        dir_b.path(),
        &scratch_b,
        None,
        &EventsConfig::default(),
    )
    .await
    .expect_err("copied identity must not join a different cluster");
    assert!(err.to_string().contains("cluster mismatch"), "{err}");
    let after = super::secret::load_cluster_row(&store_b)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after.cluster_id, session_b.cluster_id);
    assert_eq!(after.secret_fingerprint, fingerprint);
    let file = load_or_create_host_identity(dir_b.path()).unwrap();
    assert_eq!(
        file.cluster_id.as_deref(),
        Some(session_a.cluster_id.as_str())
    );
}

#[tokio::test]
async fn independent_connections_share_cluster_events_and_isolate_hosts() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let files_a = tempdir().unwrap();
    let files_b = tempdir().unwrap();
    let store_a = file_store(&path).await;
    let session_a = bootstrap_pair(&store_a, files_a.path()).await;
    let store_b = file_store(&path).await;
    std::fs::copy(
        master_key_path(files_a.path()),
        master_key_path(files_b.path()),
    )
    .unwrap();
    let session_b = bootstrap_pair(&store_b, files_b.path()).await;
    assert_eq!(session_a.cluster_id, session_b.cluster_id);
    assert_eq!(session_a.events.body, session_b.events.body);

    let label_a = HostRuntimeSettingsV1 {
        label: "alpha".into(),
    };
    let applied = replace_host_runtime(
        &store_a,
        &operator(),
        &session_a.host.host_id,
        session_a.host_runtime.revision,
        &label_a,
        "host-a-label",
    )
    .await
    .unwrap();
    let HostRuntimeReplace::Applied(doc_a) = applied else {
        panic!("expected apply");
    };
    let seen_by_b = load_host_runtime(&store_b, &session_a.host.host_id)
        .await
        .unwrap();
    assert_eq!(seen_by_b.body.label, "alpha");
    let local_b = load_host_runtime(&store_b, &session_b.host.host_id)
        .await
        .unwrap();
    assert_ne!(local_b.body.label, doc_a.body.label);
    assert_eq!(local_b.scope_id, session_b.host.host_id);
    assert_eq!(doc_a.scope_id, session_a.host.host_id);
}

#[tokio::test]
async fn same_expected_revision_cannot_commit_twice() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    let before_audit = audit_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let before_changes = change_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let first = replace_events(
        &store,
        &operator(),
        session.events.revision,
        &events(4),
        "events-write-1",
    )
    .await
    .unwrap();
    let EventsReplace::Applied(applied) = first else {
        panic!("first write should apply: {first:?}");
    };
    assert_eq!(applied.revision, session.events.revision + 1);
    let second = replace_events(
        &store,
        &operator(),
        session.events.revision,
        &events(5),
        "events-write-2",
    )
    .await
    .unwrap();
    let EventsReplace::Conflict { current_revision } = second else {
        panic!("second write should conflict: {second:?}");
    };
    assert_eq!(current_revision, applied.revision);
    let loaded = load_events(&store).await.unwrap();
    assert_eq!(loaded.body.retention_days, 4);
    assert_eq!(
        audit_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        before_audit + 1
    );
    assert_eq!(
        change_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        before_changes + 1
    );
    let replay = replace_events(
        &store,
        &operator(),
        session.events.revision,
        &events(4),
        "events-write-1",
    )
    .await
    .unwrap();
    assert!(
        matches!(replay, EventsReplace::Applied(ref doc) if doc.revision == applied.revision)
            || matches!(replay, EventsReplace::Replayed { revision } if revision == applied.revision),
        "{replay:?}"
    );
    assert_eq!(
        load_events(&store).await.unwrap().revision,
        applied.revision
    );
}

#[tokio::test]
async fn invalid_unauthorized_and_unsupported_versions_do_not_commit() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    let revision = session.events.revision;
    let invalid = replace_events(
        &store,
        &operator(),
        revision,
        &EventsSettingsV1 {
            retention_days: 0,
            dead_letter_retention_days: 30,
            concurrency: 1,
        },
        "invalid-events",
    )
    .await
    .expect_err("zero retention");
    assert!(
        invalid.to_string().contains("invalid configuration"),
        "{invalid}"
    );

    let denied = replace_events(
        &store,
        &ConfigActor::Member {
            id: "member".into(),
        },
        revision,
        &events(3),
        "member-events",
    )
    .await
    .expect_err("member");
    assert!(
        denied
            .to_string()
            .contains("unauthorized configuration write"),
        "{denied}"
    );
    let admin = replace_events(
        &store,
        &ConfigActor::Administrator { id: "admin".into() },
        revision,
        &events(3),
        "admin-events",
    )
    .await
    .expect_err("administrator");
    assert!(admin
        .to_string()
        .contains("unauthorized configuration write"));

    let row = configuration_documents::Entity::find()
        .all(store.db())
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.namespace == EVENTS_NAMESPACE)
        .unwrap();
    let mut active: configuration_documents::ActiveModel = row.into();
    active.schema_version = Set(99);
    active.update(store.db()).await.unwrap();
    let unsupported = load_events(&store).await.expect_err("schema 99");
    assert!(
        unsupported
            .to_string()
            .contains("unsupported configuration schema"),
        "{unsupported}"
    );
    let still = configuration_documents::Entity::find()
        .all(store.db())
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.namespace == EVENTS_NAMESPACE)
        .unwrap();
    assert_eq!(still.schema_version, 99);
    assert_eq!(still.revision, revision);
}

#[tokio::test]
async fn import_is_idempotent_and_later_seeds_do_not_overwrite() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    assert_eq!(session.events.revision, 1);
    assert_eq!(session.events.body.retention_days, 7);
    let again = import_events_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &EventsConfig {
            retention_days: 2,
            dead_letter_retention_days: 3,
            concurrency: 4,
        },
        "second-import",
    )
    .await
    .unwrap();
    assert_eq!(again.revision, 1);
    assert_eq!(again.body.retention_days, 7);
    assert_eq!(again.body.concurrency, 1);

    let mut config = bookclerk_config::Config::default();
    config.events.retention_days = 11;
    overlay_events(&mut config, &again, &session.cluster_id);
    assert_eq!(config.events.retention_days, 7);
    assert_eq!(config.events_revision, Some(1));
    assert_eq!(
        config.events_authority.as_deref(),
        Some(session.cluster_id.as_str())
    );
}

#[tokio::test]
async fn replay_after_a_later_edit_returns_the_original_revision() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    let expected = session.events.revision;
    let first = replace_events(&store, &operator(), expected, &events(4), "op-a")
        .await
        .unwrap();
    let EventsReplace::Applied(applied_a) = first else {
        panic!("A should apply: {first:?}");
    };
    let second = replace_events(&store, &operator(), applied_a.revision, &events(6), "op-b")
        .await
        .unwrap();
    let EventsReplace::Applied(applied_b) = second else {
        panic!("B should apply: {second:?}");
    };
    let audits = audit_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let changes = change_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let replay = replace_events(&store, &operator(), expected, &events(4), "op-a")
        .await
        .unwrap();
    assert_eq!(
        replay,
        EventsReplace::Replayed {
            revision: applied_a.revision,
        }
    );
    let loaded = load_events(&store).await.unwrap();
    assert_eq!(loaded.revision, applied_b.revision);
    assert_eq!(loaded.body.retention_days, 6);
    assert_eq!(audit_count(&store, EVENTS_NAMESPACE).await.unwrap(), audits);
    assert_eq!(
        change_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        changes
    );

    let changed = replace_events(&store, &operator(), expected, &events(8), "op-a")
        .await
        .expect_err("different payload reuses the operation id");
    assert!(
        changed.to_string().contains("idempotency conflict"),
        "{changed}"
    );
    assert_eq!(
        load_events(&store).await.unwrap().revision,
        applied_b.revision
    );

    let denied = replace_events(
        &store,
        &ConfigActor::Member {
            id: "member".into(),
        },
        expected,
        &events(4),
        "op-a",
    )
    .await
    .expect_err("member replay");
    assert!(denied
        .to_string()
        .contains("unauthorized configuration write"));
}

#[tokio::test]
async fn expired_receipt_for_the_same_operation_still_replays() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    let applied = replace_events(
        &store,
        &operator(),
        session.events.revision,
        &events(4),
        "op-expire",
    )
    .await
    .unwrap();
    let EventsReplace::Applied(applied) = applied else {
        panic!("apply: {applied:?}");
    };
    let row = crate::entities::bookclerk_receipts::Entity::find_by_id("op-expire")
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    let mut active: crate::entities::bookclerk_receipts::ActiveModel = row.into();
    active.expires_at = Set("2000-01-01T00:00:00Z".into());
    active.update(store.db()).await.unwrap();
    let replay = replace_events(
        &store,
        &operator(),
        session.events.revision,
        &events(4),
        "op-expire",
    )
    .await
    .unwrap();
    assert_eq!(
        replay,
        EventsReplace::Replayed {
            revision: applied.revision,
        }
    );
    assert_eq!(
        load_events(&store).await.unwrap().revision,
        applied.revision
    );
}

#[tokio::test]
async fn existing_events_ignore_an_invalid_seed() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    let audits = audit_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let changes = change_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let bad = EventsConfig {
        retention_days: 0,
        dead_letter_retention_days: 0,
        concurrency: 33,
    };
    let scratch = files.path().join("cache");
    let again = bootstrap_control_plane(&store, files.path(), &scratch, None, &bad)
        .await
        .expect("stored document is authoritative");
    assert_eq!(again.events.revision, session.events.revision);
    assert_eq!(again.events.body, session.events.body);
    assert_eq!(audit_count(&store, EVENTS_NAMESPACE).await.unwrap(), audits);
    assert_eq!(
        change_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        changes
    );
}

#[tokio::test]
async fn first_import_rejects_an_invalid_seed() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let scratch = files.path().join("cache");
    let err = bootstrap_control_plane(
        &store,
        files.path(),
        &scratch,
        None,
        &EventsConfig {
            retention_days: 0,
            dead_letter_retention_days: 30,
            concurrency: 1,
        },
    )
    .await
    .expect_err("invalid seed");
    assert!(err.to_string().contains("retention_days"), "{err}");
    let missing = load_events(&store).await.expect_err("no document");
    assert!(missing.to_string().contains("not initialized"), "{missing}");
    assert_eq!(audit_count(&store, EVENTS_NAMESPACE).await.unwrap(), 0);
    assert_eq!(change_count(&store, EVENTS_NAMESPACE).await.unwrap(), 0);
}

#[tokio::test]
async fn initial_import_commits_document_audit_and_change_together() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let store = file_store(&path).await;
    store
        .db()
        .execute_unprepared(
            "CREATE TRIGGER fail_config_audit BEFORE INSERT ON configuration_audit \
             BEGIN SELECT RAISE(ABORT, 'injected'); END",
        )
        .await
        .unwrap();
    let err = import_events_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &EventsConfig::default(),
        "import-fail",
    )
    .await
    .expect_err("audit insert aborts the batch");
    assert!(err.to_string().contains("injected"), "{err}");
    assert!(load_events(&store).await.is_err());
    assert_eq!(audit_count(&store, EVENTS_NAMESPACE).await.unwrap(), 0);
    assert_eq!(change_count(&store, EVENTS_NAMESPACE).await.unwrap(), 0);

    store
        .db()
        .execute_unprepared("DROP TRIGGER fail_config_audit")
        .await
        .unwrap();
    let imported = import_events_if_absent(
        &store,
        &ConfigActor::Bootstrap,
        &EventsConfig::default(),
        "import-ok",
    )
    .await
    .unwrap();
    assert_eq!(imported.revision, 1);
    assert_eq!(audit_count(&store, EVENTS_NAMESPACE).await.unwrap(), 1);
    assert_eq!(change_count(&store, EVENTS_NAMESPACE).await.unwrap(), 1);

    let other = file_store(&path).await;
    let seen = load_events(&other).await.unwrap();
    assert_eq!(seen, imported);
    assert_eq!(audit_count(&other, EVENTS_NAMESPACE).await.unwrap(), 1);
    assert_eq!(change_count(&other, EVENTS_NAMESPACE).await.unwrap(), 1);
}

#[tokio::test]
async fn retry_paused_after_empty_receipt_lookup_replays_without_writes() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let files = tempdir().unwrap();
    let store = file_store(&path).await;
    let session = bootstrap_pair(&store, files.path()).await;
    let peer = file_store(&path).await;
    let expected = session.events.revision;
    let before_audit = audit_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let before_changes = change_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    super::documents::arm_empty_receipt_pause(super::documents::EmptyReceiptPause {
        operation_id: "op-retry".into(),
        arrived: arrived_tx,
        release: release_rx,
    })
    .await;
    let retry_store = file_store(&path).await;
    let retry = tokio::spawn(async move {
        replace_events(&retry_store, &operator(), expected, &events(4), "op-retry").await
    });
    arrived_rx
        .await
        .expect("retry reaches the empty receipt lookup");
    let applied = replace_events(&peer, &operator(), expected, &events(4), "op-retry")
        .await
        .unwrap();
    let EventsReplace::Applied(doc_a) = applied else {
        panic!("original commit should apply: {applied:?}");
    };
    let later = replace_events(&store, &operator(), doc_a.revision, &events(6), "op-later")
        .await
        .unwrap();
    let EventsReplace::Applied(doc_b) = later else {
        panic!("later edit should apply: {later:?}");
    };
    release_tx.send(()).expect("release paused retry");
    let replay = retry.await.unwrap().unwrap();
    assert_eq!(
        replay,
        EventsReplace::Replayed {
            revision: doc_a.revision,
        }
    );
    let loaded = load_events(&store).await.unwrap();
    assert_eq!(loaded.revision, doc_b.revision);
    assert_eq!(loaded.body.retention_days, 6);
    assert_eq!(
        audit_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        before_audit + 2
    );
    assert_eq!(
        change_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        before_changes + 2
    );
}

#[tokio::test]
async fn paused_retry_with_a_different_payload_is_an_idempotency_conflict() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let files = tempdir().unwrap();
    let store = file_store(&path).await;
    let session = bootstrap_pair(&store, files.path()).await;
    let peer = file_store(&path).await;
    let expected = session.events.revision;
    let before_audit = audit_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let before_changes = change_count(&store, EVENTS_NAMESPACE).await.unwrap();
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    super::documents::arm_empty_receipt_pause(super::documents::EmptyReceiptPause {
        operation_id: "op-changed".into(),
        arrived: arrived_tx,
        release: release_rx,
    })
    .await;
    let retry_store = file_store(&path).await;
    let retry = tokio::spawn(async move {
        replace_events(
            &retry_store,
            &operator(),
            expected,
            &events(8),
            "op-changed",
        )
        .await
    });
    arrived_rx
        .await
        .expect("changed-payload retry reaches the empty lookup");
    let applied = replace_events(&peer, &operator(), expected, &events(4), "op-changed")
        .await
        .unwrap();
    let EventsReplace::Applied(doc_a) = applied else {
        panic!("original commit should apply: {applied:?}");
    };
    release_tx.send(()).expect("release changed-payload retry");
    let err = retry.await.unwrap().expect_err("different payload");
    assert!(err.to_string().contains("idempotency conflict"), "{err}");
    let loaded = load_events(&store).await.unwrap();
    assert_eq!(loaded.revision, doc_a.revision);
    assert_eq!(loaded.body.retention_days, 4);
    assert_eq!(
        audit_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        before_audit + 1
    );
    assert_eq!(
        change_count(&store, EVENTS_NAMESPACE).await.unwrap(),
        before_changes + 1
    );
}

#[tokio::test]
async fn begin_waits_for_a_peer_sqlite_writer() {
    use sea_orm::TransactionTrait;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let holder = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
    let waiter = bookclerk_plugin_database_sqlite::open(&path).await.unwrap();
    let txn = holder.begin().await.expect("holder begin");
    txn.execute_unprepared("CREATE TABLE hold_lock (id INTEGER PRIMARY KEY)")
        .await
        .unwrap();
    let waiting = tokio::spawn(async move {
        let txn = waiter.begin().await.expect("waiter begin");
        txn.execute_unprepared("CREATE TABLE IF NOT EXISTS waiter_seen (id INTEGER PRIMARY KEY)")
            .await
            .unwrap();
        txn.commit().await.unwrap();
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    txn.commit().await.unwrap();
    waiting.await.unwrap();
}

#[tokio::test]
async fn concurrent_imports_leave_one_document_and_one_notice() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let store_a = file_store(&path).await;
    let store_b = file_store(&path).await;
    let left = tokio::spawn(async move {
        import_events_if_absent(
            &store_a,
            &ConfigActor::Bootstrap,
            &EventsConfig::default(),
            "import-left",
        )
        .await
    });
    let right = tokio::spawn(async move {
        import_events_if_absent(
            &store_b,
            &ConfigActor::Bootstrap,
            &EventsConfig {
                retention_days: 9,
                dead_letter_retention_days: 30,
                concurrency: 2,
            },
            "import-right",
        )
        .await
    });
    let left = left.await.unwrap().unwrap();
    let right = right.await.unwrap().unwrap();
    assert_eq!(left.revision, 1);
    assert_eq!(right.revision, 1);
    assert_eq!(left.body, right.body);
    let check = file_store(&path).await;
    assert_eq!(audit_count(&check, EVENTS_NAMESPACE).await.unwrap(), 1);
    assert_eq!(change_count(&check, EVENTS_NAMESPACE).await.unwrap(), 1);
    let rows = configuration_documents::Entity::find()
        .all(check.db())
        .await
        .unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row.namespace == EVENTS_NAMESPACE)
            .count(),
        1
    );
}

#[tokio::test]
async fn committed_revision_is_visible_to_a_later_connection() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempdir().unwrap();
    let path = dir.path().join("library.db");
    let files = tempdir().unwrap();
    let store = file_store(&path).await;
    let session = bootstrap_pair(&store, files.path()).await;
    let applied = replace_events(
        &store,
        &operator(),
        session.events.revision,
        &events(6),
        "propagate-events",
    )
    .await
    .unwrap();
    let EventsReplace::Applied(doc) = applied else {
        panic!("{applied:?}");
    };
    drop(store);
    let later = file_store(&path).await;
    let caught_up = load_events(&later).await.unwrap();
    assert_eq!(caught_up.revision, doc.revision);
    assert_eq!(caught_up.body.retention_days, 6);
    assert!(change_count(&later, EVENTS_NAMESPACE).await.unwrap() >= 1);
}

#[tokio::test]
async fn missing_and_wrong_secret_roots_do_not_replace_the_cluster_key() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    let fingerprint = super::secret::load_cluster_row(&store)
        .await
        .unwrap()
        .unwrap()
        .secret_fingerprint;
    std::fs::remove_file(master_key_path(files.path())).unwrap();
    let missing = align_cluster_root(&store, files.path(), None)
        .await
        .expect_err("missing key");
    assert!(
        missing.to_string().contains("secret root missing"),
        "{missing}"
    );
    assert!(!master_key_path(files.path()).exists());
    assert_eq!(
        super::secret::load_cluster_row(&store)
            .await
            .unwrap()
            .unwrap()
            .secret_fingerprint,
        fingerprint
    );

    let other = tempdir().unwrap();
    crate::configure_master_key(other.path()).unwrap();
    std::fs::copy(master_key_path(other.path()), master_key_path(files.path())).unwrap();
    let wrong = align_cluster_root(&store, files.path(), None)
        .await
        .expect_err("wrong key");
    assert!(
        wrong.to_string().contains("secret root mismatch"),
        "{wrong}"
    );
    assert_eq!(
        super::secret::load_cluster_row(&store)
            .await
            .unwrap()
            .unwrap()
            .cluster_id,
        session.cluster_id
    );
    assert_eq!(
        super::secret::load_cluster_row(&store)
            .await
            .unwrap()
            .unwrap()
            .secret_fingerprint,
        fingerprint
    );
    assert!(master_key_path(files.path()).is_file());
}

#[tokio::test]
async fn first_and_concurrent_secret_initialization_keep_one_root() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    assert!(!master_key_path(files.path()).exists());
    let session = bootstrap_pair(&store, files.path()).await;
    let fingerprint = super::secret::load_cluster_row(&store)
        .await
        .unwrap()
        .unwrap()
        .secret_fingerprint;
    let again = align_cluster_root(&store, files.path(), None)
        .await
        .unwrap();
    assert_eq!(again.cluster_id, session.cluster_id);
    assert_eq!(again.secret_fingerprint, fingerprint);
    assert_eq!(
        master_key_fingerprint(&require_master_key(Some(files.path())).unwrap()),
        fingerprint
    );

    let fresh = memory_store().await;
    let dir_a = tempdir().unwrap();
    let dir_b = tempdir().unwrap();
    let store = Arc::new(fresh);
    let left = {
        let store = Arc::clone(&store);
        let path = dir_a.path().to_path_buf();
        tokio::spawn(async move { align_cluster_root(&store, &path, None).await })
    };
    let right = {
        let store = Arc::clone(&store);
        let path = dir_b.path().to_path_buf();
        tokio::spawn(async move { align_cluster_root(&store, &path, None).await })
    };
    let left = left
        .await
        .unwrap()
        .map(|_| ())
        .map_err(|err| err.to_string());
    let right = right
        .await
        .unwrap()
        .map(|_| ())
        .map_err(|err| err.to_string());
    let wins = u32::from(left.is_ok()) + u32::from(right.is_ok());
    assert_eq!(wins, 1, "left={left:?} right={right:?}");
    let row = super::secret::load_cluster_row(&store)
        .await
        .unwrap()
        .unwrap();
    let winner_dir = if left.is_ok() {
        dir_a.path()
    } else {
        dir_b.path()
    };
    let loser_dir = if left.is_ok() {
        dir_b.path()
    } else {
        dir_a.path()
    };
    assert!(master_key_path(winner_dir).is_file());
    assert!(
        !master_key_path(loser_dir).exists(),
        "losing initializer must not keep a second master.key"
    );
    let winner = align_cluster_root(&store, winner_dir, None).await.unwrap();
    assert_eq!(winner.secret_fingerprint, row.secret_fingerprint);
    assert_eq!(
        master_key_fingerprint(&require_master_key(Some(winner_dir)).unwrap()),
        row.secret_fingerprint
    );
}

#[tokio::test]
async fn single_host_sqlite_bootstrap_is_one_host_and_one_events_document() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let session = bootstrap_pair(&store, files.path()).await;
    assert!(session.host.compatible);
    assert!(session.host.schema_state.starts_with("unreleased@base0+"));
    assert_eq!(session.events.namespace, EVENTS_NAMESPACE);
    assert_eq!(session.events.scope_type, CLUSTER_SCOPE_TYPE);
    assert_eq!(session.events.scope_id, CLUSTER_SCOPE_ID);
    assert_eq!(session.host_runtime.scope_id, session.host.host_id);
    assert_eq!(
        super::identity::list_host_ids(&store).await.unwrap().len(),
        1
    );
    assert!(master_key_path(files.path()).is_file());
    assert!(host_identity_path(files.path()).is_file());
}

fn postgres_tests_enabled() -> bool {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL")
        .ok()
        .filter(|value| !value.trim().is_empty());
    if url.is_some() {
        return true;
    }
    assert!(
        std::env::var("BOOKCLERK_REQUIRE_POSTGRES_TESTS")
            .ok()
            .as_deref()
            != Some("1"),
        "BOOKCLERK_TEST_POSTGRES_URL is required when BOOKCLERK_REQUIRE_POSTGRES_TESTS=1"
    );
    false
}

fn postgres_url_with_db(url: &str, db_name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (url, None),
    };
    let trimmed = base.trim_end_matches('/');
    let slash = trimmed
        .rfind('/')
        .expect("BOOKCLERK_TEST_POSTGRES_URL must include a database path");
    let head = &trimmed[..slash];
    match query {
        Some(q) => format!("{head}/{db_name}?{q}"),
        None => format!("{head}/{db_name}"),
    }
}

async fn postgres_stores() -> (LibraryStore, LibraryStore) {
    let url = std::env::var("BOOKCLERK_TEST_POSTGRES_URL").expect("postgres url");
    let db_name = format!("cp_{}", Uuid::new_v4().as_simple());
    let admin = sea_orm::Database::connect(url.as_str())
        .await
        .expect("connect admin");
    let backend = admin.get_database_backend();
    admin
        .execute_raw(sea_orm::Statement::from_string(
            backend,
            format!("CREATE DATABASE {db_name}"),
        ))
        .await
        .expect("create database");
    let target = postgres_url_with_db(&url, &db_name);
    let mut first = sea_orm::ConnectOptions::new(target.clone());
    first.max_connections(4).min_connections(1);
    let mut second = sea_orm::ConnectOptions::new(target);
    second.max_connections(4).min_connections(1);
    let db_a = sea_orm::Database::connect(first).await.expect("pool a");
    crate::apply_host_schema(&db_a).await.expect("schema");
    let db_b = sea_orm::Database::connect(second).await.expect("pool b");
    (
        LibraryStore::from_connection(db_a).with_in_process_sql(),
        LibraryStore::from_connection(db_b).with_in_process_sql(),
    )
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_control_plane_two_pools_share_events_and_reject_one_cas() {
    if !postgres_tests_enabled() {
        return;
    }
    let _guard = master_key_test_lock_async().await;
    let (store_a, store_b) = postgres_stores().await;
    let files_a = tempdir().unwrap();
    let files_b = tempdir().unwrap();
    let session = bootstrap_pair(&store_a, files_a.path()).await;
    std::fs::copy(
        master_key_path(files_a.path()),
        master_key_path(files_b.path()),
    )
    .unwrap();
    let other = bootstrap_pair(&store_b, files_b.path()).await;
    assert_eq!(session.cluster_id, other.cluster_id);
    assert_ne!(session.host.host_id, other.host.host_id);

    let expected = session.events.revision;
    let left = {
        let store = store_a.clone();
        tokio::spawn(async move {
            replace_events(&store, &operator(), expected, &events(8), "pg-cas-a").await
        })
    };
    let right = {
        let store = store_b.clone();
        tokio::spawn(async move {
            replace_events(&store, &operator(), expected, &events(9), "pg-cas-b").await
        })
    };
    let (left, right) = (left.await.unwrap().unwrap(), right.await.unwrap().unwrap());
    let applied = matches!(left, EventsReplace::Applied(_)) as u32
        + matches!(right, EventsReplace::Applied(_)) as u32;
    let conflicts = matches!(left, EventsReplace::Conflict { .. }) as u32
        + matches!(right, EventsReplace::Conflict { .. }) as u32;
    assert_eq!(applied, 1, "left={left:?} right={right:?}");
    assert_eq!(conflicts, 1, "left={left:?} right={right:?}");
    let from_b = load_events(&store_b).await.unwrap();
    let from_a = load_events(&store_a).await.unwrap();
    assert_eq!(from_a, from_b);
    assert_eq!(from_a.revision, expected + 1);
    assert!(from_a.body.retention_days == 8 || from_a.body.retention_days == 9);

    let base = from_a.revision;
    let next = replace_events(&store_a, &operator(), base, &events(11), "pg-seq-a")
        .await
        .unwrap();
    let EventsReplace::Applied(applied_a) = next else {
        panic!("sequential A should apply: {next:?}");
    };
    let later = replace_events(
        &store_b,
        &operator(),
        applied_a.revision,
        &events(12),
        "pg-seq-b",
    )
    .await
    .unwrap();
    let EventsReplace::Applied(applied_b) = later else {
        panic!("sequential B should apply: {later:?}");
    };
    let replay = replace_events(&store_a, &operator(), base, &events(11), "pg-seq-a")
        .await
        .unwrap();
    assert_eq!(
        replay,
        EventsReplace::Replayed {
            revision: applied_a.revision,
        }
    );
    let after = load_events(&store_b).await.unwrap();
    assert_eq!(after.revision, applied_b.revision);
    assert_eq!(after.body.retention_days, 12);
    let changed = replace_events(&store_b, &operator(), base, &events(13), "pg-seq-a")
        .await
        .expect_err("postgres payload mismatch");
    assert!(
        changed.to_string().contains("idempotency conflict"),
        "{changed}"
    );
    assert_eq!(load_events(&store_a).await.unwrap().body.retention_days, 12);
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_initial_import_is_atomic_across_connections() {
    if !postgres_tests_enabled() {
        return;
    }
    let (store_a, store_b) = postgres_stores().await;
    store_a
        .db()
        .execute_unprepared(
            "CREATE OR REPLACE FUNCTION bookclerk_fail_config_audit() RETURNS trigger AS $$
             BEGIN
               RAISE EXCEPTION 'injected';
             END;
             $$ LANGUAGE plpgsql",
        )
        .await
        .expect("fail function");
    store_a
        .db()
        .execute_unprepared(
            "CREATE TRIGGER fail_config_audit BEFORE INSERT ON configuration_audit
             FOR EACH ROW EXECUTE FUNCTION bookclerk_fail_config_audit()",
        )
        .await
        .expect("fail trigger");
    let err = import_events_if_absent(
        &store_a,
        &ConfigActor::Bootstrap,
        &EventsConfig::default(),
        "pg-import-fail",
    )
    .await
    .expect_err("injected audit failure");
    assert!(err.to_string().contains("injected"), "{err}");
    assert!(load_events(&store_b).await.is_err());
    assert_eq!(audit_count(&store_b, EVENTS_NAMESPACE).await.unwrap(), 0);
    assert_eq!(change_count(&store_b, EVENTS_NAMESPACE).await.unwrap(), 0);

    store_a
        .db()
        .execute_unprepared("DROP TRIGGER fail_config_audit ON configuration_audit")
        .await
        .unwrap();
    let left = {
        let store = store_a.clone();
        tokio::spawn(async move {
            import_events_if_absent(
                &store,
                &ConfigActor::Bootstrap,
                &EventsConfig::default(),
                "pg-import-left",
            )
            .await
        })
    };
    let right = {
        let store = store_b.clone();
        tokio::spawn(async move {
            import_events_if_absent(
                &store,
                &ConfigActor::Bootstrap,
                &EventsConfig {
                    retention_days: 9,
                    dead_letter_retention_days: 30,
                    concurrency: 2,
                },
                "pg-import-right",
            )
            .await
        })
    };
    left.await.unwrap().unwrap();
    right.await.unwrap().unwrap();
    assert_eq!(audit_count(&store_a, EVENTS_NAMESPACE).await.unwrap(), 1);
    assert_eq!(change_count(&store_b, EVENTS_NAMESPACE).await.unwrap(), 1);
    assert_eq!(
        load_events(&store_a).await.unwrap(),
        load_events(&store_b).await.unwrap()
    );
}

#[tokio::test]
async fn heartbeat_persists_fake_cgroup_observations() {
    let _guard = master_key_test_lock_async().await;
    let store = memory_store().await;
    let files = tempdir().unwrap();
    let scratch = files.path().join("cache");
    std::fs::create_dir_all(scratch.join("acquire")).unwrap();
    std::fs::write(scratch.join("acquire").join("part.bin"), vec![7u8; 32]).unwrap();
    let session = bootstrap_pair(&store, files.path()).await;

    let cgroup = tempdir().unwrap();
    std::fs::write(cgroup.path().join("cpu.max"), "100000 100000\n").unwrap();
    std::fs::write(cgroup.path().join("memory.max"), "1073741824\n").unwrap();
    std::fs::write(cgroup.path().join("memory.current"), "4096\n").unwrap();
    std::fs::write(cgroup.path().join("memory.stat"), "anon 2048\nfile 10\n").unwrap();
    let observation =
        super::identity::sample_host_observation(Some(cgroup.path()), files.path(), &scratch);
    let host = super::identity::register_and_heartbeat(
        &store,
        &session.host.host_id,
        &session.cluster_id,
        "incarnation-obs",
        &observation,
    )
    .await
    .unwrap();
    assert_eq!(host.cpu_max_quota_us, Some(100_000));
    assert_eq!(host.cpu_max_period_us, Some(100_000));
    assert_eq!(host.memory_max_bytes, Some(1_073_741_824));
    assert_eq!(host.memory_current_bytes, Some(4096));
    assert_eq!(host.memory_anon_bytes, Some(2048));
    assert_eq!(host.scratch_bytes, Some(32));
    // `filesystem_free_bytes` is implemented on Linux and macOS. Windows has no
    // populated value, so the heartbeat stores None there.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(host.files_dir_free_bytes.is_some());
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    assert!(host.files_dir_free_bytes.is_none());
    assert!(host.logical_cpus.is_some());
    assert_eq!(host.created_at, session.host.created_at);
    assert_eq!(host.incarnation, "incarnation-obs");
}
