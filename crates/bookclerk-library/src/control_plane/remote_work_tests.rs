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
use crate::entities::{
    account_links, accounts, cluster_leases, jobs, node_credentials, node_upload_sessions, nodes,
};
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
        ..Default::default()
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
    claim_remote_job(store, HOST, JobResourceClass::Network, SETTINGS, None)
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

/// A job that names its account by label, or by its id in another case,
/// is handed that account's secrets: the same accounts the executor and
/// the trust check resolve.
#[tokio::test]
async fn a_job_naming_its_account_by_label_gets_its_secrets() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    accounts::Entity::update_many()
        .col_expr(accounts::Column::Label, Expr::value("Riley Main"))
        .filter(accounts::Column::AccountId.eq("riley-acct"))
        .exec(w.store.db())
        .await
        .unwrap();
    for needle in ["riley main", "RILEY-ACCT"] {
        let job = enqueue(&w.store, JobKind::Scan, needle).await;
        let claimed = claim(&w.store).await.expect("Riley's job is claimed");
        assert_eq!(claimed.job.id, job, "{needle}");
        let refs: Vec<&JobSecretRef> = claimed.secrets.iter().map(|s| &s.secret_ref).collect();
        assert_eq!(refs, vec![&w.riley_token], "{needle}");
        assert_eq!(
            finish_remote_job(
                &w.store,
                HOST,
                &job,
                claimed.job.lease_generation,
                SETTINGS.limits,
                RemoteOutcome::Complete(None)
            )
            .await
            .unwrap(),
            RemoteFinish::Done,
            "{needle}"
        );
    }
}

/// A handoff whose lease ran out while the secrets were read records
/// nothing, so the secrets are not handed out.
#[tokio::test]
async fn a_handoff_after_the_lease_ran_out_records_nothing() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.unwrap();
    let fence = claimed.job.fence().unwrap();
    assert!(remote_work::record_handoff(&w.store, HOST, &fence, &[])
        .await
        .unwrap());
    let past = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
    jobs::Entity::update_many()
        .col_expr(jobs::Column::LeaseExpiresAt, Expr::value(Some(past)))
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(w.store.db())
        .await
        .unwrap();
    assert!(!remote_work::record_handoff(&w.store, HOST, &fence, &[])
        .await
        .unwrap());
}

/// A job pinned to an instance is handed only that plugin's secrets, even
/// when another plugin holds an account with the same id.
#[tokio::test]
async fn a_pinned_job_gets_only_its_plugins_secrets() {
    const OTHER: &str = "path:file:///tmp/other#other";
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let now = chrono::Utc::now().to_rfc3339();
    accounts::ActiveModel {
        plugin_key: Set(OTHER.into()),
        account_id: Set("riley-acct".into()),
        marketplace: Set("us".into()),
        label: Set(None),
        scan_enabled: Set(1),
        source: Set("test".into()),
        connection_status: Set("active".into()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(w.store.db())
    .await
    .unwrap();
    let other = build_sealed_record(
        b"other-token",
        secret_kind::SOURCE_AUTH,
        OTHER,
        secret_account_type::INTEGRATION,
        "riley-acct",
        "riley-acct.auth",
    )
    .unwrap();
    upsert_secret(w.store.db(), &other).await.unwrap();
    let operator = ConfigActor::Operator {
        id: "operator".into(),
    };
    let instance = super::instances::create_plugin_instance(&w.store, &operator, PLUGIN, "")
        .await
        .unwrap();

    let job_for = |instance_id: Option<String>| {
        let store = w.store.clone();
        async move {
            let outcome = store
                .enqueue_job(EnqueueJobSpec {
                    kind: JobKind::Scan,
                    payload: JobPayload {
                        account: Some("riley-acct".into()),
                        plugin_instance_id: instance_id,
                        ..Default::default()
                    },
                    priority: 0,
                    max_attempts: 3,
                    max_pending: 256,
                    run_after: None,
                })
                .await
                .unwrap();
            let EnqueueOutcome::Created { id } = outcome else {
                panic!("expected a new job: {outcome:?}");
            };
            store.get_job(&id).await.unwrap().unwrap()
        }
    };
    let unpinned = job_for(None).await;
    assert_eq!(job_secret_refs(&w.store, &unpinned).await.unwrap().len(), 2);
    let pinned = job_for(Some(instance.id.to_string())).await;
    assert_eq!(
        job_secret_refs(&w.store, &pinned).await.unwrap(),
        vec![w.riley_token.clone()]
    );
    let dangling = job_for(Some("pi_missing".into())).await;
    assert!(job_secret_refs(&w.store, &dangling)
        .await
        .unwrap()
        .is_empty());
}

/// A Host renews the permits its claim took even after the account is
/// relabelled so the job's name no longer reaches it.
#[tokio::test]
async fn a_relabelled_account_keeps_the_claims_permit_renewed() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    // An Administrator's Host is trusted with every job, so the relabel
    // does not cost it the job's trust.
    crate::entities::users::Entity::update_many()
        .col_expr(
            crate::entities::users::Column::Role,
            Expr::value(UserRole::Administrator.as_str()),
        )
        .filter(crate::entities::users::Column::Id.eq(w.riley))
        .exec(w.store.db())
        .await
        .unwrap();
    let label = |value: &'static str| {
        let store = w.store.clone();
        async move {
            accounts::Entity::update_many()
                .col_expr(accounts::Column::Label, Expr::value(value))
                .filter(accounts::Column::AccountId.eq("riley-acct"))
                .exec(store.db())
                .await
                .unwrap();
        }
    };
    label("Riley Main").await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley main").await;
    let claimed = claim(&w.store).await.unwrap();
    let account = account_semaphore(PLUGIN, "riley-acct");
    let before = live_permits(&w.store, &account, 1).await.unwrap();
    assert_eq!(before.len(), 1);

    label("Something Else").await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(
        renew_remote_job(
            &w.store,
            HOST,
            &job_id,
            claimed.job.lease_generation,
            SETTINGS,
            None
        )
        .await
        .unwrap(),
        RemoteRenew::Renewed { cancel: false }
    );
    let after = live_permits(&w.store, &account, 1).await.unwrap();
    assert_eq!(after.len(), 1);
    assert!(
        after[0].lease.expires_at_ms > before[0].lease.expires_at_ms,
        "the account permit the claim took is still renewed"
    );
}

/// Jobs naming one account by id and by label share its permit: the
/// second is not claimed while the first runs.
#[tokio::test]
async fn jobs_naming_one_account_two_ways_share_its_permit() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    accounts::Entity::update_many()
        .col_expr(accounts::Column::Label, Expr::value("Riley Main"))
        .filter(accounts::Column::AccountId.eq("riley-acct"))
        .exec(w.store.db())
        .await
        .unwrap();
    enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    enqueue(&w.store, JobKind::Scan, "riley main").await;
    let first = claim(&w.store).await.expect("one job is claimed");
    assert!(
        claim(&w.store).await.is_none(),
        "the other waits for the account's permit"
    );
    assert_eq!(
        finish_remote_job(
            &w.store,
            HOST,
            &first.job.id,
            first.job.lease_generation,
            SETTINGS.limits,
            RemoteOutcome::Complete(None)
        )
        .await
        .unwrap(),
        RemoteFinish::Done
    );
    assert!(claim(&w.store).await.is_some(), "then the other runs");
}

/// Giving back a claim that resumed a suspended job keeps it a resume:
/// the flag is set again and no attempt is taken back.
#[tokio::test]
async fn a_released_resume_stays_a_resume() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let payload = serde_json::to_string(&JobPayload {
        account: Some("riley-acct".into()),
        resume_pending: true,
        ..Default::default()
    })
    .unwrap();
    jobs::Entity::update_many()
        .col_expr(jobs::Column::Payload, Expr::value(payload))
        .col_expr(jobs::Column::AttemptCount, Expr::value(1i64))
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(w.store.db())
        .await
        .unwrap();
    let claimed = claim(&w.store).await.unwrap();
    assert!(claimed.resumed);
    assert_eq!(
        release_remote_job(
            &w.store,
            HOST,
            &job_id,
            claimed.job.lease_generation,
            claimed.resumed,
            SETTINGS.limits
        )
        .await
        .unwrap(),
        RemoteFinish::Done
    );
    let job = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert!(job.payload.resume_pending, "the resume flag is back");
    assert_eq!(job.attempt_count, 1, "the earlier attempt is kept");
    assert!(claim(&w.store).await.unwrap().resumed);
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

fn filter(kinds: &[&str], plugins: &[&str]) -> JobClaimFilter {
    JobClaimFilter {
        kinds: kinds.iter().map(|k| (*k).to_string()).collect(),
        plugin_keys: plugins.iter().map(|p| (*p).to_string()).collect(),
    }
}

async fn claim_filtered(store: &LibraryStore, filter: &JobClaimFilter) -> Option<RemoteJobClaim> {
    claim_remote_job(
        store,
        HOST,
        JobResourceClass::Network,
        SETTINGS,
        Some(filter),
    )
    .await
    .unwrap()
}

/// A Host's own filter narrows its claims: a plugin it did not consent to
/// and a kind it does not run are never claimed, and the job stays
/// pending, unspent, for a node that can take it.
#[tokio::test]
async fn a_host_claims_only_consented_plugins_and_kinds() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;

    for refused in [
        filter(&["scan"], &[]),
        filter(&["scan"], &["path:file:///tmp/other#other"]),
        filter(&["acquire"], &[PLUGIN]),
        filter(&[], &[PLUGIN]),
    ] {
        assert!(
            claim_filtered(&w.store, &refused).await.is_none(),
            "{refused:?} claimed the job"
        );
    }
    let untouched = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(untouched.state, crate::models::JobState::Pending);
    assert_eq!(untouched.attempt_count, 0);
    assert!(
        live_permits(&w.store, &class_semaphore(JobResourceClass::Network), 8)
            .await
            .unwrap()
            .is_empty(),
        "a filtered-out job takes no permit"
    );

    let claimed = claim_filtered(&w.store, &filter(&["scan"], &[PLUGIN]))
        .await
        .expect("a consented plugin is claimed");
    assert_eq!(claimed.job.id, job_id);
    assert_eq!(claimed.plugin_keys, vec![PLUGIN.to_string()]);
    assert_eq!(claimed.source_plugin_key.as_deref(), Some(PLUGIN));
}

/// An unpinned job whose account id exists under two plugins has no one
/// source a Host could run, so no filter admits it, even one consenting to
/// both plugins: it stays pending for a node that holds the database.
#[tokio::test]
async fn a_job_without_one_source_plugin_passes_no_filter() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    const OTHER: &str = "path:file:///tmp/other#other";
    let now = chrono::Utc::now().to_rfc3339();
    accounts::ActiveModel {
        plugin_key: Set(OTHER.into()),
        account_id: Set("riley-acct".into()),
        marketplace: Set("us".into()),
        label: Set(None),
        scan_enabled: Set(1),
        source: Set("test".into()),
        connection_status: Set("active".into()),
        created_at: Set(now.clone()),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(w.store.db())
    .await
    .unwrap();
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let record = w.store.get_job(&job_id).await.unwrap().unwrap();
    let keys = job_plugin_keys(&w.store, &record).await.unwrap().unwrap();
    assert_eq!(keys.len(), 2, "both plugins hold the account");
    assert_eq!(
        job_source_plugin_key(&w.store, &record).await.unwrap(),
        None
    );
    let both = filter(&["scan"], &[PLUGIN, OTHER]);
    assert!(!both.admits(&w.store, &record).await.unwrap());
    assert!(claim_filtered(&w.store, &both).await.is_none());
    let untouched = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(untouched.state, crate::models::JobState::Pending);
    assert_eq!(untouched.attempt_count, 0, "no attempt is spent");
}

