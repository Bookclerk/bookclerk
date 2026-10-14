//! SeaORM autoincrement inserts through the production `RpcDatabaseProxy`.
//!
//! The typed DB ABI carries no last-insert-id (D1 / Postgres portability), so
//! `RpcDatabaseProxy::execute` reads identity keys back with `RETURNING`.
//! Without that, `ActiveModel::insert` writes the row and then fails
//! `find_by_id(0)` with "Failed to find inserted item", and
//! `Entity::insert(..).exec` reports `last_insert_id == 0`.
//!
//! These tests open the library exactly as `bookclerkd` does
//! ([`open_library_store`] over a spawned database guest) and drive the insert
//! / update paths behind operator login and impersonation.

#![allow(clippy::missing_docs_in_private_items)]

use bookclerk_library::entities::operator_sessions;
use bookclerk_library::{LibraryStore, UserRole};
use chrono::{Duration, Utc};
use sea_orm::{ActiveValue::NotSet, EntityTrait, Set};

use super::database::{open_library_store, DatabaseRegistry, ExternalDatabase};
use super::rpc_proxy_like::{
    approve_guest, create_disposable_postgres_url, guest_config, missing_guest,
    postgres_plugin_tests_enabled, stage_first_party_guest, StagedGuest,
};

/// Spawns the staged guest and opens the host library through it.
async fn open_store(
    staged: &StagedGuest,
    plugin_id: &str,
    postgres_url: Option<String>,
) -> LibraryStore {
    let config = guest_config(staged, plugin_id, postgres_url);
    approve_guest(&config, &staged.plugin);
    let ext = ExternalDatabase::spawn(&staged.plugin, &config)
        .await
        .unwrap_or_else(|err| panic!("spawn {plugin_id}: {err}"));
    open_library_store(&config, &DatabaseRegistry::with_active(ext))
        .await
        .unwrap_or_else(|err| panic!("open library store through {plugin_id}: {err}"))
}

fn operator_session(token_hash: &str) -> operator_sessions::ActiveModel {
    let now = Utc::now();
    operator_sessions::ActiveModel {
        id: NotSet,
        token_hash: Set(token_hash.to_string()),
        expires_at: Set((now + Duration::hours(1)).to_rfc3339()),
        created_at: Set(now.to_rfc3339()),
        last_used_at: Set(None),
        elevated_from_user_id: Set(None),
        impersonating_user_id: Set(None),
        user_agent: Set(None),
        device_type: Set(None),
        client_label: Set(None),
    }
}

async fn assert_autoincrement_inserts(store: &LibraryStore) {
    let expires = Utc::now() + Duration::hours(1);

    // `ActiveModel::insert` (operator login): the row must be read back.
    let client = bookclerk_library::SessionClientInfo {
        user_agent: Some("rpc-proxy-insert".into()),
        device_type: "api".into(),
        client_label: "API".into(),
    };
    store
        .insert_operator_session_with_client("login-a", expires, Some(&client))
        .await
        .expect("insert_operator_session_with_client through RpcDatabaseProxy");
    store
        .insert_operator_session_with_client("login-b", expires, None)
        .await
        .expect("second operator session insert");
    let a = store
        .get_operator_session("login-a")
        .await
        .expect("get login-a")
        .expect("login-a persisted");
    let b = store
        .get_operator_session("login-b")
        .await
        .expect("get login-b")
        .expect("login-b persisted");
    assert_ne!(a.id, b.id, "autoincrement ids must be distinct");
    assert_eq!(a.client_label.as_deref(), Some("API"));

    // `Entity::insert(..).exec`: the reported key must name the new row.
    let res = operator_sessions::Entity::insert(operator_session("exec-c"))
        .exec(store.db())
        .await
        .expect("Entity::insert(..).exec through RpcDatabaseProxy");
    assert!(
        res.last_insert_id > b.id,
        "last_insert_id {} must be the new autoincrement key (> {})",
        res.last_insert_id,
        b.id
    );
    let row = operator_sessions::Entity::find_by_id(res.last_insert_id)
        .one(store.db())
        .await
        .expect("find by last_insert_id")
        .expect("row at last_insert_id");
    assert_eq!(row.token_hash, "exec-c");

    // Impersonation: user insert, `ActiveModel::update`, portal identity and
    // portal session inserts.
    let user = store
        .create_user(UserRole::Member, Some("Reader"), None)
        .await
        .expect("create user through RpcDatabaseProxy");
    assert!(user.id > 0, "user id must be the generated key");
    let session = store
        .set_operator_session_impersonating("login-a", Some(user.id))
        .await
        .expect("set impersonation through RpcDatabaseProxy");
    assert_eq!(session.id, a.id);
    assert_eq!(session.impersonating_user_id, Some(user.id));
    let identity = store
        .ensure_local_portal_identity(user.id, Some("Reader"))
        .await
        .expect("ensure local portal identity through RpcDatabaseProxy");
    assert!(
        identity.id > 0,
        "portal identity id must be the generated key"
    );
    assert_eq!(identity.user_id, Some(user.id));
    store
        .insert_portal_session("portal-a", identity.id, expires)
        .await
        .expect("insert portal session through RpcDatabaseProxy");

    // `ON CONFLICT … DO UPDATE` with a natural key: insert, then update.
    for _ in 0..2 {
        store
            .upsert_event_subscriber_at("node-a", "plugin-a", &[], true, Utc::now())
            .await
            .expect("upsert event subscriber through RpcDatabaseProxy");
    }
}

#[tokio::test]
async fn production_rpc_proxy_returns_autoincrement_ids_through_sqlite_guest() {
    let _live = crate::authority::test_live_lock();
    let Some(staged) = stage_first_party_guest("sqlite") else {
        missing_guest("bookclerk-plugin-database-sqlite");
        return;
    };
    let store = open_store(&staged, "sqlite", None).await;
    assert_autoincrement_inserts(&store).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and bookclerk-plugin-database-postgres"]
async fn production_rpc_proxy_returns_autoincrement_ids_through_postgres_guest() {
    if !postgres_plugin_tests_enabled() {
        return;
    }
    let _live = crate::authority::test_live_lock();
    let staged = stage_first_party_guest("postgres").unwrap_or_else(|| {
        panic!("postgres plugin binary missing; cargo build -p bookclerk-plugin-database-postgres")
    });
    let url = create_disposable_postgres_url().await;
    let store = open_store(&staged, "postgres", Some(url)).await;
    assert_autoincrement_inserts(&store).await;
}
