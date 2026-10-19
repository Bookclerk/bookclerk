//! Host work over the node API: remote claims hand only trusted secrets,
//! fences lost while a Host was away stay lost, and write-back needs a
//! live fence.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter,
};

use super::*;
use crate::entities::{account_links, accounts, cluster_leases, jobs, node_credentials, nodes};
use crate::master_key::master_key_test_lock_async;
use crate::models::UserRole;
use crate::models::{EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobResourceClass};
use crate::secrets::{
    build_sealed_record, get_secret, secret_account_type, secret_kind, unseal_secret, upsert_secret,
};
use crate::store::LibraryStore;

const PLUGIN: &str = "path:file:///tmp/store#store";
const HOST: &str = "host-riley";

const SETTINGS: RemoteClaimSettings = RemoteClaimSettings {
    lease_secs: 60,
    limits: JobClaimLimits {
        class: 8,
        plugin_instance: 8,
    },
};

/// A store on a fresh SQLite file in `dir`, with the host schema and a master key.
async fn store_in(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    crate::configure_master_key(dir).expect("master key");
    LibraryStore::from_connection(db)
}

/// Adds an `accounts` row for `account_id` under [`PLUGIN`].
async fn add_account(store: &LibraryStore, account_id: &str) {
    let now = chrono::Utc::now().to_rfc3339();
    accounts::ActiveModel {
        plugin_key: Set(PLUGIN.into()),
        account_id: Set(account_id.into()),
        marketplace: Set("us".into()),
        label: Set(None),
        scan_enabled: Set(1),
        source: Set("test".into()),
        connection_status: Set("active".into()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(store.db())
    .await
    .unwrap();
}

/// Links `account_id` to `user_id`'s local portal identity.
async fn link(store: &LibraryStore, user_id: i64, account_id: &str) {
    let identity = store
        .ensure_local_portal_identity(user_id, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, account_id, "test")
        .await
        .unwrap();
}

/// Adds the [`HOST`] node, owned by `owner`, with a live credential.
async fn add_host(store: &LibraryStore, owner: i64) {
    let now = chrono::Utc::now().to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(HOST.into()),
        cluster_id: Set("cluster".into()),
        created_at: Set(now.clone()),
        incarnation: Set("test".into()),
        heartbeat_at: Set(now.clone()),
        software_version: Set("1.0.0".into()),
        schema_state: Set("current".into()),
        compatible: Set(1),
        logical_cpus: Set(None),
        cpu_max_quota_us: Set(None),
        cpu_max_period_us: Set(None),
        memory_max_bytes: Set(None),
        memory_current_bytes: Set(None),
        memory_anon_bytes: Set(None),
        files_dir_free_bytes: Set(None),
        scratch_bytes: Set(None),
        target: Set(String::new()),
        labels_json: Set("[]".into()),
        kind: Set("host".into()),
        owner_user_id: Set(Some(owner)),
    }
    .insert(store.db())
    .await
    .unwrap();
    node_credentials::ActiveModel {
        node_id: Set(HOST.into()),
        sign_public_key: Set(vec![1; 32]),
        box_public_key: Set(vec![2; 32]),
        link_id: Set("link".into()),
        enrolled_at: Set(now),
        revoked_at: Set(None),
        last_connected_at: Set(None),
    }
    .insert(store.db())
    .await
    .unwrap();
}

/// Seals `value` as the store token of `account_id` and names its row.
async fn save_token(store: &LibraryStore, account_id: &str, value: &str) -> JobSecretRef {
    let name = format!("{account_id}.auth");
    let record = build_sealed_record(
        value.as_bytes(),
        secret_kind::SOURCE_AUTH,
        PLUGIN,
        secret_account_type::INTEGRATION,
        account_id,
        &name,
    )
    .unwrap();
    upsert_secret(store.db(), &record).await.unwrap();
    JobSecretRef {
        kind: secret_kind::SOURCE_AUTH.into(),
        provider: PLUGIN.into(),
        account_id: account_id.into(),
        name,
    }
}

/// Unseals the current value of `secret`.
async fn read_token(store: &LibraryStore, secret: &JobSecretRef) -> String {
    let record = get_secret(
        store.db(),
        &secret.kind,
        Some(&secret.provider),
        secret_account_type::INTEGRATION,
        Some(&secret.account_id),
        &secret.name,
    )
    .await
    .unwrap()
    .unwrap();
    String::from_utf8(unseal_secret(&record).unwrap()).unwrap()
}

/// Enqueues a `kind` job for `account` and returns its id.
async fn enqueue(store: &LibraryStore, kind: JobKind, account: &str) -> String {
    match store
        .enqueue_job(EnqueueJobSpec {
            kind,
            payload: JobPayload {
                account: Some(account.into()),
                ..Default::default()
            },
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Created { id } => id,
        other => panic!("expected a new job: {other:?}"),
    }
}

/// A Member's Host with Riley's and Sam's accounts and tokens in place.
struct World {
    _dir: tempfile::TempDir,
    store: LibraryStore,
    riley: i64,
    riley_token: JobSecretRef,
    sam_token: JobSecretRef,
}

/// Builds the [`World`]: Riley owns the Host, Sam does not.
async fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path()).await;
    let riley = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    let sam = store
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap()
        .id;
    add_account(&store, "riley-acct").await;
    add_account(&store, "sam-acct").await;
    link(&store, riley, "riley-acct").await;
    link(&store, sam, "sam-acct").await;
    add_host(&store, riley).await;
    let riley_token = save_token(&store, "riley-acct", "riley-token-v1").await;
    let sam_token = save_token(&store, "sam-acct", "sam-token-v1").await;
    World {
        _dir: dir,
        store,
        riley,
        riley_token,
        sam_token,
    }
}