/// A job pinned to an instance but naming no account has one source
/// plugin, yet no filter admits it: a Host is handed one account's
/// secrets, and this job has none to hand. Naming the account admits it.
#[tokio::test]
async fn a_pinned_job_without_an_account_passes_no_filter() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let actor = ConfigActor::Operator { id: "test".into() };
    let instance = create_plugin_instance(&w.store, &actor, PLUGIN, "Pinned")
        .await
        .unwrap();
    let pinned = |account: Option<&str>| JobPayload {
        plugin_instance_id: Some(instance.id.to_string()),
        account: account.map(str::to_string),
        ..Default::default()
    };
    let EnqueueOutcome::Created { id } = w
        .store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Acquire,
            payload: pinned(None),
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap()
    else {
        panic!("expected a new job");
    };
    let mut record = w.store.get_job(&id).await.unwrap().unwrap();
    assert_eq!(
        job_source_plugin_key(&w.store, &record)
            .await
            .unwrap()
            .as_deref(),
        Some(PLUGIN)
    );
    let consented = filter(&["acquire"], &[PLUGIN]);
    assert!(!consented.admits(&w.store, &record).await.unwrap());
    record.payload = pinned(Some("riley-acct"));
    assert!(consented.admits(&w.store, &record).await.unwrap());
}

/// A job naming its account by label, or by its id in another case, is
/// filtered by that account's plugin, as the executor resolves it.
#[tokio::test]
async fn a_filter_resolves_an_account_named_by_label() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    accounts::Entity::update_many()
        .col_expr(accounts::Column::Label, Expr::value("Riley Main"))
        .filter(accounts::Column::AccountId.eq("riley-acct"))
        .exec(w.store.db())
        .await
        .unwrap();
    for needle in ["riley main", "RILEY-ACCT"] {
        let job_id = enqueue(&w.store, JobKind::Scan, needle).await;
        let claimed = claim_filtered(&w.store, &filter(&["scan"], &[PLUGIN]))
            .await
            .unwrap_or_else(|| panic!("{needle}: a consented plugin is claimed"));
        assert_eq!(claimed.job.id, job_id, "{needle}");
        assert_eq!(claimed.plugin_keys, vec![PLUGIN.to_string()], "{needle}");
        finish_remote_job(
            &w.store,
            HOST,
            &job_id,
            claimed.job.lease_generation,
            SETTINGS.limits,
            RemoteOutcome::Complete(None),
        )
        .await
        .unwrap();
    }
}

/// A scan of every account names no plugin, so no filter admits it.
#[tokio::test]
async fn a_job_whose_plugins_are_unknown_passes_no_filter() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job = w
        .store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload::default(),
            priority: 0,
            max_attempts: 3,
            max_pending: 256,
            run_after: None,
        })
        .await
        .unwrap();
    assert!(matches!(job, EnqueueOutcome::Created { .. }));
    let record = w
        .store
        .list_job_claim_candidates(JobResourceClass::Network, None, 8)
        .await
        .unwrap()
        .jobs
        .pop()
        .unwrap();
    assert_eq!(job_plugin_keys(&w.store, &record).await.unwrap(), None);
    assert!(!filter(&["scan"], &[PLUGIN])
        .admits(&w.store, &record)
        .await
        .unwrap());
}

/// A released job goes back to pending without spending its attempt and
/// with its permits given back; the released fence is lost.
#[tokio::test]
async fn a_released_job_returns_unspent() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let claimed = claim(&w.store).await.unwrap();
    let fence = claimed.job.lease_generation;
    assert_eq!(claimed.job.attempt_count, 1);

    assert_eq!(
        release_remote_job(&w.store, HOST, &job_id, fence, false, SETTINGS.limits)
            .await
            .unwrap(),
        RemoteFinish::Done
    );
    let back = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(back.state, crate::models::JobState::Pending);
    assert_eq!(back.attempt_count, 0);
    assert!(
        live_permits(&w.store, &class_semaphore(JobResourceClass::Network), 8)
            .await
            .unwrap()
            .is_empty(),
        "permits are given back"
    );
    assert_eq!(
        release_remote_job(&w.store, HOST, &job_id, fence, false, SETTINGS.limits)
            .await
            .unwrap(),
        RemoteFinish::Lost
    );
    assert_eq!(
        renew_remote_job(&w.store, HOST, &job_id, fence, SETTINGS, None)
            .await
            .unwrap(),
        RemoteRenew::Lost
    );
    let again = claim(&w.store).await.expect("the job is claimable again");
    assert_eq!(again.job.id, job_id);
    assert!(again.job.lease_generation > fence);
}

/// What a Host reads about its deployments is scoped to the deployments
/// that target it: another node's instance, observation and archive are
/// out of reach, a Host cannot claim `healthy`, and a revoked Host reads
/// nothing.
#[tokio::test]
async fn remote_reads_are_scoped_to_the_nodes_own_deployments() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let actor = ConfigActor::Operator { id: "test".into() };
    let url = put_artifact_blob(&w.store, b"archive bytes").await.unwrap();
    let archive = url.strip_prefix("blob:sha256:").unwrap().to_string();
    add_plugin_version(
        &w.store,
        &actor,
        PluginVersion {
            plugin_key: PLUGIN.into(),
            version: "1.0.0".into(),
            manifest_id: "store".into(),
            manifest_toml: String::new(),
            settings: Default::default(),
            artifacts: vec![PluginArtifact {
                target: "any".into(),
                url: url.clone(),
                format: "tar.gz".into(),
                archive_root: ".".into(),
                archive_sha256: archive.clone(),
                payload_root_sha256: "ab".repeat(32),
                executable: "guest".into(),
            }],
            publisher_verified: false,
            grant_json: String::new(),
            coordinate: String::new(),
            installed_at: String::new(),
            installed_by: String::new(),
        },
    )
    .await
    .unwrap();
    activate_version(&w.store, &actor, PLUGIN, "1.0.0", HOST)
        .await
        .unwrap();
    let here = create_plugin_instance(&w.store, &actor, PLUGIN, "")
        .await
        .unwrap();
    let elsewhere = create_plugin_instance(&w.store, &actor, PLUGIN, "")
        .await
        .unwrap();
    let mine = ensure_plugin_deployment(&w.store, &actor, &here.id, HOST, DesiredState::Present)
        .await
        .unwrap();
    let theirs = ensure_plugin_deployment(
        &w.store,
        &actor,
        &elsewhere.id,
        "node-other",
        DesiredState::Present,
    )
    .await
    .unwrap();

    let listed = remote_deployments_for_node(&w.store, HOST).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].deployment.deployment_id, mine.deployment_id);
    assert_eq!(listed[0].version.as_ref().unwrap().version, "1.0.0");
    assert!(remote_instance_for_node(&w.store, HOST, here.id.as_str())
        .await
        .unwrap()
        .is_some_and(|instance| instance.trust != NodeTrust::All));
    assert!(
        remote_instance_for_node(&w.store, HOST, elsewhere.id.as_str())
            .await
            .unwrap()
            .is_none()
    );
    assert!(remote_instance_for_node(&w.store, HOST, "not an id")
        .await
        .unwrap()
        .is_none());
    assert!(artifact_served_to_node(&w.store, HOST, &archive)
        .await
        .unwrap());
    assert!(!artifact_served_to_node(&w.store, HOST, &"cd".repeat(32))
        .await
        .unwrap());
    let observation = |deployment_id, status| RemoteObservation {
        deployment_id,
        status,
        detail: "awaiting_host_consent",
        applied_config_revision: None,
    };
    assert!(record_remote_observation(
        &w.store,
        HOST,
        "inc",
        &observation(&mine.deployment_id, DeploymentStatus::Error)
    )
    .await
    .unwrap());
    assert!(!record_remote_observation(
        &w.store,
        HOST,
        "inc",
        &observation(&theirs.deployment_id, DeploymentStatus::Error)
    )
    .await
    .unwrap());
    assert!(
        !record_remote_observation(
            &w.store,
            HOST,
            "inc",
            &observation(&mine.deployment_id, DeploymentStatus::Healthy)
        )
        .await
        .unwrap(),
        "a host does not claim healthy"
    );

    node_credentials::Entity::update_many()
        .col_expr(
            node_credentials::Column::RevokedAt,
            Expr::value(Some(chrono::Utc::now().to_rfc3339())),
        )
        .filter(node_credentials::Column::NodeId.eq(HOST))
        .exec(w.store.db())
        .await
        .unwrap();
    assert!(remote_deployments_for_node(&w.store, HOST)
        .await
        .unwrap()
        .is_empty());
    assert!(remote_instance_for_node(&w.store, HOST, here.id.as_str())
        .await
        .unwrap()
        .is_none());
    assert!(!artifact_served_to_node(&w.store, HOST, &archive)
        .await
        .unwrap());
}

/// A store row for `account_id` as a guest's scan would produce it.
fn scanned(account_id: &str, product_id: &str) -> crate::store::NewBook {
    crate::store::NewBook {
        uuid: Some("00000000-0000-0000-0000-000000000000".into()),
        product_id: product_id.into(),
        plugin_key: "path:file:///tmp/other#other".into(),
        source: "spoofed".into(),
        account_id: account_id.into(),
        asin: None,
        isbn: None,
        marketplace: "us".into(),
        title: format!("Title {product_id}"),
        authors: None,
        narrators: None,
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: None,
        length_minutes: None,
        is_abridged: false,
        content_kind: "book".into(),
        categories: None,
        subtitle: None,
        published_at: None,
    }
}

/// The library row of `product_id` for `account_id`, if any.
async fn book_row(
    store: &LibraryStore,
    account_id: &str,
    product_id: &str,
) -> Option<crate::models::BookRecord> {
    store
        .list_books(Some(account_id))
        .await
        .unwrap()
        .into_iter()
        .find(|book| book.product_id == product_id)
}

