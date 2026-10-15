//! Host-node enrollment: mint, redeem, revoke, and the enrollment switch.

use std::time::Duration;

use bookclerk_config::EventsConfig;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use tempfile::TempDir;

use super::*;
use crate::entities::{bookclerk_receipts, domain_events, node_credentials, node_enrollment_links};
use crate::error::LibraryError;
use crate::master_key::master_key_test_lock_async;
use crate::models::{UserRole, UserStatus};
use crate::store::LibraryStore;
use crate::token_hash::hash_token;

async fn file_store(path: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    LibraryStore::from_connection(db)
}

fn operator() -> EnrollmentActor {
    EnrollmentActor::Operator { id: "cli".into() }
}

fn config_operator() -> ConfigActor {
    ConfigActor::Operator { id: "cli".into() }
}

/// A store with a cluster identity, plus its files directory.
async fn cluster() -> (LibraryStore, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = file_store(&dir.path().join("library.db")).await;
    bootstrap(&store, &dir).await;
    (store, dir)
}

async fn bootstrap(store: &LibraryStore, dir: &TempDir) -> ControlPlaneSession {
    bootstrap_control_plane(
        store,
        dir.path(),
        &dir.path().join("cache"),
        None,
        &EventsConfig::default(),
    )
    .await
    .expect("bootstrap")
}

async fn user(store: &LibraryStore, role: UserRole, name: &str) -> i64 {
    store.create_user(role, Some(name), None).await.unwrap().id
}

/// A redeem of `minted` with keys derived from `seed`.
fn redeem_request(minted: &MintedLink, seed: u8) -> RedeemRequest {
    RedeemRequest {
        link_id: minted.link.link_id.clone(),
        secret: minted.secret.clone(),
        sign_public_key: bookclerk_node_proto::sign_public_key(&[seed; NODE_PUBLIC_KEY_LEN])
            .to_vec(),
        box_public_key: bookclerk_node_proto::box_public_key(
            &[seed.wrapping_add(1); NODE_PUBLIC_KEY_LEN],
        )
        .to_vec(),
        software_version: "1.2.3".into(),
    }
}

fn assert_forbidden(err: &LibraryError) {
    assert!(matches!(err, LibraryError::Forbidden(_)), "{err}");
}

#[tokio::test]
async fn a_link_redeems_once_into_a_host_node_owned_by_its_user() {
    let _guard = master_key_test_lock_async().await;
    let (store, _dir) = cluster().await;
    let riley = user(&store, UserRole::Member, "Riley").await;
    let minted = mint_enrollment_link(&store, &operator(), riley, "desk", None)
        .await
        .unwrap();
    assert_eq!(minted.link.for_user_id, riley);
    assert_eq!(minted.link.created_by, "operator:cli");
    assert_eq!(minted.link.state(chrono::Utc::now()), "pending");

    let enrolled = redeem_enrollment_link(&store, &redeem_request(&minted, 7))
        .await
        .unwrap();
    assert!(!enrolled.replayed);
    assert_eq!(enrolled.owner_user_id, riley);
    let node = load_node(&store, &enrolled.node_id).await.unwrap();
    assert_eq!(node.kind, NodeKind::Host);
    assert_eq!(node.owner_user_id, Some(riley));
    assert_eq!(node.cluster_id, enrolled.cluster_id);
    assert_eq!(node.software_version, "1.2.3");
    let credential = load_node_credential(&store, &enrolled.node_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        credential.sign_public_key,
        bookclerk_node_proto::sign_public_key(&[7; 32])
    );
    assert_eq!(
        credential.box_public_key,
        bookclerk_node_proto::box_public_key(&[8; 32])
    );
    assert_eq!(credential.link_id, minted.link.link_id);
    assert!(credential.revoked_at.is_none());

    // A retry with the same secret and keys returns the same node.
    let again = redeem_enrollment_link(&store, &redeem_request(&minted, 7))
        .await
        .unwrap();
    assert!(again.replayed);
    assert_eq!(again.node_id, enrolled.node_id);
    // Different keys cannot reuse the link.
    let other = redeem_enrollment_link(&store, &redeem_request(&minted, 9))
        .await
        .unwrap_err();
    assert_forbidden(&other);
    // Neither can a wrong secret.
    let mut wrong = redeem_request(&minted, 7);
    wrong.secret = EnrollmentSecret::from_text("not-the-secret".into());
    assert_forbidden(&redeem_enrollment_link(&store, &wrong).await.unwrap_err());
    let hosts: Vec<_> = list_nodes(&store)
        .await
        .unwrap()
        .into_iter()
        .filter(|node| node.kind == NodeKind::Host)
        .collect();
    assert_eq!(hosts.len(), 1);

    let links = list_enrollment_links(&store, &operator()).await.unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].state(chrono::Utc::now()), "redeemed");
    assert_eq!(
        links[0].redeemed_node_id.as_deref(),
        Some(enrolled.node_id.as_str())
    );
}

