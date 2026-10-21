//! Singleton maintenance duties across two connections (two nodes).

use std::time::Duration;

use sea_orm::{ActiveValue::Set, EntityTrait};

use super::*;
use crate::error::LibraryError;
use crate::store::LibraryStore;

async fn file_store(path: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(path)
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    LibraryStore::from_connection(db)
}

/// Writes `count` already-expired KV rows.
async fn expired_kv(store: &LibraryStore, prefix: &str, count: usize) {
    for i in 0..count {
        crate::entities::plugin_kv::Entity::insert(crate::entities::plugin_kv::ActiveModel {
            plugin_key: Set("path:file:///tmp/kv#kv".into()),
            namespace: Set("cache".into()),
            key: Set(format!("{prefix}-{i}")),
            value: Set("v".into()),
            metadata: Set(String::new()),
            expires_at: Set("2000-01-01T00:00:00Z".into()),
            updated_at: Set("2000-01-01T00:00:00Z".into()),
        })
        .exec(store.db())
        .await
        .unwrap();
    }
}

/// One maintenance pass as a daemon runs it: poll, fence, prune.
async fn kv_pass(duty: &mut SingletonDuty, store: &LibraryStore) -> Option<u64> {
    let mut fence = duty.poll(store).await.unwrap()?;
    fence.check().await.unwrap();
    let pruned = store.kv_prune_expired().await.unwrap();
    duty.mark_ran();
    Some(pruned)
}

#[tokio::test]
async fn one_node_prunes_per_interval() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let a = file_store(&db).await;
    let b = file_store(&db).await;
    let ttl = Duration::from_secs(60);
    let interval = Duration::from_secs(3600);
    let mut duty_a = SingletonDuty::new(KV_EXPIRY_LEASE, "node-a", ttl, interval);
    let mut duty_b = SingletonDuty::new(KV_EXPIRY_LEASE, "node-b", ttl, interval);

    expired_kv(&a, "first", 3).await;
    // Both nodes tick; only the lease holder prunes.
    assert_eq!(kv_pass(&mut duty_a, &a).await, Some(3));
    assert_eq!(kv_pass(&mut duty_b, &b).await, None);

    // Within the interval neither node prunes again, though both keep ticking
    // and the holder keeps renewing.
    expired_kv(&b, "second", 2).await;
    for _ in 0..3 {
        assert_eq!(kv_pass(&mut duty_a, &a).await, None);
        assert_eq!(kv_pass(&mut duty_b, &b).await, None);
    }
    let lease = load_lease(&b, KV_EXPIRY_LEASE).await.unwrap().unwrap();
    assert_eq!((lease.holder.as_str(), lease.token), ("node-a", 1));

    // Once the interval passes, the same holder runs it once more.
    let mut short_a = SingletonDuty::new(KV_EXPIRY_LEASE, "node-a", ttl, Duration::ZERO);
    assert_eq!(kv_pass(&mut short_a, &a).await, Some(2));
    assert_eq!(kv_pass(&mut duty_b, &b).await, None);
}

#[tokio::test]
async fn the_lease_passes_on_when_the_holder_dies_and_the_stale_holder_is_fenced() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let a = file_store(&db).await;
    let b = file_store(&db).await;
    let ttl = Duration::from_millis(200);
    let interval = Duration::from_secs(3600);
    let mut duty_a = SingletonDuty::new(JOB_RETENTION_LEASE, "node-a", ttl, interval);
    let mut duty_b = SingletonDuty::new(JOB_RETENTION_LEASE, "node-b", ttl, interval);

    let mut stale = duty_a.poll(&a).await.unwrap().expect("node a takes it");
    assert_eq!(stale.lease().token, 1);
    duty_a.mark_ran();
    assert!(duty_b.poll(&b).await.unwrap().is_none());

    // Node A stalls (or dies) past the TTL; node B's next tick takes over
    // with a larger fencing token and runs at once.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut fence_b = duty_b.poll(&b).await.unwrap().expect("handover");
    assert_eq!(fence_b.lease().token, 2);
    assert_eq!(fence_b.lease().holder, "node-b");
    fence_b.check().await.unwrap();

    // Node A wakes up holding its old fence: the check refuses it before any
    // destructive work, and its own next tick finds the lease taken.
    let refused = stale.check().await.expect_err("stale holder");
    assert!(matches!(refused, LibraryError::Conflict(_)), "{refused}");
    assert!(duty_a.poll(&a).await.unwrap().is_none());
    let lease = load_lease(&a, JOB_RETENTION_LEASE).await.unwrap().unwrap();
    assert_eq!((lease.holder.as_str(), lease.token), ("node-b", 2));
}

#[tokio::test]
async fn an_expired_fence_is_refused_even_before_anyone_takes_over() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    let a = file_store(&db).await;
    let mut duty = SingletonDuty::new(
        EVENT_RETENTION_LEASE,
        "node-a",
        Duration::from_millis(50),
        Duration::from_secs(3600),
    );
    let mut fence = duty.poll(&a).await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(fence.check().await.is_err());
    // Re-polling retakes the expired lease as a takeover (a new token); the
    // duty runs at once.
    let again = duty.poll(&a).await.unwrap().expect("retaken");
    assert_eq!(again.lease().token, 2);
    assert!(fence.check().await.is_err(), "the old token stays fenced");
    assert_eq!(
        list_leases(&a)
            .await
            .unwrap()
            .iter()
            .map(|lease| lease.name.as_str())
            .collect::<Vec<_>>(),
        // The schema migration that created the database left its row too.
        vec![EVENT_RETENTION_LEASE, MIGRATION_LEASE]
    );
}

#[tokio::test]
async fn a_replacement_with_the_same_holder_id_fences_the_stalled_process() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("library.db");
    // Two daemon processes sharing one node identity.
    let old = file_store(&db).await;
    let new = file_store(&db).await;
    let ttl = Duration::from_millis(100);
    let lease = acquire_lease(&old, SESSION_EXPIRY_LEASE, "node-a", ttl)
        .await
        .unwrap()
        .unwrap();
    let mut stalled = LeaseFence::new(&old, lease, ttl);
    // The old process pauses past expiry; its replacement reacquires.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let taken = acquire_lease(
        &new,
        SESSION_EXPIRY_LEASE,
        "node-a",
        Duration::from_secs(60),
    )
    .await
    .unwrap()
    .expect("replacement takes the expired lease");
    assert_eq!(taken.token, 2, "an expired lease is a takeover");
    let refused = stalled.check().await.expect_err("stalled process fenced");
    assert!(matches!(refused, LibraryError::Conflict(_)), "{refused}");
    let mut current = LeaseFence::new(&new, taken, Duration::from_secs(60));
    current.check().await.expect("replacement holds it");
}