/// A Member's Host returns its scan's rows; the owner writes the rows for
/// the job's account (forcing the plugin and the account's label), and
/// refuses a whole batch carrying a row for another user's account, a
/// malformed row, or another plugin.
#[tokio::test]
async fn a_members_host_writes_scan_rows_only_for_its_own_accounts() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;

    assert_eq!(
        accept_scan_results(
            &w.store,
            HOST,
            &job_id,
            fence,
            PLUGIN,
            vec![scanned("riley-acct", "p1")]
        )
        .await
        .unwrap(),
        ScanAccept::Accepted { upserted: 1 }
    );
    let row = book_row(&w.store, "riley-acct", "p1")
        .await
        .expect("written");
    assert_eq!(row.plugin_key, PLUGIN);
    assert_eq!(row.source, "test", "labelled from the account row");
    assert_ne!(row.uuid, "00000000-0000-0000-0000-000000000000");

    for (rows, reason) in [
        (
            vec![scanned("riley-acct", "p2"), scanned("sam-acct", "s1")],
            "account_not_in_job",
        ),
        (vec![scanned("", "p3")], "invalid_row"),
    ] {
        assert_eq!(
            accept_scan_results(&w.store, HOST, &job_id, fence, PLUGIN, rows)
                .await
                .unwrap(),
            ScanAccept::Refused(reason)
        );
    }
    assert_eq!(
        accept_scan_results(
            &w.store,
            HOST,
            &job_id,
            fence,
            "path:file:///tmp/other#other",
            vec![scanned("riley-acct", "p4")]
        )
        .await
        .unwrap(),
        ScanAccept::Refused("plugin_not_in_job")
    );
    assert!(
        book_row(&w.store, "riley-acct", "p2").await.is_none(),
        "nothing of a refused batch is written"
    );
    assert!(book_row(&w.store, "sam-acct", "s1").await.is_none());

    // Unlinking Riley's account loses the job on the next batch.
    account_links::Entity::delete_many()
        .exec(w.store.db())
        .await
        .unwrap();
    assert_eq!(
        accept_scan_results(
            &w.store,
            HOST,
            &job_id,
            fence,
            PLUGIN,
            vec![scanned("riley-acct", "p5")]
        )
        .await
        .unwrap(),
        ScanAccept::Lost,
        "trust is re-checked on every batch"
    );
    assert!(book_row(&w.store, "riley-acct", "p5").await.is_none());
}

/// A results batch or an upload grant under a fence that expired and was
/// reclaimed is refused, and writes nothing.
#[tokio::test]
async fn a_stale_fence_cannot_write_scan_rows_or_open_uploads() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let stale = claim(&w.store).await.unwrap().job.lease_generation;
    expire_lease(&w.store, &job_id).await;
    let fresh = claim(&w.store).await.unwrap().job.lease_generation;
    assert!(fresh > stale);
    assert_eq!(
        accept_scan_results(
            &w.store,
            HOST,
            &job_id,
            stale,
            PLUGIN,
            vec![scanned("riley-acct", "late")]
        )
        .await
        .unwrap(),
        ScanAccept::Lost
    );
    assert!(book_row(&w.store, "riley-acct", "late").await.is_none());
    finish_remote_job(
        &w.store,
        HOST,
        &job_id,
        fresh,
        SETTINGS.limits,
        RemoteOutcome::Complete(None),
    )
    .await
    .unwrap();

    let acquire = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let stale = claim(&w.store).await.unwrap().job.lease_generation;
    expire_lease(&w.store, &acquire).await;
    assert!(matches!(
        grant_upload(
            &w.store,
            HOST,
            &acquire,
            stale,
            "owner-node",
            &UploadGrantRequest {
                book_uuid: "",
                primary_key: "",
                pdf_key: None,
                files: &[],
                destinations: &[],
            }
        )
        .await
        .unwrap(),
        UploadGrant::Lost
    ));
}

/// A closing upload's finish with `detail`.
fn finish(detail: &str) -> UploadFinish<'_> {
    UploadFinish {
        detail,
        download_episodes: false,
    }
}

/// Opens the closing upload of `job_id` under `fence`.
async fn closing_upload(store: &LibraryStore, job_id: &str, fence: i64) -> String {
    let UploadGrant::Granted { upload_id, .. } = grant_upload(
        store,
        HOST,
        job_id,
        fence,
        "owner",
        &UploadGrantRequest {
            book_uuid: "",
            primary_key: "",
            pdf_key: None,
            files: &[],
            destinations: &[],
        },
    )
    .await
    .unwrap() else {
        panic!("closing upload granted");
    };
    upload_id
}

/// The owner's one `local` destination, its title scope `folder`.
fn local(folder: &str) -> Vec<UploadDestination> {
    vec![UploadDestination {
        id: "local".into(),
        title_folders: vec![folder.into()],
        title_names: None,
    }]
}

/// Grants, receives and commits one title's upload.
async fn upload_title(
    store: &LibraryStore,
    job_id: &str,
    fence: i64,
    book_uuid: &str,
    files: &[bookclerk_node_proto::UploadFile],
) {
    let primary_key = files[0].copies[0].key.as_str();
    let folder = primary_key
        .rsplit_once('/')
        .map_or("", |(folder, _)| folder);
    let UploadGrant::Granted { upload_id, .. } = grant_upload(
        store,
        HOST,
        job_id,
        fence,
        "owner",
        &UploadGrantRequest {
            book_uuid,
            primary_key,
            pdf_key: None,
            files,
            destinations: &local(folder),
        },
    )
    .await
    .unwrap() else {
        panic!("granted");
    };
    for index in 0..files.len() {
        assert!(record_upload_file(store, &upload_id, index).await.unwrap());
    }
    assert_eq!(
        commit_upload(store, &upload_id, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Committed {
            job_completed: false
        }
    );
}

/// The owner decides the job's success: a closing upload is refused while
/// a title of the job is still unacquired (the job keeps running and its
/// permits), and completes the job once every title is.
#[tokio::test]
async fn the_closing_upload_waits_for_every_title() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (first, files) = riley_title(&w.store).await;
    let second = w
        .store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("riley-acct", "t2")
        })
        .await
        .unwrap()
        .uuid;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    upload_title(&w.store, &job_id, fence, &first, &files).await;
    let early = closing_upload(&w.store, &job_id, fence).await;
    assert_eq!(
        commit_upload(&w.store, &early, Some(finish("all done")), SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Refused("titles_remaining"),
        "the host's word is not enough"
    );
    let running = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(running.state, crate::models::JobState::Running);
    assert!(
        !live_permits(&w.store, &class_semaphore(JobResourceClass::Network), 8)
            .await
            .unwrap()
            .is_empty(),
        "the job keeps its permits"
    );
    let mut second_files = files.clone();
    second_files[0].copies[0].key = "Author/Title 2/Title 2.m4b".into();
    upload_title(&w.store, &job_id, fence, &second, &second_files).await;
    let closing = closing_upload(&w.store, &job_id, fence).await;
    assert_eq!(
        commit_upload(
            &w.store,
            &closing,
            Some(finish("all done")),
            SETTINGS.limits
        )
        .await
        .unwrap(),
        UploadCommit::Committed {
            job_completed: true
        }
    );
    assert_eq!(
        w.store.get_job(&job_id).await.unwrap().unwrap().state,
        crate::models::JobState::Succeeded
    );
}

/// True when `token` opens `upload_id`.
async fn token_opens(store: &LibraryStore, upload_id: &str, token: &str) -> bool {
    authorize_upload(store, upload_id, token, false)
        .await
        .unwrap()
        .is_some()
}

/// Sets every upload's token to have expired.
async fn expire_tokens(store: &LibraryStore) {
    let past = (chrono::Utc::now() - chrono::Duration::seconds(5))
        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    node_upload_sessions::Entity::update_many()
        .col_expr(node_upload_sessions::Column::ExpiresAt, Expr::value(past))
        .exec(store.db())
        .await
        .unwrap();
}

/// An upload's token lives on while its fence does: a renewal of the
/// job's lease and an accepted file each extend it, and once the fence is
/// lost a renewal extends nothing.
#[tokio::test]
async fn a_live_fence_keeps_its_upload_token_alive() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let UploadGrant::Granted {
        upload_id, token, ..
    } = grant_upload(
        &w.store,
        HOST,
        &job_id,
        fence,
        "owner",
        &UploadGrantRequest {
            book_uuid: &book_uuid,
            primary_key: "Author/Title/Title.m4b",
            pdf_key: None,
            files: &files,
            destinations: &local("Author/Title"),
        },
    )
    .await
    .unwrap()
    else {
        panic!("granted");
    };
    expire_tokens(&w.store).await;
    assert!(
        !token_opens(&w.store, &upload_id, &token).await,
        "an expired token opens nothing"
    );
    assert_eq!(
        renew_remote_job(&w.store, HOST, &job_id, fence, SETTINGS, None)
            .await
            .unwrap(),
        RemoteRenew::Renewed { cancel: false }
    );
    assert!(
        token_opens(&w.store, &upload_id, &token).await,
        "a renewal extends the token"
    );

    expire_tokens(&w.store).await;
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    assert!(
        token_opens(&w.store, &upload_id, &token).await,
        "an accepted file extends the token"
    );

    expire_tokens(&w.store).await;
    expire_lease(&w.store, &job_id).await;
    assert_eq!(
        renew_remote_job(&w.store, HOST, &job_id, fence, SETTINGS, None)
            .await
            .unwrap(),
        RemoteRenew::Lost
    );
    assert!(
        !token_opens(&w.store, &upload_id, &token).await,
        "a lost fence extends nothing"
    );
}

/// Aborting an upload answers what it had received, so the receiving node
/// can delete those copies; a second abort answers nothing, and a job's
/// uploads are aborted by fence only for that node and generation.
#[tokio::test]
async fn an_abort_answers_the_files_it_had_received() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, mut files) = riley_title(&w.store).await;
    files.push(bookclerk_node_proto::UploadFile {
        copies: vec![bookclerk_node_proto::FileCopy {
            destination: "local".into(),
            key: "Author/Title/cover.jpg".into(),
        }],
        ..files[0].clone()
    });
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let request = UploadGrantRequest {
        book_uuid: &book_uuid,
        primary_key: "Author/Title/Title.m4b",
        pdf_key: None,
        files: &files,
        destinations: &local("Author/Title"),
    };
    let UploadGrant::Granted { upload_id, .. } =
        grant_upload(&w.store, HOST, &job_id, fence, "owner", &request)
            .await
            .unwrap()
    else {
        panic!("granted");
    };
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    // Re-sending the title supersedes the open upload, receipts and all.
    let UploadGrant::Granted {
        upload_id: again,
        superseded,
        ..
    } = grant_upload(&w.store, HOST, &job_id, fence, "owner", &request)
        .await
        .unwrap()
    else {
        panic!("granted again");
    };
    assert_eq!(superseded.len(), 1);
    assert_eq!(superseded[0].upload_id, upload_id);
    assert_eq!(superseded[0].received, [0usize].into_iter().collect());
    assert!(
        !record_upload_file(&w.store, &upload_id, 1).await.unwrap(),
        "an aborted upload records no more files"
    );
    assert!(abort_upload(&w.store, &upload_id).await.unwrap().is_none());

    assert!(record_upload_file(&w.store, &again, 1).await.unwrap());
    assert!(abort_job_uploads(&w.store, "host-other", &job_id, fence)
        .await
        .unwrap()
        .is_empty());
    assert!(abort_job_uploads(&w.store, HOST, &job_id, fence + 1)
        .await
        .unwrap()
        .is_empty());
    let aborted = abort_job_uploads(&w.store, HOST, &job_id, fence)
        .await
        .unwrap();
    assert_eq!(aborted.len(), 1);
    assert_eq!(aborted[0].upload_id, again);
    assert_eq!(aborted[0].received, [1usize].into_iter().collect());
    assert_eq!(
        commit_upload(&w.store, &again, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Refused("upload_aborted")
    );
}