#[tokio::test]
async fn the_database_holds_only_the_secret_hash() {
    let _guard = master_key_test_lock_async().await;
    let (store, _dir) = cluster().await;
    let riley = user(&store, UserRole::Member, "Riley").await;
    let minted = mint_enrollment_link(&store, &operator(), riley, "", None)
        .await
        .unwrap();
    let secret = minted.secret.expose().to_string();
    assert!(secret.len() >= 43, "32 random bytes, base64url");
    assert!(!format!("{minted:?}").contains(&secret));
    let url = minted.url("https://owner.example/", None);
    assert_eq!(
        url.as_str(),
        format!(
            "https://owner.example/enroll#bcn1.{}.{secret}",
            minted.link.link_id
        )
    );
    let parsed = parse_enrollment_token(&url).unwrap();
    assert_eq!(parsed.link_id, minted.link.link_id);
    assert_eq!(parsed.secret.expose(), secret);
    assert_eq!(parsed.spki_pin, None);
    let pinned = parse_enrollment_token(&minted.token(Some("abc_-1"))).unwrap();
    assert_eq!(pinned.spki_pin.as_deref(), Some("abc_-1"));
    assert!(parse_enrollment_token("bcn2.x.y").is_err());

    redeem_enrollment_link(&store, &redeem_request(&minted, 3))
        .await
        .unwrap();
    let row = node_enrollment_links::Entity::find_by_id(minted.link.link_id.clone())
        .one(store.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.token_hash, hash_token(&secret));
    // No stored text anywhere in the rows this flow wrote carries the secret.
    let mut stored = format!("{row:?}");
    for receipt in bookclerk_receipts::Entity::find()
        .all(store.db())
        .await
        .unwrap()
    {
        stored.push_str(&format!("{receipt:?}"));
    }
    for credential in node_credentials::Entity::find()
        .all(store.db())
        .await
        .unwrap()
    {
        stored.push_str(&format!("{credential:?}"));
    }
    assert!(!stored.contains(&secret), "secret leaked into {stored}");
}

#[tokio::test]
async fn an_expired_or_revoked_link_does_not_redeem() {
    let _guard = master_key_test_lock_async().await;
    let (store, _dir) = cluster().await;
    let riley = user(&store, UserRole::Member, "Riley").await;
    let expiring = mint_enrollment_link(&store, &operator(), riley, "", Some(MIN_LINK_TTL))
        .await
        .unwrap();
    node_enrollment_links::Entity::update_many()
        .col_expr(
            node_enrollment_links::Column::ExpiresAt,
            Expr::value("2000-01-01T00:00:00.000000Z"),
        )
        .filter(node_enrollment_links::Column::LinkId.eq(expiring.link.link_id.as_str()))
        .exec(store.db())
        .await
        .unwrap();
    assert_forbidden(
        &redeem_enrollment_link(&store, &redeem_request(&expiring, 1))
            .await
            .unwrap_err(),
    );

    let revoked = mint_enrollment_link(&store, &operator(), riley, "", None)
        .await
        .unwrap();
    let after = revoke_enrollment_link(&store, &operator(), &revoked.link.link_id)
        .await
        .unwrap();
    assert_eq!(after.state(chrono::Utc::now()), "revoked");
    assert_forbidden(
        &redeem_enrollment_link(&store, &redeem_request(&revoked, 1))
            .await
            .unwrap_err(),
    );

    // A link for a user who was disabled since does not redeem either.
    let later = mint_enrollment_link(&store, &operator(), riley, "", None)
        .await
        .unwrap();
    store
        .set_user_status(riley, UserStatus::Disabled)
        .await
        .unwrap();
    assert_forbidden(
        &redeem_enrollment_link(&store, &redeem_request(&later, 1))
            .await
            .unwrap_err(),
    );

    // Ttl bounds.
    store
        .set_user_status(riley, UserStatus::Active)
        .await
        .unwrap();
    for ttl in [
        Duration::from_secs(60),
        MAX_LINK_TTL + Duration::from_secs(1),
    ] {
        let err = mint_enrollment_link(&store, &operator(), riley, "", Some(ttl))
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
    }
    assert!(list_nodes(&store)
        .await
        .unwrap()
        .iter()
        .all(|node| node.kind == NodeKind::Owner));
}