/// Claims the next network job as [`HOST`].
async fn claim(store: &LibraryStore) -> Option<RemoteJobClaim> {
    claim_remote_job(store, HOST, JobResourceClass::Network, SETTINGS)
        .await
        .unwrap()
}

/// Expires the job's lease as if the Host vanished, and reclaims it.
async fn expire_lease(store: &LibraryStore, job_id: &str) {
    let past = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
    jobs::Entity::update_many()
        .col_expr(jobs::Column::LeaseExpiresAt, Expr::value(Some(past)))
        .filter(jobs::Column::Id.eq(job_id))
        .exec(store.db())
        .await
        .unwrap();
    assert_eq!(store.reclaim_expired_leases().await.unwrap(), 1);
    // The vanished holder's permits expire with its lease.
    cluster_leases::Entity::update_many()
        .col_expr(cluster_leases::Column::ExpiresAtMs, Expr::value(0i64))
        .exec(store.db())
        .await
        .unwrap();
    jobs::Entity::update_many()
        .col_expr(
            jobs::Column::RunAfter,
            Expr::value(chrono::Utc::now().to_rfc3339()),
        )
        .filter(jobs::Column::Id.eq(job_id))
        .exec(store.db())
        .await
        .unwrap();
}

/// A Host is handed only its owner's jobs and secrets, and gives its
/// permits back when the job ends.
#[tokio::test]
async fn a_remote_claim_hands_only_the_owners_secrets() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    enqueue(&w.store, JobKind::Scan, "sam-acct").await;
    let riley_job = enqueue(&w.store, JobKind::Scan, "riley-acct").await;

    let claimed = claim(&w.store).await.expect("Riley's job is claimed");
    assert_eq!(claimed.job.id, riley_job);
    assert_eq!(
        claimed.job.lease_owner.as_deref(),
        Some(remote_lease_owner(HOST).as_str())
    );
    assert_eq!(claimed.secrets.len(), 1);
    assert_eq!(claimed.secrets[0].secret_ref, w.riley_token);
    assert_eq!(claimed.secrets[0].value.as_slice(), b"riley-token-v1");
    let debug = format!("{claimed:?}");
    assert!(!debug.contains("riley-token-v1"), "{debug}");
    assert!(claim(&w.store).await.is_none(), "Sam's job is never handed");

    let fence = claimed.job.lease_generation;
    assert_eq!(
        renew_remote_job(&w.store, HOST, &riley_job, fence, SETTINGS, Some("half"))
            .await
            .unwrap(),
        RemoteRenew::Renewed { cancel: false }
    );
    assert_eq!(
        finish_remote_job(
            &w.store,
            HOST,
            &riley_job,
            fence,
            SETTINGS.limits,
            RemoteOutcome::Complete(Some("done"))
        )
        .await
        .unwrap(),
        RemoteFinish::Done
    );
    assert!(
        live_permits(&w.store, &class_semaphore(JobResourceClass::Network), 8)
            .await
            .unwrap()
            .is_empty(),
        "permits are given back at the end"
    );
}