/// The title Riley's acquire uploads, with its one file declared.
async fn riley_title(store: &LibraryStore) -> (String, Vec<bookclerk_node_proto::UploadFile>) {
    let book = store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("riley-acct", "t1")
        })
        .await
        .unwrap();
    let files = vec![bookclerk_node_proto::UploadFile {
        copies: vec![bookclerk_node_proto::FileCopy {
            destination: "local".into(),
            key: "Author/Title/Title.m4b".into(),
        }],
        size: 3,
        sha256: "ab".repeat(32),
        content_type: Some("audio/mp4".into()),
    }];
    (book.uuid, files)
}

/// An upload is granted only for a title of the job, its token opens only
/// that upload, the commit waits for every file, and the title's commit
/// marks the title acquired and publishes `book_acquired` but cannot
/// finish the job; only the closing upload completes it and gives its
/// permits back, and a repeat answers the same.
#[tokio::test]
async fn an_upload_commit_acquires_the_title_and_completes_the_job() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let sam_book = w
        .store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("sam-acct", "s1")
        })
        .await
        .unwrap();
    let destinations = local("Author/Title");
    let sams = UploadGrantRequest {
        book_uuid: &sam_book.uuid,
        primary_key: "Author/Title/Title.m4b",
        pdf_key: None,
        files: &files,
        destinations: &destinations,
    };
    assert!(matches!(
        grant_upload(&w.store, HOST, &job_id, fence, "owner", &sams)
            .await
            .unwrap(),
        UploadGrant::Refused("book_not_in_job")
    ));
    let riley = UploadGrantRequest {
        book_uuid: &book_uuid,
        ..sams
    };
    let bad = UploadGrantRequest {
        primary_key: "elsewhere.m4b",
        ..riley
    };
    assert!(matches!(
        grant_upload(&w.store, HOST, &job_id, fence, "owner", &bad)
            .await
            .unwrap(),
        UploadGrant::Refused("invalid_manifest")
    ));
    let s3 = UploadDestination {
        id: "s3".into(),
        title_folders: vec!["Title".into()],
        title_names: None,
    };
    let two = vec![destinations[0].clone(), s3.clone()];
    let unwritten = UploadGrantRequest {
        destinations: &two,
        ..riley
    };
    assert!(
        matches!(
            grant_upload(&w.store, HOST, &job_id, fence, "owner", &unwritten)
                .await
                .unwrap(),
            UploadGrant::Refused("destinations_changed")
        ),
        "every destination of the receiving node must be written"
    );
    let s3_first = vec![s3, destinations[0].clone()];
    let elsewhere = vec![bookclerk_node_proto::UploadFile {
        copies: vec![bookclerk_node_proto::FileCopy {
            destination: "s3".into(),
            key: "Title/Title.m4b".into(),
        }],
        ..files[0].clone()
    }];
    let off_primary = UploadGrantRequest {
        files: &elsewhere,
        destinations: &destinations,
        ..riley
    };
    assert!(
        matches!(
            grant_upload(&w.store, HOST, &job_id, fence, "owner", &off_primary)
                .await
                .unwrap(),
            UploadGrant::Refused("destinations_changed")
        ),
        "a copy on a destination the node lacks is refused"
    );
    let mut both = files.clone();
    both[0].copies.push(bookclerk_node_proto::FileCopy {
        destination: "s3".into(),
        key: "Title/Title.m4b".into(),
    });
    let wrong_primary = UploadGrantRequest {
        files: &both,
        destinations: &s3_first,
        ..riley
    };
    assert!(
        matches!(
            grant_upload(&w.store, HOST, &job_id, fence, "owner", &wrong_primary)
                .await
                .unwrap(),
            UploadGrant::Refused("invalid_manifest")
        ),
        "the primary key is the primary destination's"
    );
    let UploadGrant::Granted {
        upload_id, token, ..
    } = grant_upload(&w.store, HOST, &job_id, fence, "owner", &riley)
        .await
        .unwrap()
    else {
        panic!("granted");
    };
    assert!(authorize_upload(&w.store, &upload_id, "wrong", false)
        .await
        .unwrap()
        .is_none());
    let session = authorize_upload(&w.store, &upload_id, &token, false)
        .await
        .unwrap()
        .expect("the token opens its upload");
    assert_eq!(session.receiving_node_id, "owner");
    assert_eq!(session.destinations, vec!["local".to_string()]);
    assert!(upload_fence_live(&w.store, &session).await.unwrap());
    assert_eq!(
        commit_upload(&w.store, &upload_id, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Refused("files_missing")
    );
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    assert_eq!(
        commit_upload(
            &w.store,
            &upload_id,
            Some(finish("1 title")),
            SETTINGS.limits
        )
        .await
        .unwrap(),
        UploadCommit::Refused("not_a_closing_upload"),
        "a title's upload cannot finish the job"
    );
    assert_eq!(
        commit_upload(&w.store, &upload_id, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Committed {
            job_completed: false
        }
    );
    let book = w.store.get_book_by_uuid(&book_uuid).await.unwrap().unwrap();
    assert_eq!(book.acquire_status, crate::models::AcquireStatus::Acquired);
    assert_eq!(book.storage_key.as_deref(), Some("Author/Title/Title.m4b"));
    assert_eq!(
        w.store.get_job(&job_id).await.unwrap().unwrap().state,
        crate::models::JobState::Running
    );
    let closing = closing_upload(&w.store, &job_id, fence).await;
    assert_eq!(
        commit_upload(&w.store, &closing, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Refused("closing_upload_must_finish")
    );
    assert_eq!(
        commit_upload(&w.store, &closing, Some(finish("1 title")), SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Committed {
            job_completed: true
        }
    );
    let done = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(done.state, crate::models::JobState::Succeeded);
    assert_eq!(done.progress.as_deref(), Some("1 title"));
    assert!(w
        .store
        .list_domain_events(50)
        .await
        .unwrap()
        .iter()
        .any(|event| event.event_type == "book_acquired" && event.correlation_id == book_uuid));
    assert!(
        live_permits(&w.store, &class_semaphore(JobResourceClass::Network), 8)
            .await
            .unwrap()
            .is_empty(),
        "permits are given back at the commit"
    );
    assert_eq!(
        commit_upload(&w.store, &closing, Some(finish("1 title")), SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Committed {
            job_completed: true
        },
        "a repeated commit answers the same"
    );
    assert!(
        authorize_upload(&w.store, &upload_id, &token, false)
            .await
            .unwrap()
            .is_none(),
        "a committed upload takes no more files"
    );
}

/// A fence lost after the files arrived but before the commit leaves the
/// title unacquired and the job unfinished.
#[tokio::test]
async fn a_fence_lost_before_the_commit_writes_nothing() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let UploadGrant::Granted { upload_id, .. } = grant_upload(
        &w.store,
        HOST,
        &job_id,
        fence,
        "owner",
        &UploadGrantRequest {
            book_uuid: &book_uuid,
            primary_key: "Author/Title/Title.m4b",
            pdf_key: None,
            files: &files,
            destinations: &local("Author/Title"),
        },
    )
    .await
    .unwrap() else {
        panic!("granted");
    };
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    expire_lease(&w.store, &job_id).await;
    assert_eq!(
        commit_upload(&w.store, &upload_id, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Lost
    );
    let book = w.store.get_book_by_uuid(&book_uuid).await.unwrap().unwrap();
    assert_ne!(book.acquire_status, crate::models::AcquireStatus::Acquired);
    assert_ne!(
        w.store.get_job(&job_id).await.unwrap().unwrap().state,
        crate::models::JobState::Succeeded
    );
}

/// H-2: a node-local instance deploys only to the node that owns it, is
/// never placed by the scheduler, and never pins an acquire to its node.
#[tokio::test]
async fn a_node_local_instance_runs_only_on_its_node() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let actor = ConfigActor::Operator { id: "test".into() };
    assert!(
        create_node_local_instance(&w.store, &actor, PLUGIN, "no-such-node", "Desk")
            .await
            .is_err(),
        "the node must exist"
    );
    let instance = create_node_local_instance(&w.store, &actor, PLUGIN, HOST, "Desk")
        .await
        .unwrap();
    assert_eq!(instance.owner_node_id.as_deref(), Some(HOST));
    assert!(
        ensure_plugin_deployment(
            &w.store,
            &actor,
            &instance.id,
            "other-node",
            DesiredState::Present
        )
        .await
        .is_err(),
        "another node may not run it"
    );
    ensure_plugin_deployment(&w.store, &actor, &instance.id, HOST, DesiredState::Present)
        .await
        .expect("its own node runs it");
    assert!(put_plugin_placement(
        &w.store,
        &actor,
        &instance.id,
        &PlacementSpec {
            replicas: Replicas::Count(1),
            constraints: Vec::new(),
        },
        None
    )
    .await
    .is_err());
    let cluster = create_plugin_instance(&w.store, &actor, PLUGIN, "Shared")
        .await
        .unwrap();
    assert_eq!(cluster.owner_node_id, None);
    ensure_plugin_deployment(&w.store, &actor, &cluster.id, HOST, DesiredState::Present)
        .await
        .unwrap();
    let bound: std::collections::BTreeSet<String> = [PLUGIN.to_string()].into();
    let requires =
        resolve_job_requirements(&w.store, JobKind::Acquire, &JobPayload::default(), &bound)
            .await
            .unwrap();
    assert_eq!(
        requires,
        vec![cluster.id.to_string()],
        "only the cluster instance binds an acquire"
    );
}

/// A scan's rows are accepted only as the job's source plugin: a plugin
/// the job merely requires (a destination or integration) is refused, even
/// for a Host trusted with every job.
#[tokio::test]
async fn scan_rows_are_accepted_only_as_the_jobs_source_plugin() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    const OTHER: &str = "path:file:///tmp/dest#dest";
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    // An administrator's Host may run jobs that require other plugins.
    let admin = w
        .store
        .create_user(UserRole::Administrator, Some("Admin"), None)
        .await
        .unwrap()
        .id;
    nodes::Entity::update_many()
        .col_expr(nodes::Column::OwnerUserId, Expr::value(Some(admin)))
        .filter(nodes::Column::NodeId.eq(HOST))
        .exec(w.store.db())
        .await
        .unwrap();
    let destination = uuid::Uuid::new_v4().hyphenated().to_string();
    crate::entities::plugin_instances::ActiveModel {
        plugin_instance_id: Set(destination.clone()),
        plugin_key: Set(OTHER.into()),
        display_name: Set(String::new()),
        created_at: Set(chrono::Utc::now().to_rfc3339()),
        created_by: Set("test".into()),
        owner_node_id: Set(None),
    }
    .insert(w.store.db())
    .await
    .unwrap();
    let mut payload = w.store.get_job(&job_id).await.unwrap().unwrap().payload;
    payload.requires = vec![destination];
    jobs::Entity::update_many()
        .col_expr(
            jobs::Column::Payload,
            Expr::value(serde_json::to_string(&payload).unwrap()),
        )
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(w.store.db())
        .await
        .unwrap();
    let job = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert!(
        job_plugin_keys(&w.store, &job)
            .await
            .unwrap()
            .unwrap()
            .contains(OTHER),
        "the job runs the destination plugin too"
    );
    assert_eq!(
        accept_scan_results(
            &w.store,
            HOST,
            &job_id,
            fence,
            OTHER,
            vec![scanned("riley-acct", "d1")]
        )
        .await
        .unwrap(),
        ScanAccept::Refused("plugin_not_in_job")
    );
    assert!(book_row(&w.store, "riley-acct", "d1").await.is_none());
    assert_eq!(
        accept_scan_results(
            &w.store,
            HOST,
            &job_id,
            fence,
            PLUGIN,
            vec![scanned("riley-acct", "d2")]
        )
        .await
        .unwrap(),
        ScanAccept::Accepted { upserted: 1 }
    );
}

/// A repeat of the commit that completed the job gives back any permit
/// the node still holds for it (as if the completing commit had stopped
/// before its own release) and answers the committed result unchanged.
#[tokio::test]
async fn a_repeated_completing_commit_gives_back_left_permits() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    upload_title(&w.store, &job_id, fence, &book_uuid, &files).await;
    let closing = closing_upload(&w.store, &job_id, fence).await;
    let done = UploadCommit::Committed {
        job_completed: true,
    };
    assert_eq!(
        commit_upload(&w.store, &closing, Some(finish("done")), SETTINGS.limits)
            .await
            .unwrap(),
        done
    );
    let network = class_semaphore(JobResourceClass::Network);
    assert!(live_permits(&w.store, &network, 8)
        .await
        .unwrap()
        .is_empty());
    let job = w.store.get_job(&job_id).await.unwrap().unwrap();
    let semaphores = job_semaphores(&w.store, &job, SETTINGS.limits)
        .await
        .unwrap();
    let holder = format!(
        "{}left",
        job_permit_holder_prefix(&remote_lease_owner(HOST), &job_id, fence)
    );
    assert!(matches!(
        acquire_permits(
            &w.store,
            &semaphores,
            &holder,
            std::time::Duration::from_secs(60)
        )
        .await
        .unwrap(),
        PermitAcquire::Granted(_)
    ));
    assert!(!live_permits(&w.store, &network, 8)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        commit_upload(&w.store, &closing, Some(finish("done")), SETTINGS.limits)
            .await
            .unwrap(),
        done,
        "the repeat keeps the committed result"
    );
    assert!(
        live_permits(&w.store, &network, 8)
            .await
            .unwrap()
            .is_empty(),
        "the repeat gives the left permit back"
    );
    assert_eq!(
        commit_upload(&w.store, &closing, Some(finish("done")), SETTINGS.limits)
            .await
            .unwrap(),
        done,
        "giving back is idempotent"
    );
    assert_eq!(
        w.store
            .get_job(&job_id)
            .await
            .unwrap()
            .unwrap()
            .progress
            .as_deref(),
        Some("done")
    );
}

/// Sets the recorded audio and PDF keys of the title `uuid`.
async fn set_book_keys(store: &LibraryStore, uuid: &str, audio: &str, pdf: Option<&str>) {
    crate::entities::books::Entity::update_many()
        .col_expr(
            crate::entities::books::Column::StorageKey,
            Expr::value(Some(audio.to_string())),
        )
        .col_expr(
            crate::entities::books::Column::PdfStorageKey,
            Expr::value(pdf.map(str::to_string)),
        )
        .filter(crate::entities::books::Column::Uuid.eq(uuid))
        .exec(store.db())
        .await
        .unwrap();
}

/// A Host can write a title only under that title's own key scope on each
/// destination, which the owner computes: a key outside it (another
/// title's folder, a root-level file, another destination's scope) is
/// refused, and so is a key inside it that another title already has (its
/// audio, its PDF, or a sidecar of its audio). Nothing is granted.
#[tokio::test]
async fn an_upload_is_bound_to_its_titles_key_scope() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    // Another title of the same account, already acquired: one in another
    // folder, and one sharing this title's folder (a folder template
    // without the title, or podcast episodes in their parent's folder).
    let elsewhere = w
        .store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("riley-acct", "t9")
        })
        .await
        .unwrap();
    set_book_keys(
        &w.store,
        &elsewhere.uuid,
        "Author/Other/Other.m4b",
        Some("Author/Other/Other.pdf"),
    )
    .await;
    let neighbour = w
        .store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("riley-acct", "t8")
        })
        .await
        .unwrap();
    set_book_keys(
        &w.store,
        &neighbour.uuid,
        "Author/Title/Neighbour.m4b",
        Some("Author/Title/Neighbour booklet.pdf"),
    )
    .await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let destinations = local("Author/Title");
    let with_extra = |key: &str| {
        let mut out = files.clone();
        out.push(bookclerk_node_proto::UploadFile {
            copies: vec![bookclerk_node_proto::FileCopy {
                destination: "local".into(),
                key: key.into(),
            }],
            sha256: "cd".repeat(32),
            ..files[0].clone()
        });
        out
    };
    let refused = |files: Vec<bookclerk_node_proto::UploadFile>, primary: &'static str| {
        let store = w.store.clone();
        let job_id = job_id.clone();
        let book_uuid = book_uuid.clone();
        let destinations = destinations.clone();
        async move {
            match grant_upload(
                &store,
                HOST,
                &job_id,
                fence,
                "owner",
                &UploadGrantRequest {
                    book_uuid: &book_uuid,
                    primary_key: primary,
                    pdf_key: None,
                    files: &files,
                    destinations: &destinations,
                },
            )
            .await
            .unwrap()
            {
                UploadGrant::Refused(reason) => reason,
                other => panic!("expected a refusal: {other:?}"),
            }
        }
    };
    for key in [
        "Author/Other/Other.m4b",
        "Author/Other/cover.jpg",
        "Author/Titles/Title.m4b",
        "Author/Title",
        "Title.m4b",
        "Elsewhere/Author/Title/Title.m4b",
    ] {
        assert_eq!(
            refused(with_extra(key), "Author/Title/Title.m4b").await,
            "key_out_of_scope",
            "{key}"
        );
    }
    let mut moved = files.clone();
    moved[0].copies[0].key = "Author/Other/Other.m4b".into();
    assert_eq!(
        refused(moved, "Author/Other/Other.m4b").await,
        "key_out_of_scope",
        "the primary key is bound too"
    );
    for key in [
        "Author/Title/Neighbour.m4b",
        "Author/Title/Neighbour.jpg",
        "Author/Title/Neighbour.chapters.flat.json",
        "Author/Title/Neighbour booklet.pdf",
    ] {
        assert_eq!(
            refused(with_extra(key), "Author/Title/Title.m4b").await,
            "key_of_another_book",
            "{key}"
        );
    }
    let mut overwrite = files.clone();
    overwrite[0].copies[0].key = "Author/Title/Neighbour.m4b".into();
    assert_eq!(
        refused(overwrite, "Author/Title/Neighbour.m4b").await,
        "key_of_another_book",
        "another title's audio cannot be the primary either"
    );
    let unscoped = vec![UploadDestination {
        id: "local".into(),
        title_folders: Vec::new(),
        title_names: None,
    }];
    assert!(matches!(
        grant_upload(
            &w.store,
            HOST,
            &job_id,
            fence,
            "owner",
            &UploadGrantRequest {
                book_uuid: &book_uuid,
                primary_key: "Author/Title/Title.m4b",
                pdf_key: None,
                files: &files,
                destinations: &unscoped,
            },
        )
        .await
        .unwrap(),
        UploadGrant::Refused("key_out_of_scope")
    ));
    assert!(
        node_upload_sessions::Entity::find()
            .all(w.store.db())
            .await
            .unwrap()
            .is_empty(),
        "nothing was granted"
    );
    // The title's own files and sidecars, a re-acquire of its own recorded
    // key included, are granted, and so is a name another title's key only
    // matches as a `LIKE` pattern (its `_` is no wildcard to the check).
    set_book_keys(&w.store, &book_uuid, "Author/Title/Title.m4b", None).await;
    let mut own = with_extra("Author/Title/Title.jpg");
    own.push(bookclerk_node_proto::UploadFile {
        copies: vec![bookclerk_node_proto::FileCopy {
            destination: "local".into(),
            key: "Author/Title/Neighbou_.jpg".into(),
        }],
        sha256: "ef".repeat(32),
        ..files[0].clone()
    });
    assert!(matches!(
        grant_upload(
            &w.store,
            HOST,
            &job_id,
            fence,
            "owner",
            &UploadGrantRequest {
                book_uuid: &book_uuid,
                primary_key: "Author/Title/Title.m4b",
                pdf_key: None,
                files: &own,
                destinations: &destinations,
            },
        )
        .await
        .unwrap(),
        UploadGrant::Granted { .. }
    ));
}

