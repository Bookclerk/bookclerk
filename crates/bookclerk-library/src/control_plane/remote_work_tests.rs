//! Host work over the node API: remote claims hand only trusted secrets,
//! fences lost while a Host was away stay lost, and write-back needs a
//! live fence.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

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

async fn store_in(dir: &std::path::Path) -> LibraryStore {
    let db = bookclerk_plugin_database_sqlite::open(&dir.join("library.db"))
        .await
        .expect("open sqlite file");
    crate::apply_host_schema(&db).await.expect("schema");
    crate::configure_master_key(dir).expect("master key");
    LibraryStore::from_connection(db)
}

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
        .list_job_claim_candidates(JobResourceClass::Network, 8)
        .await
        .unwrap()
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
        release_remote_job(&w.store, HOST, &job_id, fence, SETTINGS.limits)
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
        release_remote_job(&w.store, HOST, &job_id, fence, SETTINGS.limits)
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
/// that upload, the commit waits for every file, and the commit marks the
/// title acquired, publishes `book_acquired`, completes the job and gives
/// its permits back in one step; a repeat answers the same.
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
    let destinations = vec!["local".to_string()];
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
    let two = vec!["local".to_string(), "s3".to_string()];
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
    let s3_first = vec!["s3".to_string(), "local".to_string()];
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
    assert_eq!(session.destinations, destinations);
    assert!(upload_fence_live(&w.store, &session).await.unwrap());
    assert_eq!(
        commit_upload(&w.store, &upload_id, Some("1 title"), SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Refused("files_missing")
    );
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    assert_eq!(
        commit_upload(&w.store, &upload_id, Some("1 title"), SETTINGS.limits)
            .await
            .unwrap(),
        UploadCommit::Committed {
            job_completed: true
        }
    );
    let book = w.store.get_book_by_uuid(&book_uuid).await.unwrap().unwrap();
    assert_eq!(book.acquire_status, crate::models::AcquireStatus::Acquired);
    assert_eq!(book.storage_key.as_deref(), Some("Author/Title/Title.m4b"));
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
        commit_upload(&w.store, &upload_id, Some("1 title"), SETTINGS.limits)
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
            destinations: &["local".to_string()],
        },
    )
    .await
    .unwrap() else {
        panic!("granted");
    };
    assert!(record_upload_file(&w.store, &upload_id, 0).await.unwrap());
    expire_lease(&w.store, &job_id).await;
    assert_eq!(
        commit_upload(&w.store, &upload_id, Some("done"), SETTINGS.limits)
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

/// H-2: a node-local instance deploys only to the node that owns it and is
/// never placed by the scheduler.
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
}
