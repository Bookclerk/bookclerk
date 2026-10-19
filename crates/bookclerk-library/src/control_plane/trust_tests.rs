//! Trust predicate, filtered claims, handoff re-check, and the scan planner.
//!
//! Each scenario runs on two SQLite connections to one file (the owner
//! node's worker and the connection serving a Host) and, when
//! `BOOKCLERK_TEST_POSTGRES_URL` is set, on two PostgreSQL pools.

use sea_orm::sea_query::Expr;
use sea_orm::{ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter};

use super::tests::{postgres_stores, postgres_tests_enabled};
use super::*;
use crate::entities::{accounts, jobs, node_credentials, nodes};
use crate::error::LibraryError;
use crate::models::{
    EnqueueJobSpec, EnqueueOutcome, JobKind, JobPayload, JobResourceClass, JobState, UserRole,
    UserStatus,
};
use crate::store::LibraryStore;

const PLUGIN: &str = "path:file:///tmp/store#store";
const OTHER_PLUGIN: &str = "path:file:///tmp/other#other";
const HOST: &str = "host-riley";

async fn sqlite_stores() -> (tempfile::TempDir, LibraryStore, LibraryStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("library.db");
    let open = |path: std::path::PathBuf| async move {
        let db = bookclerk_plugin_database_sqlite::open(&path)
            .await
            .expect("open sqlite file");
        crate::apply_host_schema(&db).await.expect("schema");
        LibraryStore::from_connection(db)
    };
    let a = open(path.clone()).await;
    let b = open(path).await;
    (dir, a, b)
}

fn request<'a>(node_id: &'a str, claimant: Claimant, owner: &'a str) -> JobClaimRequest<'a> {
    JobClaimRequest {
        resource_class: JobResourceClass::Network,
        node_id,
        claimant,
        owner,
        lease_secs: 60,
        limits: JobClaimLimits {
            class: 8,
            plugin_instance: 8,
        },
    }
}

async fn user(store: &LibraryStore, role: UserRole, name: &str) -> i64 {
    store.create_user(role, Some(name), None).await.unwrap().id
}