/// A destination's title scope: below a folder, or top level for the
/// root; never the folder itself or a sibling with the same prefix.
#[test]
fn a_destination_scope_admits_only_keys_below_its_folders() {
    let folder = UploadDestination {
        id: "local".into(),
        title_folders: vec!["A/T".into()],
        title_names: None,
    };
    assert!(folder.admits("A/T/T.m4b"));
    assert!(folder.admits("A/T/sub/c1.mp3"));
    assert!(!folder.admits("A/T"));
    assert!(!folder.admits("A/T2/T.m4b"));
    assert!(!folder.admits("B/A/T/T.m4b"));
    let root = UploadDestination {
        id: "local".into(),
        title_folders: vec![String::new()],
        title_names: None,
    };
    assert!(root.admits("T.m4b"));
    assert!(!root.admits("A/T.m4b"));
    let none = UploadDestination {
        id: "local".into(),
        title_folders: Vec::new(),
        title_names: None,
    };
    assert!(!none.admits("T.m4b"));
}

/// The planned names of title `One [B00ONE]` by `Ann Author` in `folder`
/// (empty for the root), as the owner renders them for a template whose
/// folder other titles share: its audio stem, its plain parts and its
/// split chapters (`<title> - <ch#> - <chapter title>`).
fn shared_scope(folder: &str) -> UploadDestination {
    let at = |name: String| {
        if folder.is_empty() {
            name
        } else {
            format!("{folder}/{name}")
        }
    };
    UploadDestination {
        id: "local".into(),
        title_folders: vec![folder.into()],
        title_names: Some(TitleNames {
            audio_stems: vec![at("One [B00ONE]".into())],
            part_patterns: vec![
                at(format!(
                    "One — {UPLOAD_NAME_WILDCARD} [B00ONE-p{UPLOAD_NUMBER_WILDCARD}]"
                )),
                at(format!(
                    "One - {UPLOAD_NUMBER_WILDCARD} - {UPLOAD_NAME_WILDCARD}"
                )),
            ],
        }),
    }
}