#[tokio::test]
async fn disabling_enrollment_refuses_mint_and_redeem() {
    let _guard = master_key_test_lock_async().await;
    let (store, _dir) = cluster().await;
    let riley = user(&store, UserRole::Member, "Riley").await;
    let owner = user(&store, UserRole::Owner, "Olive").await;
    let admin = user(&store, UserRole::Administrator, "Ada").await;
    let defaults = load_host_enrollment(&store).await.unwrap();
    assert!(defaults.body.enabled);
    assert_eq!(defaults.revision, None);
    let minted = mint_enrollment_link(&store, &operator(), riley, "", None)
        .await
        .unwrap();

    // Only the operator or an Owner flips the switch.
    let member_actor = ConfigActor::Member {
        id: riley.to_string(),
    };
    assert_forbidden(
        &set_host_enrollment_enabled(&store, &member_actor, false)
            .await
            .unwrap_err(),
    );
    assert_forbidden(&verify_cluster_owner(&store, admin).await.unwrap_err());
    let owner_actor = verify_cluster_owner(&store, owner).await.unwrap();
    let off = set_host_enrollment_enabled(&store, &owner_actor, false)
        .await
        .unwrap();
    assert!(!off.body.enabled);
    assert_eq!(off.updated_by, format!("owner:{owner}"));
    // The Owner's actor writes nothing else.
    let events_write = replace_events(
        &store,
        &owner_actor,
        1,
        &EventsSettingsV1 {
            retention_days: 3,
            dead_letter_retention_days: 30,
            concurrency: 1,
        },
        "owner-events",
    )
    .await
    .unwrap_err();
    assert_forbidden(&events_write);

    assert_forbidden(
        &mint_enrollment_link(&store, &operator(), riley, "", None)
            .await
            .unwrap_err(),
    );
    assert_forbidden(
        &redeem_enrollment_link(&store, &redeem_request(&minted, 1))
            .await
            .unwrap_err(),
    );

    let on = set_host_enrollment_enabled(&store, &config_operator(), true)
        .await
        .unwrap();
    assert!(on.body.enabled);
    assert!(on.revision > off.revision);
    let enrolled = redeem_enrollment_link(&store, &redeem_request(&minted, 1))
        .await
        .unwrap();
    assert_eq!(enrolled.owner_user_id, riley);
}

#[tokio::test]
async fn a_user_mints_for_themselves_and_only_an_owner_for_others() {
    let _guard = master_key_test_lock_async().await;
    let (store, _dir) = cluster().await;
    let riley = user(&store, UserRole::Member, "Riley").await;
    let sam = user(&store, UserRole::Member, "Sam").await;
    let admin = user(&store, UserRole::Administrator, "Ada").await;
    let owner = user(&store, UserRole::Owner, "Olive").await;
    let as_user = |user_id| EnrollmentActor::User { user_id };

    let own = mint_enrollment_link(&store, &as_user(riley), riley, "laptop", None)
        .await
        .unwrap();
    assert_eq!(own.link.created_by, format!("user:{riley}"));
    assert_forbidden(
        &mint_enrollment_link(&store, &as_user(riley), sam, "", None)
            .await
            .unwrap_err(),
    );
    // A member probing an unknown user id reads the same refusal.
    assert_forbidden(
        &mint_enrollment_link(&store, &as_user(riley), 999_999, "", None)
            .await
            .unwrap_err(),
    );
    assert_forbidden(
        &mint_enrollment_link(&store, &as_user(admin), sam, "", None)
            .await
            .unwrap_err(),
    );
    let for_sam = mint_enrollment_link(&store, &as_user(owner), sam, "", None)
        .await
        .unwrap();
    assert_eq!(for_sam.link.created_by, format!("owner:{owner}"));

    // Listings and revocation are scoped the same way.
    let mine = list_enrollment_links(&store, &as_user(riley))
        .await
        .unwrap();
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].link_id, own.link.link_id);
    assert_eq!(
        list_enrollment_links(&store, &as_user(owner))
            .await
            .unwrap()
            .len(),
        2
    );
    let hidden = revoke_enrollment_link(&store, &as_user(riley), &for_sam.link.link_id)
        .await
        .unwrap_err();
    assert!(matches!(hidden, LibraryError::NotFound(_)), "{hidden}");
    revoke_enrollment_link(&store, &as_user(sam), &for_sam.link.link_id)
        .await
        .unwrap();

    // A disabled user mints nothing, even for themselves.
    store
        .set_user_status(riley, UserStatus::Disabled)
        .await
        .unwrap();
    assert_forbidden(
        &mint_enrollment_link(&store, &as_user(riley), riley, "", None)
            .await
            .unwrap_err(),
    );
    // The operator cannot mint for a disabled user.
    let err = mint_enrollment_link(&store, &operator(), riley, "", None)
        .await
        .unwrap_err();
    assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
}