/// A fence reclaimed while the Host was away, presented by another node,
/// or whose account was unlinked, is lost.
#[tokio::test]
async fn fences_lost_while_the_host_was_away_stay_lost() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let first = claim(&w.store).await.unwrap();
    let stale = first.job.lease_generation;

    // A reconnect within the lease presents the fence again and keeps it.
    assert_eq!(
        renew_remote_job(&w.store, HOST, &job_id, stale, SETTINGS, None)
            .await
            .unwrap(),
        RemoteRenew::Renewed { cancel: false }
    );

    // The host stays away past its lease; the job is reclaimed and claimed
    // again under a new generation.
    expire_lease(&w.store, &job_id).await;
    assert_eq!(
        renew_remote_job(&w.store, HOST, &job_id, stale, SETTINGS, None)
            .await
            .unwrap(),
        RemoteRenew::Lost,
        "a reclaimed fence is lost on resume"
    );
    let second = claim(&w.store).await.expect("claimed again");
    assert!(second.job.lease_generation > stale);
    for outcome in [
        RemoteOutcome::Complete(None),
        RemoteOutcome::Fail {
            kind: "x",
            message: "y",
        },
    ] {
        assert_eq!(
            finish_remote_job(&w.store, HOST, &job_id, stale, SETTINGS.limits, outcome)
                .await
                .unwrap(),
            RemoteFinish::Lost
        );
    }
    // Another node cannot present the Host's fence.
    assert_eq!(
        renew_remote_job(
            &w.store,
            "other-node",
            &job_id,
            second.job.lease_generation,
            SETTINGS,
            None
        )
        .await
        .unwrap(),
        RemoteRenew::Lost
    );
    // Unlinking the account mid-job loses the fence on the next renewal.
    account_links::Entity::delete_many()
        .exec(w.store.db())
        .await
        .unwrap();
    assert_eq!(
        renew_remote_job(
            &w.store,
            HOST,
            &job_id,
            second.job.lease_generation,
            SETTINGS,
            None
        )
        .await
        .unwrap(),
        RemoteRenew::Lost
    );
    let _ = w.riley;
}

/// Write-back needs a live fence, a secret handed under it, a valid value
/// and a node that is still trusted.
#[tokio::test]
async fn write_back_needs_a_live_fence_and_a_handed_secret() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.unwrap();
    let fence = claimed.job.lease_generation;

    let write = |secret: JobSecretRef, generation: i64, value: &'static str| {
        let store = w.store.clone();
        let job_id = job_id.clone();
        async move {
            write_back_job_secret(&store, HOST, &job_id, generation, &secret, value.as_bytes())
                .await
                .unwrap()
        }
    };
    assert_eq!(
        write(w.riley_token.clone(), fence, "riley-token-v2").await,
        WriteBack::Written
    );
    assert_eq!(read_token(&w.store, &w.riley_token).await, "riley-token-v2");
    assert_eq!(
        write(w.sam_token.clone(), fence, "stolen").await,
        WriteBack::Refused("secret_not_handed")
    );
    assert_eq!(read_token(&w.store, &w.sam_token).await, "sam-token-v1");
    assert_eq!(
        write(w.riley_token.clone(), fence + 1, "riley-token-v3").await,
        WriteBack::Refused("fence_not_live")
    );
    assert_eq!(
        write(w.riley_token.clone(), fence, "").await,
        WriteBack::Refused("invalid_value")
    );

    // After the job ends the fence is gone, so is write-back.
    assert_eq!(
        finish_remote_job(
            &w.store,
            HOST,
            &job_id,
            fence,
            SETTINGS.limits,
            RemoteOutcome::Fail {
                kind: "store",
                message: "boom"
            }
        )
        .await
        .unwrap(),
        RemoteFinish::Done
    );
    assert_eq!(
        write(w.riley_token.clone(), fence, "riley-token-v4").await,
        WriteBack::Refused("fence_not_live")
    );

    // A revoked node writes nothing back even under a live fence.
    let retry = claim(&w.store).await;
    assert!(retry.is_none(), "the failed job waits out its backoff");
    jobs::Entity::update_many()
        .col_expr(
            jobs::Column::RunAfter,
            Expr::value(chrono::Utc::now().to_rfc3339()),
        )
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(w.store.db())
        .await
        .unwrap();
    let again = claim(&w.store).await.expect("claimed after backoff");
    node_credentials::Entity::update_many()
        .col_expr(
            node_credentials::Column::RevokedAt,
            Expr::value(Some(chrono::Utc::now().to_rfc3339())),
        )
        .exec(w.store.db())
        .await
        .unwrap();
    assert_eq!(
        write(
            w.riley_token.clone(),
            again.job.lease_generation,
            "riley-token-v5"
        )
        .await,
        WriteBack::Refused("node_untrusted")
    );
    assert_eq!(read_token(&w.store, &w.riley_token).await, "riley-token-v2");
}