/// In a folder several titles share (`<first author>`) and at the root, an
/// upload's scope is the title's planned names, not the folder: its audio
/// with any extension, its sidecars, its parts and its chapters are
/// admitted; another title's planned name and an arbitrary name are not,
/// and only audio names (not a sidecar's) may be the primary key.
#[test]
fn an_upload_into_a_shared_folder_admits_only_the_titles_planned_names() {
    for folder in ["Ann Author", ""] {
        let scope = shared_scope(folder);
        let at = |name: &str| {
            if folder.is_empty() {
                name.to_string()
            } else {
                format!("{folder}/{name}")
            }
        };
        for name in [
            "One [B00ONE].m4b",
            "One [B00ONE].mp3",
            "One [B00ONE].jpg",
            "One [B00ONE].pdf",
            "One [B00ONE].cue",
            "One [B00ONE].chapters.flat.json",
            "One [B00ONE].chapters.tree.json",
            "One [B00ONE].m4b.bookclerk-meta.json",
            "One — Part 1 [B00ONE-p001].mp3",
            "One — Part 1 [B00ONE-p001].jpg",
            "One - 3 - The Storm.mp3",
            "One - 12 - A. B. Sea.m4a",
        ] {
            assert!(scope.admits(&at(name)), "{folder:?}: {name}");
        }
        for name in [
            "One [B00ONE].m4b",
            "One [B00ONE].flac",
            "One — Part 1 [B00ONE-p001].mp3",
            "One - 3 - The Storm.mp3",
        ] {
            assert!(scope.admits_primary(&at(name)), "{folder:?}: {name}");
        }
        for name in [
            // Another title's planned audio and sidecar, and its parts.
            "Two [B00TWO].m4b",
            "Two [B00TWO].jpg",
            "Two — Part 1 [B00TWO-p001].mp3",
            // Arbitrary names, and near misses of this title's.
            "cover.jpg",
            "anything.m4b",
            "One [B00ONE]",
            "One [B00ONE]x.m4b",
            "One.m4b",
            "One — Part 1 [B00ONE-pX].mp3",
            "One - x - The Storm.mp3",
            "One - 3 - .mp3",
            "One [B00ONE]/inside.m4b",
        ] {
            assert!(!scope.admits(&at(name)), "{folder:?}: {name}");
        }
        assert!(
            !scope.admits_primary(&at("One [B00ONE].m4b.bookclerk-meta.json")),
            "{folder:?}: a sidecar is not the audio"
        );
    }
    assert!(
        !shared_scope("Ann Author").admits("Ann Author Two/One [B00ONE].m4b"),
        "still bound to the folder"
    );
}

/// A Host trusted with one title cannot claim another title's not yet
/// acquired name in a folder they share: the grant is refused
/// (`key_out_of_scope`) for another title's planned name, an arbitrary
/// name or a primary that is not the title's audio, and granted for the
/// title's own audio and sidecars.
#[tokio::test]
async fn an_upload_grant_into_a_shared_folder_is_bound_to_the_titles_names() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, mut files) = riley_title(&w.store).await;
    files[0].copies[0].key = "Ann Author/One [B00ONE].m4b".into();
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let destinations = vec![shared_scope("Ann Author")];
    let with_extra = |key: &str| {
        let mut out = files.clone();
        out.push(bookclerk_node_proto::UploadFile {
            copies: vec![bookclerk_node_proto::FileCopy {
                destination: "local".into(),
                key: key.into(),
            }],
            sha256: "cd".repeat(32),
            ..files[0].clone()
        });
        out
    };
    let grant = |files: Vec<bookclerk_node_proto::UploadFile>, primary: &'static str| {
        let store = w.store.clone();
        let job_id = job_id.clone();
        let book_uuid = book_uuid.clone();
        let destinations = destinations.clone();
        async move {
            grant_upload(
                &store,
                HOST,
                &job_id,
                fence,
                "owner",
                &UploadGrantRequest {
                    book_uuid: &book_uuid,
                    primary_key: primary,
                    pdf_key: None,
                    files: &files,
                    destinations: &destinations,
                },
            )
            .await
            .unwrap()
        }
    };
    for key in ["Ann Author/Two [B00TWO].m4b", "Ann Author/anything.m4b"] {
        assert!(
            matches!(
                grant(with_extra(key), "Ann Author/One [B00ONE].m4b").await,
                UploadGrant::Refused("key_out_of_scope")
            ),
            "{key}"
        );
    }
    let sidecar_primary = with_extra("Ann Author/One [B00ONE].m4b.bookclerk-meta.json");
    assert!(
        matches!(
            grant(
                sidecar_primary,
                "Ann Author/One [B00ONE].m4b.bookclerk-meta.json"
            )
            .await,
            UploadGrant::Refused("key_out_of_scope")
        ),
        "the primary must be the title's audio"
    );
    assert!(
        node_upload_sessions::Entity::find()
            .all(w.store.db())
            .await
            .unwrap()
            .is_empty(),
        "nothing was granted"
    );
    assert!(matches!(
        grant(
            with_extra("Ann Author/One [B00ONE].jpg"),
            "Ann Author/One [B00ONE].m4b"
        )
        .await,
        UploadGrant::Granted { .. }
    ));
}