#[tokio::test]
async fn revoking_a_node_marks_its_credential_and_wakes_every_node() {
    let _guard = master_key_test_lock_async().await;
    let (store, _dir) = cluster().await;
    let riley = user(&store, UserRole::Member, "Riley").await;
    let sam = user(&store, UserRole::Member, "Sam").await;
    let minted = mint_enrollment_link(&store, &operator(), riley, "", None)
        .await
        .unwrap();
    let enrolled = redeem_enrollment_link(&store, &redeem_request(&minted, 5))
        .await
        .unwrap();
    // The owning user reaches its node's runtime document without a role.
    verify_node_owner(&store, riley, &enrolled.node_id)
        .await
        .unwrap();

    let stranger = revoke_node_credential(
        &store,
        &EnrollmentActor::User { user_id: sam },
        &enrolled.node_id,
    )
    .await
    .unwrap_err();
    assert!(matches!(stranger, LibraryError::NotFound(_)), "{stranger}");
    let revoked = revoke_node_credential(
        &store,
        &EnrollmentActor::User { user_id: riley },
        &enrolled.node_id,
    )
    .await
    .unwrap();
    let first = revoked.revoked_at.clone().unwrap();
    let again = revoke_node_credential(&store, &operator(), &enrolled.node_id)
        .await
        .unwrap();
    assert_eq!(again.revoked_at.as_deref(), Some(first.as_str()));
    let messages = domain_events::Entity::find()
        .all(store.db())
        .await
        .unwrap()
        .into_iter()
        .filter(|event| event.payload.contains("node.revoked"))
        .count();
    assert!(messages >= 1, "no node.revoked message published");

    // An owner node has no credential to revoke.
    let owner_node = list_nodes(&store)
        .await
        .unwrap()
        .into_iter()
        .find(|node| node.kind == NodeKind::Owner)
        .unwrap();
    let err = revoke_node_credential(&store, &operator(), &owner_node.node_id)
        .await
        .unwrap_err();
    assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
}

/// Two redeems of one link race on `stores`; exactly one node results.
async fn race_two_redeems(stores: [LibraryStore; 2], minted: &MintedLink) {
    let mut tasks = Vec::new();
    for (index, store) in stores.iter().enumerate() {
        let store = store.clone();
        let req = redeem_request(minted, 10 + u8::try_from(index).unwrap() * 2);
        tasks.push(tokio::spawn(async move {
            redeem_enrollment_link(&store, &req).await
        }));
    }
    let mut won = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => won += 1,
            Err(err) => assert_forbidden(&err),
        }
    }
    assert_eq!(won, 1, "exactly one concurrent redeem wins");
    let hosts = list_nodes(&stores[0])
        .await
        .unwrap()
        .into_iter()
        .filter(|node| node.kind == NodeKind::Host)
        .count();
    assert_eq!(hosts, 1);
    let credentials = node_credentials::Entity::find()
        .filter(node_credentials::Column::LinkId.eq(minted.link.link_id.as_str()))
        .all(stores[1].db())
        .await
        .unwrap()
        .len();
    assert_eq!(credentials, 1);
}