/// An acquire cannot be completed over the node API; only its upload completes it.
#[tokio::test]
async fn an_acquire_completes_only_through_its_upload() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let claimed = claim(&w.store).await.expect("acquire claimed");
    assert_eq!(claimed.job.id, job_id);
    assert_eq!(
        finish_remote_job(
            &w.store,
            HOST,
            &job_id,
            claimed.job.lease_generation,
            SETTINGS.limits,
            RemoteOutcome::Complete(None)
        )
        .await
        .unwrap(),
        RemoteFinish::Refused("acquire_completes_on_upload")
    );
}

/// A secret created for the account after the handoff cannot be written back.
#[tokio::test]
async fn write_back_accepts_only_the_secrets_handed_under_that_fence() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.unwrap();
    let fence = claimed.job.lease_generation;
    assert_eq!(claimed.secrets.len(), 1);

    // A second token is created for the same account after the handoff:
    // it is one of the account's secrets now, but the Host never got it.
    let late = JobSecretRef {
        name: "riley-acct.refresh".into(),
        ..w.riley_token.clone()
    };
    let record = build_sealed_record(
        b"late-v1",
        &late.kind,
        &late.provider,
        secret_account_type::INTEGRATION,
        &late.account_id,
        &late.name,
    )
    .unwrap();
    upsert_secret(w.store.db(), &record).await.unwrap();
    assert!(job_secret_refs(&w.store, &claimed.job)
        .await
        .unwrap()
        .contains(&late));

    assert_eq!(
        write_back_job_secret(&w.store, HOST, &job_id, fence, &late, b"overwritten")
            .await
            .unwrap(),
        WriteBack::Refused("secret_not_handed")
    );
    assert_eq!(read_token(&w.store, &late).await, "late-v1");
    assert_eq!(
        write_back_job_secret(&w.store, HOST, &job_id, fence, &w.riley_token, b"riley-v2")
            .await
            .unwrap(),
        WriteBack::Written
    );
}

/// A renewal that finds a permit taken over answers `Lost` without pushing
/// the job's reclaim out.
#[tokio::test]
async fn a_renewal_that_is_lost_does_not_extend_the_lease() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.unwrap();
    let fence = claimed.job.lease_generation;

    // The lease is close to running out, and another holder took the
    // job's permits over.
    let soon = chrono::Utc::now() + chrono::Duration::seconds(5);
    jobs::Entity::update_many()
        .col_expr(
            jobs::Column::LeaseExpiresAt,
            Expr::value(Some(soon.to_rfc3339())),
        )
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(w.store.db())
        .await
        .unwrap();
    cluster_leases::Entity::update_many()
        .col_expr(cluster_leases::Column::Holder, Expr::value("someone-else"))
        .exec(w.store.db())
        .await
        .unwrap();

    assert_eq!(
        renew_remote_job(&w.store, HOST, &job_id, fence, SETTINGS, None)
            .await
            .unwrap(),
        RemoteRenew::Lost
    );
    let job = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert!(
        job.lease_expires_at.unwrap() <= soon,
        "a lost renewal must not push the reclaim out"
    );
}

/// A released claim is claimable again at once, with no attempt spent.
#[tokio::test]
async fn a_released_claim_is_given_back_unspent() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.unwrap();
    let fence = claimed.job.lease_generation;

    assert!(
        release_remote_job(&w.store, HOST, &job_id, fence, SETTINGS.limits)
            .await
            .unwrap()
    );
    let job = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(job.state, crate::models::JobState::Pending);
    assert_eq!(job.attempt_count, 0, "no attempt is spent");
    assert!(
        live_permits(&w.store, &class_semaphore(JobResourceClass::Network), 8)
            .await
            .unwrap()
            .is_empty(),
        "the permits are given back"
    );
    assert_eq!(
        write_back_job_secret(&w.store, HOST, &job_id, fence, &w.riley_token, b"late")
            .await
            .unwrap(),
        WriteBack::Refused("fence_not_live")
    );
    assert!(
        !release_remote_job(&w.store, HOST, &job_id, fence, SETTINGS.limits)
            .await
            .unwrap(),
        "a released fence is not live"
    );
    let again = claim(&w.store).await.expect("claimable again at once");
    assert!(again.job.lease_generation > fence);
}