/// A job whose cancel was requested never succeeds on a Host's word: a
/// scan's `complete` ends it cancelled, and an acquire's closing upload is
/// refused (the Host's failure report then ends it cancelled).
#[tokio::test]
async fn a_cancelled_remote_job_never_succeeds() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let scan = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    w.store.request_job_cancel(&scan).await.unwrap();
    assert_eq!(
        finish_remote_job(
            &w.store,
            HOST,
            &scan,
            fence,
            SETTINGS.limits,
            RemoteOutcome::Complete(Some("partial"))
        )
        .await
        .unwrap(),
        RemoteFinish::Done
    );
    assert_eq!(
        w.store.get_job(&scan).await.unwrap().unwrap().state,
        crate::models::JobState::Cancelled
    );

    let acquire = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    upload_title(&w.store, &acquire, fence, &book_uuid, &files).await;
    w.store.request_job_cancel(&acquire).await.unwrap();
    let closing = closing_upload(&w.store, &acquire, fence).await;
    assert_eq!(
        commit_upload(&w.store, &closing, Some(finish("done")), SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Refused("job_cancelled")
    );
    assert_eq!(
        w.store.get_job(&acquire).await.unwrap().unwrap().state,
        crate::models::JobState::Running
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

    assert_eq!(
        release_remote_job(&w.store, HOST, &job_id, fence, false, SETTINGS.limits)
            .await
            .unwrap(),
        RemoteFinish::Done
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
    assert_eq!(
        release_remote_job(&w.store, HOST, &job_id, fence, false, SETTINGS.limits)
            .await
            .unwrap(),
        RemoteFinish::Lost,
        "a released fence is not live"
    );
    let again = claim(&w.store).await.expect("claimable again at once");
    assert!(again.job.lease_generation > fence);
}

/// A claim whose plugin keys cannot be read is given back unspent, with
/// its permits, rather than left `running` until its lease expires.
#[tokio::test]
async fn a_claim_whose_plugin_keys_cannot_be_read_is_given_back() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    // Only a privileged owner's Host may take a job that requires an
    // instance, the one input of the plugin-key lookup that the claim
    // itself never reads.
    w.store
        .set_user_role(w.riley, UserRole::Administrator)
        .await
        .unwrap();
    let actor = ConfigActor::Operator { id: "test".into() };
    let url = put_artifact_blob(&w.store, b"archive bytes").await.unwrap();
    let archive = url.strip_prefix("blob:sha256:").unwrap().to_string();
    add_plugin_version(
        &w.store,
        &actor,
        PluginVersion {
            plugin_key: PLUGIN.into(),
            version: "1.0.0".into(),
            manifest_id: "store".into(),
            manifest_toml: String::new(),
            settings: Default::default(),
            artifacts: vec![PluginArtifact {
                target: "any".into(),
                url,
                format: "tar.gz".into(),
                archive_root: ".".into(),
                archive_sha256: archive,
                payload_root_sha256: "ab".repeat(32),
                executable: "guest".into(),
            }],
            publisher_verified: false,
            grant_json: String::new(),
            coordinate: String::new(),
            installed_at: String::new(),
            installed_by: String::new(),
        },
    )
    .await
    .unwrap();
    activate_version(&w.store, &actor, PLUGIN, "1.0.0", HOST)
        .await
        .unwrap();
    let instance = create_plugin_instance(&w.store, &actor, PLUGIN, "")
        .await
        .unwrap();
    let deployment =
        ensure_plugin_deployment(&w.store, &actor, &instance.id, HOST, DesiredState::Present)
            .await
            .unwrap();
    upsert_observation(
        &w.store,
        &DeploymentObservation {
            deployment_id: deployment.deployment_id.clone(),
            node_id: HOST.into(),
            incarnation: "inc".into(),
            status: DeploymentStatus::Running,
            detail: String::new(),
            applied_config_revision: None,
            observed_at: chrono::Utc::now().to_rfc3339(),
        },
    )
    .await
    .unwrap();
    let job_id = match w
        .store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload {
                account: Some("riley-acct".into()),
                requires: vec![instance.id.as_str().to_string()],
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
    };
    // The instance row no longer decodes (a value that is not UTF-8 text),
    // so reading the job's plugin keys fails after the claim was taken.
    let corrupt = format!(
        "UPDATE plugin_instances SET display_name = X'FF' WHERE plugin_instance_id = '{}'",
        instance.id.as_str()
    );
    w.store.db().execute_unprepared(&corrupt).await.unwrap();

    claim_remote_job(&w.store, HOST, JobResourceClass::Network, SETTINGS, None)
        .await
        .expect_err("the plugin-key lookup fails");
    let back = w.store.get_job(&job_id).await.unwrap().unwrap();
    assert_eq!(back.state, crate::models::JobState::Pending, "given back");
    assert_eq!(back.attempt_count, 0, "no attempt is spent");
    assert!(
        live_permits(&w.store, &class_semaphore(JobResourceClass::Network), 8)
            .await
            .unwrap()
            .is_empty(),
        "the permits are given back"
    );
    assert!(
        live_permits(&w.store, &instance_semaphore(instance.id.as_str()), 8)
            .await
            .unwrap()
            .is_empty(),
        "the instance permit is given back"
    );

    let repair = format!(
        "UPDATE plugin_instances SET display_name = '' WHERE plugin_instance_id = '{}'",
        instance.id.as_str()
    );
    w.store.db().execute_unprepared(&repair).await.unwrap();
    let again = claim(&w.store).await.expect("claimable again at once");
    assert_eq!(again.job.id, job_id);
    assert_eq!(again.plugin_keys, vec![PLUGIN.to_string()]);
}

/// Forgetting an earlier lease's handoffs keeps a successor claim's: a
/// release that runs after the job was claimed again does not cost the new
/// Host its write-back.
#[tokio::test]
async fn forgetting_an_earlier_lease_keeps_the_successors_handoffs() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let first = claim(&w.store).await.unwrap().job.lease_generation;
    assert_eq!(
        release_remote_job(&w.store, HOST, &job_id, first, false, SETTINGS.limits)
            .await
            .unwrap(),
        RemoteFinish::Done
    );
    let second = claim(&w.store).await.unwrap().job.lease_generation;
    assert!(second > first);

    // The first release's cleanup lands after the successor's handoff.
    remote_work::forget_handoffs(&w.store, &job_id, first)
        .await
        .unwrap();
    assert_eq!(
        write_back_job_secret(&w.store, HOST, &job_id, second, &w.riley_token, b"riley-v2")
            .await
            .unwrap(),
        WriteBack::Written
    );
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

/// The current row of the semaphore slot lease `name`.
async fn slot_lease(store: &LibraryStore, name: &str) -> ClusterLease {
    load_lease(store, name).await.unwrap().expect("slot row")
}

/// A still-live permit left over from an earlier claim of the same job by
/// the same Host (an older lease generation, in a low slot) is not taken for
/// the current claim's permit (in a slot above a lowered limit): renewing
/// extends the current permit and finishing frees its slot, while the
/// leftover is left to expire with its own lease.
#[tokio::test]
async fn a_leftover_permit_of_an_earlier_generation_is_not_the_claims() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let semaphore = class_semaphore(JobResourceClass::Network);
    let far = chrono::Utc::now().timestamp_millis() + 3_600_000;
    let job_id = enqueue(&w.store, JobKind::Scan, "riley-acct").await;
    let leftover_holder = format!(
        "{}stale",
        job_permit_holder_prefix(&remote_lease_owner(HOST), &job_id, 0)
    );
    for (slot, holder) in [(0, leftover_holder.as_str()), (1, "someone-else")] {
        cluster_leases::ActiveModel {
            name: Set(semaphore_slot_lease_name(&semaphore, slot)),
            holder: Set(holder.into()),
            token: Set(1),
            expires_at_ms: Set(far),
            renewed_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(w.store.db())
        .await
        .unwrap();
    }
    let claimed = claim(&w.store).await.expect("a free slot above the others");
    let fence = claimed.job.lease_generation;
    assert_eq!(fence, 1, "the claim runs one generation after the leftover");
    let current = semaphore_slot_lease_name(&semaphore, 2);
    let row = slot_lease(&w.store, &current).await;
    assert!(
        row.holder.starts_with(&job_permit_holder_prefix(
            &remote_lease_owner(HOST),
            &job_id,
            fence
        )),
        "the claim's permit is in the first free slot, bound to its generation"
    );
    // Bring the current permit close to expiry so a renewal is visible.
    let soon = chrono::Utc::now().timestamp_millis() + 1_000;
    cluster_leases::Entity::update_many()
        .col_expr(cluster_leases::Column::ExpiresAtMs, Expr::value(soon))
        .filter(cluster_leases::Column::Name.eq(current.as_str()))
        .exec(w.store.db())
        .await
        .unwrap();

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
    assert!(
        slot_lease(&w.store, &current).await.expires_at_ms > soon + 30_000,
        "the renewal extends the current claim's permit"
    );
    let leftover = slot_lease(&w.store, &semaphore_slot_lease_name(&semaphore, 0)).await;
    assert_eq!(leftover.holder, leftover_holder);
    assert_eq!(leftover.expires_at_ms, far, "the leftover is not renewed");

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
    let mut slots: Vec<u32> = live.iter().map(|p| p.slot).collect();
    slots.sort_unstable();
    assert_eq!(
        slots,
        vec![0, 1],
        "finishing frees the current slot; the leftover expires on its own"
    );
}

/// One extra `local` file at `key` added to `files`.
fn with_local_file(
    files: &[bookclerk_node_proto::UploadFile],
    key: &str,
) -> Vec<bookclerk_node_proto::UploadFile> {
    let mut out = files.to_vec();
    out.push(bookclerk_node_proto::UploadFile {
        copies: vec![bookclerk_node_proto::FileCopy {
            destination: "local".into(),
            key: key.into(),
        }],
        sha256: "cd".repeat(32),
        ..files[0].clone()
    });
    out
}

/// In a folder several titles share, a key recorded for another library
/// title on that destination (a chapter file its acquire wrote, or a key
/// another title's open upload claimed) is refused; the same key recorded
/// only on another destination, or for a title no longer in the library,
/// is granted, and the grant claims it for its own title.
#[tokio::test]
async fn a_shared_folder_never_grants_another_titles_recorded_file() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let neighbour = w
        .store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("riley-acct", "t8")
        })
        .await
        .unwrap();
    // The neighbour's acquire wrote a chapter file into the shared folder
    // on `local`, and a cover there on `s3` only.
    w.store
        .record_title_file("local", "Author/Title/Neighbour - 01.mp3", &neighbour.uuid)
        .await
        .unwrap();
    w.store
        .record_title_file("s3", "Author/Title/cover.jpg", &neighbour.uuid)
        .await
        .unwrap();
    w.store
        .record_title_file("local", "Author/Title/Gone.jpg", "no-such-title")
        .await
        .unwrap();
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let destinations = local("Author/Title");
    let grant = |uuid: String, files: Vec<bookclerk_node_proto::UploadFile>| {
        let store = w.store.clone();
        let job_id = job_id.clone();
        let destinations = destinations.clone();
        async move {
            grant_upload(
                &store,
                HOST,
                &job_id,
                fence,
                "owner",
                &UploadGrantRequest {
                    book_uuid: &uuid,
                    primary_key: "Author/Title/Title.m4b",
                    pdf_key: None,
                    files: &files,
                    destinations: &destinations,
                },
            )
            .await
            .unwrap()
        }
    };
    assert!(matches!(
        grant(
            book_uuid.clone(),
            with_local_file(&files, "Author/Title/Neighbour - 01.mp3")
        )
        .await,
        UploadGrant::Refused("key_of_another_book")
    ));
    let granted = grant(
        book_uuid.clone(),
        with_local_file(
            &with_local_file(&files, "Author/Title/cover.jpg"),
            "Author/Title/Gone.jpg",
        ),
    )
    .await;
    let UploadGrant::Granted { upload_id, .. } = granted else {
        panic!("a key recorded only on another destination is granted: {granted:?}");
    };
    for key in [
        "Author/Title/Title.m4b",
        "Author/Title/cover.jpg",
        "Author/Title/Gone.jpg",
    ] {
        let file = w.store.title_file("local", key).await.unwrap().unwrap();
        assert_eq!(file.book_uuid, book_uuid, "{key}");
        assert_eq!(file.writer, upload_id, "{key}");
    }
    let session = load_upload(&w.store, &upload_id).await.unwrap().unwrap();
    assert!(
        upload_owns_key(&w.store, &session, "local", "Author/Title/cover.jpg")
            .await
            .unwrap()
    );
    assert!(
        !upload_owns_key(&w.store, &session, "s3", "Author/Title/cover.jpg")
            .await
            .unwrap(),
        "the neighbour's s3 cover is not this upload's"
    );
    // Another title of the job asking for a key that open upload claimed
    // is refused, though nothing is acquired yet.
    assert!(matches!(
        grant(neighbour.uuid.clone(), files.clone()).await,
        UploadGrant::Refused("key_of_another_book")
    ));
    // An acquire on the owner that writes the key afterwards takes it
    // over, so the upload no longer owns it.
    w.store
        .record_title_file("local", "Author/Title/cover.jpg", &book_uuid)
        .await
        .unwrap();
    assert!(
        !upload_owns_key(&w.store, &session, "local", "Author/Title/cover.jpg")
            .await
            .unwrap()
    );
}

/// The commit's gate admits the node only while it is still trusted with
/// the title: its owning user active and still linked to the title's
/// account. Unlinking the account fails a title's gate, and disabling the
/// user fails the closing upload's too.
#[tokio::test]
async fn the_commit_gate_carries_the_nodes_trust() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let (book_uuid, _) = riley_title(&w.store).await;
    let book = crate::entities::books::Entity::find()
        .filter(crate::entities::books::Column::Uuid.eq(&book_uuid))
        .one(w.store.db())
        .await
        .unwrap()
        .unwrap();
    let sam_book = w
        .store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("sam-acct", "s1")
        })
        .await
        .unwrap();
    let sam_book = crate::entities::books::Entity::find()
        .filter(crate::entities::books::Column::Uuid.eq(&sam_book.uuid))
        .one(w.store.db())
        .await
        .unwrap()
        .unwrap();
    let passes = |book: Option<crate::entities::books::Model>| {
        let store = w.store.clone();
        async move {
            let (sql, binds) = uploads::commit_trust_gate(HOST, book.as_ref());
            let reply = batch::execute_host_batch(
                &store,
                batch::request(
                    &format!("trust-gate-test:{}", uuid::Uuid::new_v4()),
                    vec![batch::query(
                        &format!("SELECT node_id FROM nodes WHERE {sql}"),
                        binds,
                        1,
                    )],
                ),
            )
            .await
            .unwrap();
            batch::rows(&reply, 0).len() == 1
        }
    };
    assert!(passes(Some(book.clone())).await);
    assert!(passes(None).await);
    assert!(
        !passes(Some(sam_book)).await,
        "another user's account is never admitted"
    );
    account_links::Entity::delete_many()
        .filter(account_links::Column::AccountId.eq("riley-acct"))
        .exec(w.store.db())
        .await
        .unwrap();
    assert!(!passes(Some(book.clone())).await, "an unlinked account");
    assert!(passes(None).await);
    crate::entities::users::Entity::update_many()
        .col_expr(
            crate::entities::users::Column::Status,
            Expr::value("disabled"),
        )
        .filter(crate::entities::users::Column::Id.eq(w.riley))
        .exec(w.store.db())
        .await
        .unwrap();
    assert!(!passes(None).await, "a disabled owner");
}

