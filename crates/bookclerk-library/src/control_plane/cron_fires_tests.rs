//! Cron fire claims: one job per fire across racing nodes, coalescing onto
//! an active fire, and cursor lifecycle.

use super::tests::memory_store;
use super::*;
use crate::models::{JobKind, JobState};

/// Operator actor for instance writes.
fn operator() -> ConfigActor {
    ConfigActor::Operator {
        id: "operator".into(),
    }
}

/// A fresh instance id in `store`.
async fn instance(store: &crate::LibraryStore) -> String {
    create_plugin_instance(store, &operator(), "platform:bookclerk/nightly", "")
        .await
        .expect("instance")
        .id
        .as_str()
        .to_string()
}

/// Marks job `id` succeeded, as a worker finishing it would.
async fn finish(store: &crate::LibraryStore, id: &str) {
    bookclerk_db_exec::execute_canonical(
        store.db(),
        "UPDATE jobs SET state = 'succeeded' WHERE id = ?",
        [id.into()],
    )
    .await
    .expect("finish job");
}

const CRON: &str = "*/30 * * * *";

#[tokio::test]
async fn a_new_cursor_starts_where_it_is_told_and_is_kept() {
    let store = memory_store().await;
    let id = instance(&store).await;
    assert_eq!(load_cron_cursor(&store, &id, CRON).await.unwrap(), None);
    let cursor = ensure_cron_cursor(&store, &id, CRON, 1_800_000)
        .await
        .unwrap();
    assert_eq!(cursor.last_fire_ms, 1_800_000);
    // A second ensure (another node, or a restart) keeps the stored value.
    let again = ensure_cron_cursor(&store, &id, CRON, 9_000_000)
        .await
        .unwrap();
    assert_eq!(again, cursor);
}

#[tokio::test]
async fn one_of_two_racing_claims_wins_and_admits_the_job() {
    let store = memory_store().await;
    let id = instance(&store).await;
    ensure_cron_cursor(&store, &id, CRON, 0).await.unwrap();
    let first = claim_cron_fire(&store, &id, CRON, 0, 1_800_000)
        .await
        .unwrap();
    let CronFireClaim::Claimed {
        job_id: Some(job_id),
    } = first
    else {
        panic!("first claim should admit a job: {first:?}");
    };
    // The second node read the same cursor; its CAS loses.
    assert_eq!(
        claim_cron_fire(&store, &id, CRON, 0, 1_800_000)
            .await
            .unwrap(),
        CronFireClaim::Lost
    );
    let job = store.get_job(&job_id).await.unwrap().expect("job row");
    assert_eq!(job.kind, JobKind::PluginScheduled);
    assert_eq!(job.state, JobState::Pending);
    assert_eq!(job.payload.plugin_instance_id.as_deref(), Some(id.as_str()));
    assert_eq!(job.payload.requires, vec![id.clone()]);
    assert_eq!(job.payload.cron.as_deref(), Some(CRON));
    assert_eq!(job.payload.scheduled_time_ms, Some(1_800_000));
    assert_eq!(
        load_cron_cursor(&store, &id, CRON)
            .await
            .unwrap()
            .map(|c| c.last_fire_ms),
        Some(1_800_000)
    );
}

#[tokio::test]
async fn a_fire_due_while_the_last_is_active_is_skipped() {
    let store = memory_store().await;
    let id = instance(&store).await;
    ensure_cron_cursor(&store, &id, CRON, 0).await.unwrap();
    let CronFireClaim::Claimed {
        job_id: Some(first),
    } = claim_cron_fire(&store, &id, CRON, 0, 1_800_000)
        .await
        .unwrap()
    else {
        panic!("first claim");
    };
    // Still pending: the next fire moves the cursor but adds no job.
    assert_eq!(
        claim_cron_fire(&store, &id, CRON, 1_800_000, 3_600_000)
            .await
            .unwrap(),
        CronFireClaim::Claimed { job_id: None }
    );
    finish(&store, &first).await;
    let third = claim_cron_fire(&store, &id, CRON, 3_600_000, 5_400_000)
        .await
        .unwrap();
    assert!(
        matches!(third, CronFireClaim::Claimed { job_id: Some(ref j) } if *j != first),
        "{third:?}"
    );
}

#[tokio::test]
async fn schedules_and_instances_are_independent() {
    let store = memory_store().await;
    let a = instance(&store).await;
    let b = instance(&store).await;
    for (id, cron) in [(&a, CRON), (&a, "0 3 * * *"), (&b, CRON)] {
        ensure_cron_cursor(&store, id, cron, 0).await.unwrap();
        assert!(matches!(
            claim_cron_fire(&store, id, cron, 0, 60_000).await.unwrap(),
            CronFireClaim::Claimed { job_id: Some(_) }
        ));
    }
}

#[tokio::test]
async fn a_claim_must_advance_and_needs_a_cursor() {
    let store = memory_store().await;
    let id = instance(&store).await;
    assert!(claim_cron_fire(&store, &id, CRON, 60_000, 60_000)
        .await
        .is_err());
    // No cursor row: nothing to swap.
    assert_eq!(
        claim_cron_fire(&store, &id, CRON, 0, 60_000).await.unwrap(),
        CronFireClaim::Lost
    );
}

#[tokio::test]
async fn deleting_the_instance_removes_its_cursors() {
    let store = memory_store().await;
    let id = instance(&store).await;
    ensure_cron_cursor(&store, &id, CRON, 0).await.unwrap();
    let parsed = PluginInstanceId::parse(&id).unwrap();
    delete_plugin_instance(&store, &operator(), &parsed)
        .await
        .unwrap();
    assert_eq!(load_cron_cursor(&store, &id, CRON).await.unwrap(), None);
}