/// Runs host batches on an open connection, as a guest that has no
/// interactive transactions (D1) does, first applying `before` once: a
/// write another node committed between the write-back's checks and its
/// batch.
struct RacingBatchExec {
    /// Same file the store already migrated.
    db: sea_orm::DatabaseConnection,
    /// SQL run once ahead of the next batch.
    before: Mutex<Option<&'static str>>,
    /// How many batches were sent.
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl crate::TypedAtomicExec for RacingBatchExec {
    async fn execute_typed(
        &self,
        envelope: bookclerk_db_exec::AdapterExecuteRequest,
    ) -> std::result::Result<bookclerk_plugin_abi::ExecuteReply, bookclerk_plugin_abi::PluginError>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let before = self.before.lock().unwrap().take();
        if let Some(sql) = before {
            self.db
                .execute_unprepared(sql)
                .await
                .map_err(|err| bookclerk_plugin_abi::PluginError::internal(err.to_string()))?;
        }
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

/// Write-back runs as one host batch with no `BEGIN`, and its guards hold
/// inside that batch: a reclaim or a credential revocation committed after
/// the checks but before the write leaves the token untouched.
#[tokio::test]
async fn write_back_is_one_guarded_batch_without_begin() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.unwrap();
    let fence = claimed.job.lease_generation;

    let calls = Arc::new(AtomicUsize::new(0));
    let exec = Arc::new(RacingBatchExec {
        db: w.store.db().clone(),
        before: Mutex::new(None),
        calls: calls.clone(),
    });
    let store = w.store.clone().with_typed_exec(exec.clone());
    crate::inject_begin_failures(8);
    let write = |value: &'static [u8]| {
        let store = store.clone();
        let job_id = job_id.clone();
        let secret = w.riley_token.clone();
        async move {
            write_back_job_secret(&store, HOST, &job_id, fence, &secret, value)
                .await
                .unwrap()
        }
    };

    assert_eq!(write(b"riley-token-v2").await, WriteBack::Written);
    assert_eq!(read_token(&w.store, &w.riley_token).await, "riley-token-v2");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // The Host's credential is revoked just before the batch.
    *exec.before.lock().unwrap() =
        Some("UPDATE node_credentials SET revoked_at = '2026-01-01T00:00:00Z'");
    assert_eq!(
        write(b"riley-token-v3").await,
        WriteBack::Refused("fence_not_live")
    );
    assert_eq!(read_token(&w.store, &w.riley_token).await, "riley-token-v2");
    w.store
        .db()
        .execute_unprepared("UPDATE node_credentials SET revoked_at = NULL")
        .await
        .unwrap();

    // The job is reclaimed just before the batch.
    *exec.before.lock().unwrap() = Some(
        "UPDATE jobs SET state = 'pending', lease_owner = NULL, \
         lease_generation = lease_generation + 1",
    );
    assert_eq!(
        write(b"riley-token-v4").await,
        WriteBack::Refused("fence_not_live")
    );
    assert_eq!(read_token(&w.store, &w.riley_token).await, "riley-token-v2");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

/// Permits taken under a higher limit are still found after the limit is
/// lowered (or on an owner node configured lower), so the fence stays live
/// and finishing gives them back.
#[tokio::test]
async fn permits_above_a_lowered_limit_are_still_held() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let semaphore = class_semaphore(JobResourceClass::Network);
    let far = chrono::Utc::now().timestamp_millis() + 3_600_000;
    for slot in 0..3 {
        cluster_leases::ActiveModel {
            name: Set(semaphore_slot_lease_name(&semaphore, slot)),
            holder: Set("someone-else".into()),
            token: Set(1),
            expires_at_ms: Set(far),
            renewed_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(w.store.db())
        .await
        .unwrap();
    }
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.expect("a free slot above the others");
    let fence = claimed.job.lease_generation;

    let lowered = RemoteClaimSettings {
        limits: JobClaimLimits {
            class: 1,
            ..SETTINGS.limits
        },
        ..SETTINGS
    };
    assert_eq!(
        renew_remote_job(&w.store, HOST, &job_id, fence, lowered, None)
            .await
            .unwrap(),
        RemoteRenew::Renewed { cancel: false }
    );
    assert_eq!(
        finish_remote_job(
            &w.store,
            HOST,
            &job_id,
            fence,
            lowered.limits,
            RemoteOutcome::Complete(None)
        )
        .await
        .unwrap(),
        RemoteFinish::Done
    );
    let live = live_permits(&w.store, &semaphore, MAX_SEMAPHORE_LIMIT)
        .await
        .unwrap();
    assert_eq!(live.len(), 3, "only the other holder's permits remain");
    assert!(live.iter().all(|p| p.lease.holder == "someone-else"));
}