async fn add_account(store: &LibraryStore, plugin_key: &str, account_id: &str, scan: bool) {
    let now = chrono::Utc::now().to_rfc3339();
    accounts::ActiveModel {
        plugin_key: Set(plugin_key.into()),
        account_id: Set(account_id.into()),
        marketplace: Set("us".into()),
        label: Set(None),
        scan_enabled: Set(i64::from(scan)),
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

/// Links `(plugin_key, account_id)` to `user_id` through a local identity.
async fn link(store: &LibraryStore, user_id: i64, plugin_key: &str, account_id: &str) {
    let identity = store
        .ensure_local_portal_identity(user_id, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, plugin_key, account_id, "test")
        .await
        .unwrap();
}

/// Inserts a node row of `kind` owned by `owner`, with a live credential
/// when it is a Host.
async fn add_node(store: &LibraryStore, node_id: &str, kind: &str, owner: Option<i64>) {
    let now = chrono::Utc::now().to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(node_id.into()),
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
        kind: Set(kind.into()),
        owner_user_id: Set(owner),
    }
    .insert(store.db())
    .await
    .unwrap();
    if kind == "host" {
        node_credentials::ActiveModel {
            node_id: Set(node_id.into()),
            sign_public_key: Set(vec![1; 32]),
            box_public_key: Set(vec![2; 32]),
            link_id: Set(format!("link-{node_id}")),
            enrolled_at: Set(now),
            revoked_at: Set(None),
            last_connected_at: Set(None),
        }
        .insert(store.db())
        .await
        .unwrap();
    }
}

async fn scan(store: &LibraryStore, account: Option<&str>) -> String {
    match store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::Scan,
            payload: JobPayload {
                account: account.map(str::to_string),
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

async fn claim(store: &LibraryStore, node: &str, claimant: Claimant) -> Option<RoutedJobClaim> {
    claim_routed_job(store, &request(node, claimant, node))
        .await
        .unwrap()
}

async fn finish(store: &LibraryStore, claimed: &RoutedJobClaim) {
    let fence = claimed.job.fence().unwrap();
    assert!(store.complete_job(&fence, None).await.unwrap());
    release_permits(store, &claimed.permits).await.unwrap();
}

/// A Member's Host claims only its owner's account. Another user's job and
/// a scan-all are refused until the owner is promoted, then both are claimed.
async fn host_trust_follows_its_owners_role(owner_db: LibraryStore, host_db: LibraryStore) {
    let riley = user(&owner_db, UserRole::Member, "Riley").await;
    let sam = user(&owner_db, UserRole::Member, "Sam").await;
    add_account(&owner_db, PLUGIN, "riley-acct", true).await;
    add_account(&owner_db, PLUGIN, "sam-acct", true).await;
    link(&owner_db, riley, PLUGIN, "riley-acct").await;
    link(&owner_db, sam, PLUGIN, "sam-acct").await;
    add_node(&owner_db, HOST, "host", Some(riley)).await;

    let all = scan(&owner_db, None).await;
    let sams = scan(&owner_db, Some("sam-acct")).await;
    let rileys = scan(&owner_db, Some("riley-acct")).await;

    assert_eq!(
        node_trust(&host_db, HOST, Claimant::Remote).await.unwrap(),
        NodeTrust::OwnedBy(riley)
    );
    let first = claim(&host_db, HOST, Claimant::Remote)
        .await
        .expect("the host claims its owner's account");
    assert_eq!(first.job.id, rileys);
    assert!(
        claim(&host_db, HOST, Claimant::Remote).await.is_none(),
        "another user's job and the scan-all are refused"
    );
    // Handoff of the trusted job keeps the claim.
    let kept = handoff_routed_job(&host_db, &request(HOST, Claimant::Remote, HOST), first)
        .await
        .unwrap()
        .expect("still trusted at handoff");
    finish(&host_db, &kept).await;

    // A Host-role owner is no more trusted than a Member.
    owner_db.set_user_role(riley, UserRole::Host).await.unwrap();
    assert!(claim(&host_db, HOST, Claimant::Remote).await.is_none());

    owner_db
        .set_user_role(riley, UserRole::Administrator)
        .await
        .unwrap();
    assert_eq!(
        node_trust(&host_db, HOST, Claimant::Remote).await.unwrap(),
        NodeTrust::All
    );
    let mut claimed = Vec::new();
    while let Some(next) = claim(&host_db, HOST, Claimant::Remote).await {
        claimed.push(next.job.id.clone());
        finish(&host_db, &next).await;
    }
    claimed.sort();
    let mut expected = vec![all, sams];
    expected.sort();
    assert_eq!(claimed, expected, "promotion lets the host claim both");
}

/// A demotion that lands after the claim but before handoff denies: the job
/// returns to pending with no attempt spent and its permits released, and
/// the owner node then runs it.
async fn demotion_before_handoff_denies(owner_db: LibraryStore, host_db: LibraryStore) {
    let riley = user(&owner_db, UserRole::Administrator, "Riley").await;
    add_account(&owner_db, PLUGIN, "sam-acct", true).await;
    add_node(&owner_db, HOST, "host", Some(riley)).await;
    let job = scan(&owner_db, Some("sam-acct")).await;

    let host_request = request(HOST, Claimant::Remote, HOST);
    let claimed = claim(&host_db, HOST, Claimant::Remote)
        .await
        .expect("an Administrator's host claims any job");
    assert_eq!(claimed.job.id, job);
    assert_eq!(claimed.job.attempt_count, 1);

    owner_db
        .set_user_role(riley, UserRole::Member)
        .await
        .unwrap();
    let handed = handoff_routed_job(&host_db, &host_request, claimed.clone())
        .await
        .unwrap();
    assert!(handed.is_none(), "demotion before handoff denies");

    let row = owner_db.get_job(&job).await.unwrap().unwrap();
    assert_eq!(row.state, JobState::Pending);
    assert_eq!(row.attempt_count, 0, "a refused handoff spends no attempt");
    assert!(row.lease_owner.is_none());
    for permit in &claimed.permits {
        assert!(
            live_permits(&owner_db, &permit.semaphore, 8)
                .await
                .unwrap()
                .is_empty(),
            "{} was released",
            permit.semaphore
        );
    }
    // The refused claimer's fence is dead.
    let stale = claimed.job.fence().unwrap();
    assert!(!host_db.complete_job(&stale, None).await.unwrap());
    assert!(!host_db.release_job_claim(&stale, false).await.unwrap());

    assert!(claim(&host_db, HOST, Claimant::Remote).await.is_none());
    let local = claim(&owner_db, "node-owner", Claimant::Local)
        .await
        .expect("the owner node's worker runs it");
    assert_eq!(local.job.id, job);
    assert_eq!(local.job.attempt_count, 1);
}

/// Handoff requires the claim to still own the job's running lease: a claim
/// presented under another worker's owner, a claim whose lease expired, a
/// claim reclaimed by another node, and a finished claim hand nothing over,
/// and only the expired claim's own row and permits are released.
async fn stale_claim_handoff_denies(owner_db: LibraryStore, host_db: LibraryStore) {
    let riley = user(&owner_db, UserRole::Administrator, "Riley").await;
    add_node(&owner_db, HOST, "host", Some(riley)).await;
    let job = scan(&owner_db, Some("acct")).await;
    let host_request = request(HOST, Claimant::Remote, HOST);

    let claimed = claim(&host_db, HOST, Claimant::Remote)
        .await
        .expect("an Administrator's host claims any job");
    // Another worker's request may not take, or release, this claim.
    let other = request(HOST, Claimant::Remote, "other-worker");
    assert!(handoff_routed_job(&host_db, &other, claimed.clone())
        .await
        .unwrap()
        .is_none());
    let row = owner_db.get_job(&job).await.unwrap().unwrap();
    assert_eq!(row.state, JobState::Running);
    assert_eq!(row.fence(), claimed.job.fence(), "the row is left alone");
    for permit in &claimed.permits {
        assert_eq!(
            live_permits(&owner_db, &permit.semaphore, 8)
                .await
                .unwrap()
                .len(),
            1,
            "{} is still held",
            permit.semaphore
        );
    }

    // An expired lease hands nothing over; its own fence releases the row.
    jobs::Entity::update_many()
        .col_expr(
            jobs::Column::LeaseExpiresAt,
            Expr::value(Some(
                (chrono::Utc::now() - chrono::Duration::seconds(5)).to_rfc3339(),
            )),
        )
        .filter(jobs::Column::Id.eq(job.as_str()))
        .exec(owner_db.db())
        .await
        .unwrap();
    assert!(handoff_routed_job(&host_db, &host_request, claimed.clone())
        .await
        .unwrap()
        .is_none());
    let row = owner_db.get_job(&job).await.unwrap().unwrap();
    assert_eq!(row.state, JobState::Pending);
    assert_eq!(row.attempt_count, 0);

    // Once another node holds the job, the old claim hands nothing over and
    // does not disturb the new holder.
    let local = claim(&owner_db, "node-owner", Claimant::Local)
        .await
        .expect("the owner node's worker claims it");
    assert!(handoff_routed_job(&host_db, &host_request, claimed.clone())
        .await
        .unwrap()
        .is_none());
    let row = owner_db.get_job(&job).await.unwrap().unwrap();
    assert_eq!(row.state, JobState::Running);
    assert_eq!(row.fence(), local.job.fence());
    for permit in &local.permits {
        assert_eq!(
            live_permits(&owner_db, &permit.semaphore, 8)
                .await
                .unwrap()
                .len(),
            1,
            "{} is still held by the new claim",
            permit.semaphore
        );
    }

    // A finished claim hands nothing over either.
    finish(&owner_db, &local).await;
    let request_local = request("node-owner", Claimant::Local, "node-owner");
    assert!(handoff_routed_job(&owner_db, &request_local, local)
        .await
        .unwrap()
        .is_none());
    let row = owner_db.get_job(&job).await.unwrap().unwrap();
    assert_eq!(row.state, JobState::Succeeded);
}

/// Nodes that are trusted with nothing: an unknown remote node, a Host with
/// no owner, a disabled owner, and a revoked credential. An owner node is
/// trusted with everything.
async fn untrusted_nodes_claim_nothing(owner_db: LibraryStore, host_db: LibraryStore) {
    // Another Owner, so Riley is not the last one and may be disabled.
    user(&owner_db, UserRole::Owner, "Olive").await;
    let riley = user(&owner_db, UserRole::Owner, "Riley").await;
    add_account(&owner_db, PLUGIN, "acct", true).await;
    link(&owner_db, riley, PLUGIN, "acct").await;
    add_node(&owner_db, HOST, "host", Some(riley)).await;
    add_node(&owner_db, "host-orphan", "host", None).await;
    add_node(&owner_db, "owner-node", "owner", None).await;
    scan(&owner_db, Some("acct")).await;

    for (node, claimant) in [
        ("nobody", Claimant::Remote),
        ("", Claimant::Remote),
        ("host-orphan", Claimant::Remote),
    ] {
        assert_eq!(
            node_trust(&host_db, node, claimant).await.unwrap(),
            NodeTrust::Nothing,
            "{node}"
        );
        assert!(claim(&host_db, node, claimant).await.is_none(), "{node}");
    }
    assert_eq!(
        node_trust(&host_db, "owner-node", Claimant::Remote)
            .await
            .unwrap(),
        NodeTrust::All
    );
    // A Host row is judged by its owner whichever way it claims.
    assert_eq!(
        node_trust(&host_db, HOST, Claimant::Local).await.unwrap(),
        NodeTrust::All
    );

    owner_db
        .set_user_status(riley, UserStatus::Disabled)
        .await
        .unwrap();
    assert_eq!(
        node_trust(&host_db, HOST, Claimant::Remote).await.unwrap(),
        NodeTrust::Nothing
    );
    owner_db
        .set_user_status(riley, UserStatus::Active)
        .await
        .unwrap();
    assert_eq!(
        node_trust(&host_db, HOST, Claimant::Remote).await.unwrap(),
        NodeTrust::All
    );
    node_credentials::Entity::update_many()
        .col_expr(
            node_credentials::Column::RevokedAt,
            Expr::value(Some(chrono::Utc::now().to_rfc3339())),
        )
        .filter(node_credentials::Column::NodeId.eq(HOST))
        .exec(owner_db.db())
        .await
        .unwrap();
    assert_eq!(
        node_trust(&host_db, HOST, Claimant::Remote).await.unwrap(),
        NodeTrust::Nothing
    );
    assert!(claim(&host_db, HOST, Claimant::Remote).await.is_none());
}

/// Jobs a Member's Host is never trusted with, even for its owner's account.
async fn scoped_trust_refuses_unowned_secrets(owner_db: LibraryStore, host_db: LibraryStore) {
    let riley = user(&owner_db, UserRole::Member, "Riley").await;
    let sam = user(&owner_db, UserRole::Member, "Sam").await;
    add_account(&owner_db, PLUGIN, "shared-id", true).await;
    add_account(&owner_db, OTHER_PLUGIN, "shared-id", true).await;
    link(&owner_db, riley, PLUGIN, "shared-id").await;
    link(&owner_db, sam, OTHER_PLUGIN, "shared-id").await;
    add_node(&owner_db, HOST, "host", Some(riley)).await;
    let trust = node_trust(&host_db, HOST, Claimant::Remote).await.unwrap();
    assert_eq!(trust, NodeTrust::OwnedBy(riley));

    let job = |kind: JobKind, payload: JobPayload| crate::models::JobRecord {
        id: "j".into(),
        kind,
        state: JobState::Pending,
        priority: 0,
        resource_class: JobResourceClass::Network,
        payload,
        progress: None,
        attempt_count: 0,
        max_attempts: 3,
        run_after: chrono::Utc::now(),
        lease_owner: None,
        lease_expires_at: None,
        lease_generation: 0,
        dedup_key: String::new(),
        error_kind: None,
        error_message: None,
        cancel_requested: false,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        started_at: None,
        finished_at: None,
    };
    let account = |id: &str| JobPayload {
        account: Some(id.into()),
        ..Default::default()
    };
    // The id is held on two plugins, one of them Sam's: the job would touch both.
    assert!(
        !job_trusted(&host_db, &trust, &job(JobKind::Scan, account("shared-id")))
            .await
            .unwrap()
    );
    accounts::Entity::delete_many()
        .filter(accounts::Column::PluginKey.eq(OTHER_PLUGIN))
        .exec(owner_db.db())
        .await
        .unwrap();
    assert!(
        job_trusted(&host_db, &trust, &job(JobKind::Scan, account("shared-id")))
            .await
            .unwrap()
    );
    assert!(job_trusted(
        &host_db,
        &trust,
        &job(JobKind::Acquire, account("shared-id"))
    )
    .await
    .unwrap());
    // The executor matches an account needle by id or label, ignoring
    // case, so Sam's `SHARED-ID` or an account labeled `shared-id` would be
    // touched too.
    for (id, label) in [("SHARED-ID", None), ("sam-other", Some("Shared-Id"))] {
        add_account(&owner_db, OTHER_PLUGIN, id, true).await;
        if let Some(label) = label {
            accounts::Entity::update_many()
                .col_expr(accounts::Column::Label, Expr::value(label))
                .filter(accounts::Column::AccountId.eq(id))
                .exec(owner_db.db())
                .await
                .unwrap();
        }
        link(&owner_db, sam, OTHER_PLUGIN, id).await;
        assert!(
            !job_trusted(&host_db, &trust, &job(JobKind::Scan, account("shared-id")))
                .await
                .unwrap(),
            "{id} {label:?}"
        );
        accounts::Entity::delete_many()
            .filter(accounts::Column::PluginKey.eq(OTHER_PLUGIN))
            .exec(owner_db.db())
            .await
            .unwrap();
    }
    let refused = [
        job(JobKind::Scan, JobPayload::default()),
        job(JobKind::Acquire, JobPayload::default()),
        job(JobKind::Scan, account("unknown")),
        job(
            JobKind::Scan,
            JobPayload {
                plugin_instance_id: Some("pi_pinned".into()),
                ..account("shared-id")
            },
        ),
        job(
            JobKind::Acquire,
            JobPayload {
                requires: vec!["pi_local".into()],
                ..account("shared-id")
            },
        ),
        job(JobKind::ListenSync, JobPayload::default()),
        job(JobKind::IntegrationScan, JobPayload::default()),
        job(JobKind::PluginCopy, JobPayload::default()),
        job(JobKind::Invalid, account("shared-id")),
    ];
    for item in refused {
        assert!(
            !job_trusted(&host_db, &trust, &item).await.unwrap(),
            "{:?} {:?}",
            item.kind,
            item.payload
        );
    }
}

/// More untrusted jobs than one claim page queued ahead of a trusted one do
/// not hide it: untrusted rows are skipped and paging continues.
async fn untrusted_window_does_not_hide_trusted_jobs(
    owner_db: LibraryStore,
    host_db: LibraryStore,
) {
    let riley = user(&owner_db, UserRole::Member, "Riley").await;
    let sam = user(&owner_db, UserRole::Member, "Sam").await;
    add_node(&owner_db, HOST, "host", Some(riley)).await;
    let ahead = ROUTED_CLAIM_CANDIDATES + 1;
    for n in 0..ahead {
        let account = format!("sam-{n}");
        add_account(&owner_db, PLUGIN, &account, true).await;
        link(&owner_db, sam, PLUGIN, &account).await;
        scan(&owner_db, Some(&account)).await;
    }
    add_account(&owner_db, PLUGIN, "riley-1", true).await;
    link(&owner_db, riley, PLUGIN, "riley-1").await;
    let riley_job = scan(&owner_db, Some("riley-1")).await;

    let claimed = claim(&host_db, HOST, Claimant::Remote)
        .await
        .expect("the trusted job past the first page is claimed");
    assert_eq!(claimed.job.id, riley_job);
    finish(&host_db, &claimed).await;
    assert!(claim(&host_db, HOST, Claimant::Remote).await.is_none());
}

/// Scan-all is split per scan-enabled account for operators, Owners and
/// Administrators; anyone else scans only what they own.
async fn scan_planner_splits_and_scopes(owner_db: LibraryStore, host_db: LibraryStore) {
    user(&owner_db, UserRole::Owner, "Olive").await;
    let admin = user(&owner_db, UserRole::Administrator, "Ada").await;
    let riley = user(&owner_db, UserRole::Member, "Riley").await;
    let sam = user(&owner_db, UserRole::Host, "Sam").await;

    // With nothing to scan, a privileged scan stays one unsplit job.
    let empty = plan_scan(&host_db, ScanRequester::Operator, None, None)
        .await
        .unwrap();
    assert_eq!(empty.len(), 1);
    assert!(empty[0].account.is_none());
    assert!(plan_scan(&host_db, ScanRequester::User(riley), None, None)
        .await
        .unwrap()
        .is_empty());

    add_account(&owner_db, PLUGIN, "a1", true).await;
    add_account(&owner_db, PLUGIN, "a2", true).await;
    add_account(&owner_db, PLUGIN, "a3", false).await;
    add_account(&owner_db, PLUGIN, "shared", true).await;
    add_account(&owner_db, OTHER_PLUGIN, "shared", true).await;
    link(&owner_db, riley, PLUGIN, "a1").await;
    link(&owner_db, riley, PLUGIN, "a3").await;
    link(&owner_db, riley, PLUGIN, "shared").await;
    link(&owner_db, sam, PLUGIN, "a2").await;
    link(&owner_db, sam, OTHER_PLUGIN, "shared").await;

    let accounts_of = |plan: Vec<JobPayload>| -> Vec<String> {
        plan.into_iter()
            .map(|payload| payload.account.expect("split per account"))
            .collect()
    };
    for requester in [ScanRequester::Operator, ScanRequester::User(admin)] {
        let plan = plan_scan(&host_db, requester, None, None).await.unwrap();
        assert_eq!(accounts_of(plan), vec!["a1", "a2", "shared"]);
    }
    // Riley: a1 only (a3 is not scan-enabled; `shared` is also Sam's elsewhere).
    let plan = plan_scan(&host_db, ScanRequester::User(riley), None, None)
        .await
        .unwrap();
    assert_eq!(accounts_of(plan), vec!["a1"]);
    let plan = plan_scan(&host_db, ScanRequester::User(sam), None, None)
        .await
        .unwrap();
    assert_eq!(accounts_of(plan), vec!["a2"]);

    // A named account: explicit, so scan_enabled does not matter, but it must be theirs.
    let plan = plan_scan(&host_db, ScanRequester::User(riley), Some("a3"), None)
        .await
        .unwrap();
    assert_eq!(accounts_of(plan), vec!["a3"]);
    for named in ["a2", "shared", "missing"] {
        let err = plan_scan(&host_db, ScanRequester::User(riley), Some(named), None)
            .await
            .unwrap_err();
        assert!(matches!(err, LibraryError::Forbidden(_)), "{named}: {err}");
    }
    let plan = plan_scan(&host_db, ScanRequester::User(admin), Some("a2"), None)
        .await
        .unwrap();
    assert_eq!(accounts_of(plan), vec!["a2"]);

    // Promotion widens the plan; disabling refuses it.
    owner_db
        .set_user_role(riley, UserRole::Owner)
        .await
        .unwrap();
    let plan = plan_scan(&host_db, ScanRequester::User(riley), None, None)
        .await
        .unwrap();
    assert_eq!(accounts_of(plan), vec!["a1", "a2", "shared"]);
    owner_db
        .set_user_status(riley, UserStatus::Disabled)
        .await
        .unwrap();
    let err = plan_scan(&host_db, ScanRequester::User(riley), None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, LibraryError::Forbidden(_)), "{err}");
    let err = plan_scan(&host_db, ScanRequester::User(9_999), None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, LibraryError::Forbidden(_)), "{err}");

    // Each planned job is a separate scan that a trusted Host can claim.
    for payload in plan_scan(&host_db, ScanRequester::Operator, None, None)
        .await
        .unwrap()
    {
        let outcome = owner_db
            .enqueue_job(EnqueueJobSpec {
                kind: JobKind::Scan,
                payload,
                priority: 0,
                max_attempts: 3,
                max_pending: 256,
                run_after: None,
            })
            .await
            .unwrap();
        assert!(matches!(outcome, EnqueueOutcome::Created { .. }));
    }
    let pending = jobs::Entity::find()
        .filter(jobs::Column::State.eq("pending"))
        .all(owner_db.db())
        .await
        .unwrap();
    assert_eq!(pending.len(), 3);
}

#[tokio::test]
async fn sqlite_host_trust_follows_its_owners_role() {
    let (_dir, a, b) = sqlite_stores().await;
    host_trust_follows_its_owners_role(a, b).await;
}

#[tokio::test]
async fn sqlite_demotion_before_handoff_denies() {
    let (_dir, a, b) = sqlite_stores().await;
    demotion_before_handoff_denies(a, b).await;
}

#[tokio::test]
async fn sqlite_stale_claim_handoff_denies() {
    let (_dir, a, b) = sqlite_stores().await;
    stale_claim_handoff_denies(a, b).await;
}

#[tokio::test]
async fn sqlite_untrusted_nodes_claim_nothing() {
    let (_dir, a, b) = sqlite_stores().await;
    untrusted_nodes_claim_nothing(a, b).await;
}

#[tokio::test]
async fn sqlite_scoped_trust_refuses_unowned_secrets() {
    let (_dir, a, b) = sqlite_stores().await;
    scoped_trust_refuses_unowned_secrets(a, b).await;
}

#[tokio::test]
async fn sqlite_untrusted_window_does_not_hide_trusted_jobs() {
    let (_dir, a, b) = sqlite_stores().await;
    untrusted_window_does_not_hide_trusted_jobs(a, b).await;
}

#[tokio::test]
async fn sqlite_scan_planner_splits_and_scopes() {
    let (_dir, a, b) = sqlite_stores().await;
    scan_planner_splits_and_scopes(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_host_trust_follows_its_owners_role() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    host_trust_follows_its_owners_role(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_demotion_before_handoff_denies() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    demotion_before_handoff_denies(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_stale_claim_handoff_denies() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    stale_claim_handoff_denies(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_untrusted_nodes_claim_nothing() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    untrusted_nodes_claim_nothing(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_scoped_trust_refuses_unowned_secrets() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    scoped_trust_refuses_unowned_secrets(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_scan_planner_splits_and_scopes() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    scan_planner_splits_and_scopes(a, b).await;
}

#[tokio::test]
#[ignore = "requires BOOKCLERK_TEST_POSTGRES_URL and a disposable Postgres"]
async fn postgres_untrusted_window_does_not_hide_trusted_jobs() {
    if !postgres_tests_enabled() {
        return;
    }
    let (a, b) = postgres_stores().await;
    untrusted_window_does_not_hide_trusted_jobs(a, b).await;
}