/// Queues a job of `kind` with `payload`.
async fn enqueue_payload(store: &LibraryStore, kind: JobKind, payload: JobPayload) -> String {
    match store
        .enqueue_job(EnqueueJobSpec {
            kind,
            payload,
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

/// A title acquire reaches a Host only when it names an account of the
/// Host owner's, and is handed that account's secrets; one naming only a
/// title, or a forced one, is never handed out.
#[tokio::test]
async fn a_title_acquire_is_handed_its_named_accounts_secrets() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let scope = w.store.scope(PLUGIN, "test");
    for (account, product, asin) in [
        ("riley-acct", "r1", "B0RILEY"),
        ("sam-acct", "s1", "B0SAM"),
        ("riley-acct", "x1", "B0BOTH"),
        ("sam-acct", "x2", "B0BOTH"),
    ] {
        scope
            .upsert_book(&crate::store::NewBook {
                uuid: None,
                asin: Some(asin.into()),
                ..scanned(account, product)
            })
            .await
            .unwrap();
    }
    let title = |id: &str| JobPayload {
        title: Some(id.into()),
        ..Default::default()
    };
    enqueue_payload(&w.store, JobKind::Acquire, title("B0SAM")).await;
    enqueue_payload(&w.store, JobKind::Acquire, title("B0BOTH")).await;
    enqueue_payload(
        &w.store,
        JobKind::Acquire,
        JobPayload {
            force: true,
            ..title("B0RILEY")
        },
    )
    .await;
    enqueue_payload(&w.store, JobKind::Acquire, title("B0RILEY")).await;
    let riley_job = enqueue_payload(
        &w.store,
        JobKind::Acquire,
        JobPayload {
            account: Some("riley-acct".into()),
            ..title("B0RILEY")
        },
    )
    .await;

    let claimed = claim(&w.store).await.expect("Riley's title is claimed");
    assert_eq!(claimed.job.id, riley_job);
    assert_eq!(claimed.plugin_keys, vec![PLUGIN.to_string()]);
    assert_eq!(claimed.secrets.len(), 1);
    assert_eq!(claimed.secrets[0].secret_ref, w.riley_token);
    assert_eq!(claimed.secrets[0].value.as_slice(), b"riley-token-v1");
    assert!(
        claim(&w.store).await.is_none(),
        "a title alone, Sam's titles and a forced acquire are never handed"
    );
}

/// Grants an upload of `book_uuid` with `files` under `fence`, in the
/// `Author/Title` scope on `local`.
async fn grant_title(
    store: &LibraryStore,
    job_id: &str,
    fence: i64,
    book_uuid: &str,
    files: &[bookclerk_node_proto::UploadFile],
) -> UploadGrant {
    grant_upload(
        store,
        HOST,
        job_id,
        fence,
        "owner",
        &UploadGrantRequest {
            book_uuid,
            primary_key: "Author/Title/Title.m4b",
            pdf_key: None,
            files,
            destinations: &local("Author/Title"),
        },
    )
    .await
    .unwrap()
}

/// A grant refused once claiming began leaves every key as it was: a key
/// it claimed fresh is unrecorded again, a key an open upload of the
/// title held is that upload's again (it is neither superseded nor robbed
/// of its keys), and no upload of the refused grant stays open.
#[tokio::test]
async fn a_refused_upload_grant_gives_back_the_keys_it_claimed() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let neighbour = w
        .store
        .scope(PLUGIN, "test")
        .upsert_book(&crate::store::NewBook {
            uuid: None,
            ..scanned("riley-acct", "t8")
        })
        .await
        .unwrap();
    w.store
        .record_title_file("local", "Author/Title/Neighbour - 01.mp3", &neighbour.uuid)
        .await
        .unwrap();
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let UploadGrant::Granted {
        upload_id: first, ..
    } = grant_title(&w.store, &job_id, fence, &book_uuid, &files).await
    else {
        panic!("the title's first upload is granted");
    };
    // Title.m4b (the open upload's), then a fresh key, then the
    // neighbour's key, which refuses the grant after two claims.
    let refused = with_local_file(
        &with_local_file(&files, "Author/Title/fresh.jpg"),
        "Author/Title/Neighbour - 01.mp3",
    );
    assert!(matches!(
        grant_title(&w.store, &job_id, fence, &book_uuid, &refused).await,
        UploadGrant::Refused("key_of_another_book")
    ));
    let primary = w
        .store
        .title_file("local", "Author/Title/Title.m4b")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(primary.writer, first, "the open upload holds its key again");
    assert_eq!(primary.book_uuid, book_uuid);
    assert!(
        w.store
            .title_file("local", "Author/Title/fresh.jpg")
            .await
            .unwrap()
            .is_none(),
        "a key the refused grant claimed fresh is unrecorded again"
    );
    assert_eq!(
        w.store
            .title_file("local", "Author/Title/Neighbour - 01.mp3")
            .await
            .unwrap()
            .unwrap()
            .book_uuid,
        neighbour.uuid
    );
    let open: Vec<String> = node_upload_sessions::Entity::find()
        .filter(node_upload_sessions::Column::JobId.eq(&job_id))
        .filter(node_upload_sessions::Column::State.eq("open"))
        .all(w.store.db())
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.upload_id)
        .collect();
    assert_eq!(open, vec![first], "only the first upload is open");
}

/// A title's commit records its keys only while the upload still holds
/// every one: once another writer took a key since the grant, the commit
/// is refused `key_taken` and the title is not marked acquired.
#[tokio::test]
async fn an_upload_commit_is_refused_once_a_key_was_taken() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let files = with_local_file(&files, "Author/Title/cover.jpg");
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let UploadGrant::Granted { upload_id, .. } =
        grant_title(&w.store, &job_id, fence, &book_uuid, &files).await
    else {
        panic!("granted");
    };
    for index in 0..files.len() {
        assert!(record_upload_file(&w.store, &upload_id, index)
            .await
            .unwrap());
    }
    // An acquire on the owner wrote the cover since.
    w.store
        .record_title_file("local", "Author/Title/cover.jpg", &book_uuid)
        .await
        .unwrap();
    assert_eq!(
        commit_upload(&w.store, &upload_id, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Refused("key_taken")
    );
    let book = w.store.get_book_by_uuid(&book_uuid).await.unwrap().unwrap();
    assert_ne!(book.acquire_status, crate::models::AcquireStatus::Acquired);
    assert_eq!(
        load_upload(&w.store, &upload_id)
            .await
            .unwrap()
            .unwrap()
            .state,
        "open",
        "nothing was committed"
    );
}

/// A job naming its account by label, or by its id in another case,
/// covers that account's titles: `job_titles` (the plan and the closing
/// commit's scope) resolves the selector as the claim path does.
#[tokio::test]
async fn an_upload_job_naming_its_account_by_label_covers_its_titles() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    label_riley(&w.store).await;
    let (book_uuid, _) = riley_title(&w.store).await;
    for selector in ["riley books", "RILEY-ACCT", "riley-acct"] {
        let job_id = enqueue(&w.store, JobKind::Acquire, selector).await;
        let job = w.store.get_job(&job_id).await.unwrap().unwrap();
        let titles = job_titles(&w.store, HOST, &job).await.unwrap().unwrap();
        assert_eq!(
            titles.iter().map(|b| b.uuid.as_str()).collect::<Vec<_>>(),
            vec![book_uuid.as_str()],
            "{selector}"
        );
        jobs::Entity::delete_by_id(job_id)
            .exec(w.store.db())
            .await
            .unwrap();
    }
}

/// Labels Riley's account `Riley Books`.
async fn label_riley(store: &LibraryStore) {
    accounts::Entity::update_many()
        .col_expr(accounts::Column::Label, Expr::value(Some("Riley Books")))
        .filter(accounts::Column::AccountId.eq("riley-acct"))
        .exec(store.db())
        .await
        .unwrap();
}

/// A lease that ran out no longer owns its job, even before the reclaim
/// runs: no upload is granted under it and an open upload cannot commit.
#[tokio::test]
async fn an_expired_lease_neither_grants_nor_commits_an_upload() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let UploadGrant::Granted { upload_id, .. } =
        grant_title(&w.store, &job_id, fence, &book_uuid, &files).await
    else {
        panic!("granted");
    };
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    let past = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
    jobs::Entity::update_many()
        .col_expr(jobs::Column::LeaseExpiresAt, Expr::value(Some(past)))
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(w.store.db())
        .await
        .unwrap();
    assert!(matches!(
        grant_title(&w.store, &job_id, fence, &book_uuid, &files).await,
        UploadGrant::Lost
    ));
    let session = load_upload(&w.store, &upload_id).await.unwrap().unwrap();
    assert!(!upload_fence_live(&w.store, &session).await.unwrap());
    assert_eq!(
        commit_upload(&w.store, &upload_id, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Lost
    );
    let book = w.store.get_book_by_uuid(&book_uuid).await.unwrap().unwrap();
    assert_ne!(book.acquire_status, crate::models::AcquireStatus::Acquired);
}

/// A scan job naming its account by label, or by its id in another case,
/// is handed with the account's canonical id (the host's credential lookup
/// and its rows use it), and the rows it returns for that id validate.
#[tokio::test]
async fn a_label_named_scans_results_validate_under_the_canonical_id() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    label_riley(&w.store).await;
    for (selector, product) in [("riley books", "p-label"), ("RILEY-ACCT", "p-case")] {
        let job_id = enqueue(&w.store, JobKind::Scan, selector).await;
        let claimed = claim(&w.store).await.expect("claimed");
        assert_eq!(claimed.job.id, job_id);
        assert_eq!(
            claimed.job.payload.account.as_deref(),
            Some("riley-acct"),
            "{selector}: handed with the canonical id"
        );
        let stored = w.store.get_job(&job_id).await.unwrap().unwrap();
        assert_eq!(stored.payload.account.as_deref(), Some(selector));
        assert_eq!(
            accept_scan_results(
                &w.store,
                HOST,
                &job_id,
                claimed.job.lease_generation,
                PLUGIN,
                vec![scanned("riley-acct", product)]
            )
            .await
            .unwrap(),
            ScanAccept::Accepted { upserted: 1 },
            "{selector}"
        );
        assert_eq!(
            accept_scan_results(
                &w.store,
                HOST,
                &job_id,
                claimed.job.lease_generation,
                PLUGIN,
                vec![scanned("sam-acct", product)]
            )
            .await
            .unwrap(),
            ScanAccept::Refused("account_not_in_job"),
            "{selector}: another account's row is still refused"
        );
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
            RemoteFinish::Done
        );
    }
}

/// Of two grants of one title, the newest wins: an older grant never
/// aborts a newer one. A grant that finds a newer open upload of the title
/// is refused `superseded`, gives back what it claimed, and leaves the
/// newer upload open and holding its keys.
#[tokio::test]
async fn an_older_upload_grant_never_aborts_a_newer_one() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let UploadGrant::Granted {
        upload_id: newer, ..
    } = grant_title(&w.store, &job_id, fence, &book_uuid, &files).await
    else {
        panic!("granted");
    };
    // Stamp it as created after any grant this test makes next, as if
    // that grant had started first and was still claiming.
    node_upload_sessions::Entity::update_many()
        .col_expr(
            node_upload_sessions::Column::CreatedAt,
            Expr::value("9999-01-01T00:00:00.000000000Z"),
        )
        .filter(node_upload_sessions::Column::UploadId.eq(&newer))
        .exec(w.store.db())
        .await
        .unwrap();
    assert!(matches!(
        grant_title(&w.store, &job_id, fence, &book_uuid, &files).await,
        UploadGrant::Refused("superseded")
    ));
    assert_eq!(
        load_upload(&w.store, &newer).await.unwrap().unwrap().state,
        "open"
    );
    assert_eq!(
        w.store
            .title_file("local", "Author/Title/Title.m4b")
            .await
            .unwrap()
            .unwrap()
            .writer,
        newer,
        "the newer upload holds its key again"
    );
}

/// A commit whose fence is lost answers `Lost` (so the receiving node
/// discards the upload), even when one of its keys was also taken.
#[tokio::test]
async fn a_lost_fence_upload_commit_answers_lost_before_key_taken() {
    let _guard = master_key_test_lock_async().await;
    let w = world().await;
    let job_id = enqueue(&w.store, JobKind::Acquire, "riley-acct").await;
    let (book_uuid, files) = riley_title(&w.store).await;
    let fence = claim(&w.store).await.unwrap().job.lease_generation;
    let UploadGrant::Granted { upload_id, .. } =
        grant_title(&w.store, &job_id, fence, &book_uuid, &files).await
    else {
        panic!("granted");
    };
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    w.store
        .record_title_file("local", "Author/Title/Title.m4b", &book_uuid)
        .await
        .unwrap();
    let past = (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
    jobs::Entity::update_many()
        .col_expr(jobs::Column::LeaseExpiresAt, Expr::value(Some(past)))
        .filter(jobs::Column::Id.eq(&job_id))
        .exec(w.store.db())
        .await
        .unwrap();
    assert_eq!(
        commit_upload(&w.store, &upload_id, None, SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Lost
    );
}