#[tokio::test]
async fn concurrent_redeems_on_two_sqlite_connections_create_one_node() {
    let _guard = master_key_test_lock_async().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let store_a = file_store(&path).await;
    bootstrap(&store_a, &dir).await;
    let store_b = file_store(&path).await;
    let riley = user(&store_a, UserRole::Member, "Riley").await;
    for _ in 0..4 {
        let minted = mint_enrollment_link(&store_a, &operator(), riley, "", None)
            .await
            .unwrap();
        race_two_redeems([store_a.clone(), store_b.clone()], &minted).await;
        // Next round starts from no host nodes.
        store_a
            .db()
            .execute_unprepared("DELETE FROM nodes WHERE kind = 'host'")
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_concurrent_redeems_on_two_pools_create_one_node() {
    let Ok(url) = std::env::var("BOOKCLERK_TEST_POSTGRES_URL") else {
        return;
    };
    if url.trim().is_empty() {
        return;
    }
    let _guard = master_key_test_lock_async().await;
    let db_name = format!("enroll_{}", uuid::Uuid::new_v4().as_simple());
    let admin = sea_orm::Database::connect(url.as_str())
        .await
        .expect("connect admin");
    admin
        .execute_raw(sea_orm::Statement::from_string(
            admin.get_database_backend(),
            format!("CREATE DATABASE {db_name}"),
        ))
        .await
        .expect("create database");
    let (base, query) = url
        .split_once('?')
        .map_or((url.as_str(), None), |(b, q)| (b, Some(q)));
    let head = &base.trim_end_matches('/')[..base.trim_end_matches('/').rfind('/').unwrap()];
    let target = match query {
        Some(q) => format!("{head}/{db_name}?{q}"),
        None => format!("{head}/{db_name}"),
    };
    let db_a = sea_orm::Database::connect(target.as_str()).await.unwrap();
    crate::apply_host_schema(&db_a).await.expect("schema");
    let db_b = sea_orm::Database::connect(target.as_str()).await.unwrap();
    let store_a = LibraryStore::from_connection(db_a).with_in_process_sql();
    let store_b = LibraryStore::from_connection(db_b).with_in_process_sql();
    let dir = tempfile::tempdir().unwrap();
    bootstrap(&store_a, &dir).await;
    let riley = user(&store_a, UserRole::Member, "Riley").await;
    let minted = mint_enrollment_link(&store_a, &operator(), riley, "", None)
        .await
        .unwrap();
    race_two_redeems([store_a, store_b], &minted).await;
}

#[tokio::test]
async fn redeem_refuses_keys_that_are_not_prime_order_points() {
    let _guard = master_key_test_lock_async().await;
    let (store, _dir) = cluster().await;
    let riley = user(&store, UserRole::Member, "Riley").await;
    let minted = mint_enrollment_link(&store, &operator(), riley, "", None)
        .await
        .unwrap();
    // The Ed25519 identity (small order), an X25519 twist point, and a
    // short key.
    let mut identity = [0u8; NODE_PUBLIC_KEY_LEN];
    identity[0] = 1;
    let mut twist = [0u8; NODE_PUBLIC_KEY_LEN];
    twist[0] = 2;
    let good = redeem_request(&minted, 7);
    for (sign, boxed) in [
        (identity.to_vec(), good.box_public_key.clone()),
        (good.sign_public_key.clone(), twist.to_vec()),
        (good.sign_public_key.clone(), vec![9; 31]),
    ] {
        let mut req = good.clone();
        req.sign_public_key = sign;
        req.box_public_key = boxed;
        let err = redeem_enrollment_link(&store, &req).await.unwrap_err();
        assert!(matches!(err, LibraryError::Invalid(_)), "{err}");
    }
    // Refused keys do not spend the link.
    let enrolled = redeem_enrollment_link(&store, &good).await.unwrap();

    // A connection stamps the credential until it is revoked.
    assert!(record_node_connected(&store, &enrolled.node_id)
        .await
        .unwrap());
    let stamped = load_node_credential(&store, &enrolled.node_id)
        .await
        .unwrap()
        .unwrap();
    assert!(stamped.last_connected_at.is_some());
    revoke_node_credential(&store, &operator(), &enrolled.node_id)
        .await
        .unwrap();
    assert!(!record_node_connected(&store, &enrolled.node_id)
        .await
        .unwrap());
    assert!(!record_node_connected(&store, "no-such-node").await.unwrap());
}
