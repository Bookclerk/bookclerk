//! Interlibrary requests: a request goes to the branch that listed the
//! work, only that branch claims it, once, by identifier; its answer is
//! recorded under the lease it was claimed with; and on the branch a
//! requested member's book is shared with the library that asked, and no
//! other while it still lists it. A request waits for an offline branch
//! and ends once the branch is revoked.

use bookclerk_node_proto::{
    Availability, CatalogMetadata, LookupAnswer, LookupEdition, LookupIdentifier, LookupScheme,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set};

use super::*;
use crate::control_plane::{
    ask_branches, claimant_may_run, pending_questions, record_answers, AnswersRecorded, Claimant,
    OwnerShareFilter,
};
use crate::entities::{node_credentials, nodes};
use crate::models::{AcquireStatus, EnqueueJobSpec, EnqueueOutcome, JobState, UserRole};
use crate::store::NewBook;
use crate::test_support::Profile;

const PLUGIN: &str = "path:file:///tmp/store#audible";
const BRANCH: &str = "branch-b";
const OTHER_BRANCH: &str = "branch-c";

async fn store(profile: Profile) -> LibraryStore {
    crate::test_support::sqlite::memory(profile)
        .await
        .into_store()
}

async fn add_branch(store: &LibraryStore, node_id: &str, owner: i64) {
    let now = Utc::now().to_rfc3339();
    nodes::ActiveModel {
        node_id: Set(node_id.into()),
        cluster_id: Set("cluster".into()),
        created_at: Set(now.clone()),
        incarnation: Set("test".into()),
        heartbeat_at: Set(now.clone()),
        software_version: Set("1.0.0".into()),
        schema_state: Set("current".into()),
        compatible: Set(1),
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

fn book(account: &str, asin: &str) -> NewBook {
    NewBook {
        uuid: None,
        product_id: asin.into(),
        plugin_key: PLUGIN.into(),
        source: "audible".into(),
        account_id: account.into(),
        asin: Some(asin.into()),
        isbn: None,
        marketplace: "us".into(),
        title: format!("Title {asin}"),
        authors: Some("Ann Author".into()),
        narrators: Some("Ned Narrator".into()),
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: None,
        length_minutes: Some(600),
        is_abridged: false,
        content_kind: "book".into(),
        categories: None,
        subtitle: None,
        published_at: None,
    }
}

/// Records `asin` as a book of `user` on `account` and returns its row.
async fn hold(store: &LibraryStore, user: i64, account: &str, asin: &str) -> BookRecord {
    store
        .upsert_account(PLUGIN, account, "us", None, true, "audible")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, account, "test")
        .await
        .unwrap();
    store.upsert_book(&book(account, asin)).await.unwrap()
}

/// The work of `book`.
async fn work_of(store: &LibraryStore, book: &BookRecord) -> String {
    store.work_id_for_book(&book.uuid).await.unwrap().unwrap()
}

/// Has [`BRANCH`] answer that it can supply `asin` for `work`.
async fn listed(store: &LibraryStore, work: &str, asin: &str) {
    ask_branches(store, &[work.to_string()]).await.unwrap();
    pending_questions(store, BRANCH).await.unwrap().unwrap();
    let answer = LookupAnswer {
        work: work.into(),
        editions: vec![LookupEdition {
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: asin.into(),
            availability: Availability::Owned,
            formats: Vec::new(),
            metadata: CatalogMetadata {
                title: format!("Title {asin}"),
                asin: Some(asin.into()),
                ..CatalogMetadata::default()
            },
        }],
    };
    match record_answers(store, BRANCH, &[answer], &[]).await.unwrap() {
        AnswersRecorded::Recorded(report) => assert_eq!(report.listings, 1),
        other => panic!("expected the answer recorded: {other:?}"),
    }
}

/// Admits the planned request and returns its job id.
async fn admit(store: &LibraryStore, plan: BranchRequestPlan) -> String {
    match store
        .enqueue_job(EnqueueJobSpec {
            kind: JobKind::BranchRequest,
            payload: plan.payload,
            priority: 0,
            max_attempts: 3,
            max_pending: 100,
            run_after: None,
        })
        .await
        .unwrap()
    {
        EnqueueOutcome::Created { id } => id,
        other => panic!("expected a new job: {other:?}"),
    }
}

/// A library whose member owns [`BRANCH`], with one work [`BRANCH`]
/// listed: the store, the member and the work.
async fn library_with_listing(profile: Profile, asin: &str) -> (LibraryStore, i64, String) {
    let store = store(profile).await;
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    add_branch(&store, BRANCH, member).await;
    let held = hold(&store, member, "riley-acct", asin).await;
    let work = work_of(&store, &held).await;
    listed(&store, &work, asin).await;
    (store, member, work)
}

#[tokio::test]
async fn a_request_goes_to_its_branch_which_claims_it_once_by_identifier() {
    for profile in Profile::ALL {
        let name = profile.name();
        let (store, _, work) = library_with_listing(profile, "B0REQUEST1").await;

        let plan = plan_branch_request(&store, &work, None, JobTrigger::Api)
            .await
            .unwrap();
        assert_eq!(plan.node_id, BRANCH, "{name}");
        assert_eq!(plan.payload.node_id.as_deref(), Some(BRANCH), "{name}");
        assert_eq!(
            plan.payload.branch_request,
            Some(BranchRequestTarget {
                work_id: work.clone(),
                store: "audible".into(),
                marketplace: "us".into(),
                product_id: "B0REQUEST1".into(),
            }),
            "{name}"
        );
        let job_id = admit(&store, plan.clone()).await;
        // Asking again while it waits folds into the same job.
        assert_eq!(
            store
                .enqueue_job(EnqueueJobSpec {
                    kind: JobKind::BranchRequest,
                    payload: plan.payload,
                    priority: 0,
                    max_attempts: 3,
                    max_pending: 100,
                    run_after: None,
                })
                .await
                .unwrap(),
            EnqueueOutcome::Duplicate {
                existing_id: job_id.clone()
            },
            "{name}"
        );
        // No claim path of the library hands it to a worker.
        let job = store.get_job(&job_id).await.unwrap().unwrap();
        assert!(!claimant_may_run(Claimant::Local, &job), "{name}");
        assert!(!claimant_may_run(Claimant::Remote, &job), "{name}");

        let claimed = claim_branch_requests(&store, BRANCH)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.len(), 1, "{name}");
        let request = &claimed[0];
        assert_eq!(request.job, job_id, "{name}");
        assert_eq!(request.problem(), None, "{name}");
        assert_eq!(request.work.work, work, "{name}");
        assert!(
            request
                .work
                .identifiers
                .iter()
                .any(|id| id.scheme == LookupScheme::Asin
                    && id.value == "B0REQUEST1"
                    && id.marketplace == "us"),
            "{name}: {:?}",
            request.work.identifiers
        );
        assert_eq!(
            (
                request.store.as_str(),
                request.marketplace.as_str(),
                request.product_id.as_str()
            ),
            ("audible", "us", "B0REQUEST1"),
            "{name}"
        );
        let running = store.get_job(&job_id).await.unwrap().unwrap();
        assert_eq!(running.state, JobState::Running, "{name}");
        assert_eq!(running.lease_generation, request.generation, "{name}");
        // Claimed: not handed out again.
        assert!(
            claim_branch_requests(&store, BRANCH)
                .await
                .unwrap()
                .unwrap()
                .is_empty(),
            "{name}"
        );

        assert_eq!(
            finish_branch_request(
                &store,
                BRANCH,
                &job_id,
                request.generation,
                RequestOutcome::Accepted
            )
            .await
            .unwrap(),
            RequestFinished::Recorded,
            "{name}"
        );
        let done = store.get_job(&job_id).await.unwrap().unwrap();
        assert_eq!(done.state, JobState::Succeeded, "{name}");
        assert_eq!(done.progress.as_deref(), Some(REQUEST_ACCEPTED), "{name}");
        // The same answer again finds the lease gone.
        assert_eq!(
            finish_branch_request(
                &store,
                BRANCH,
                &job_id,
                request.generation,
                RequestOutcome::Accepted
            )
            .await
            .unwrap(),
            RequestFinished::Lost,
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_work_no_branch_lists_cannot_be_requested() {
    let (store, member, work) = library_with_listing(Profile::FirstParty, "B0REQUEST2").await;
    let unlisted = hold(&store, member, "riley-acct", "B0UNLISTED").await;
    let unlisted = work_of(&store, &unlisted).await;
    assert!(matches!(
        plan_branch_request(&store, &unlisted, None, JobTrigger::Api).await,
        Err(LibraryError::NotFound(_))
    ));
    assert!(matches!(
        plan_branch_request(&store, &work, Some(OTHER_BRANCH), JobTrigger::Api).await,
        Err(LibraryError::NotFound(_))
    ));
    assert!(
        plan_branch_request(&store, &work, Some(BRANCH), JobTrigger::Api)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn not_listed_ends_the_request_and_a_branch_failure_retries_it() {
    let store = store(Profile::FirstParty).await;
    let member = store
        .create_user(UserRole::Member, Some("Riley"), None)
        .await
        .unwrap()
        .id;
    add_branch(&store, BRANCH, member).await;
    let mut jobs = Vec::new();
    for asin in ["B0OUTCOME1", "B0OUTCOME2"] {
        let held = hold(&store, member, "riley-acct", asin).await;
        let work = work_of(&store, &held).await;
        listed(&store, &work, asin).await;
        let plan = plan_branch_request(&store, &work, None, JobTrigger::Api)
            .await
            .unwrap();
        jobs.push(admit(&store, plan).await);
    }
    let claimed = claim_branch_requests(&store, BRANCH)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.len(), 2);
    let generation_of = |job: &str| {
        claimed
            .iter()
            .find(|r| r.job == job)
            .map(|r| r.generation)
            .unwrap()
    };

    assert_eq!(
        finish_branch_request(
            &store,
            BRANCH,
            &jobs[0],
            generation_of(&jobs[0]),
            RequestOutcome::NotListed
        )
        .await
        .unwrap(),
        RequestFinished::Recorded
    );
    let ended = store.get_job(&jobs[0]).await.unwrap().unwrap();
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(ended.error_kind.as_deref(), Some(REQUEST_NOT_LISTED));
    assert!(ended.attempt_count < ended.max_attempts);

    assert_eq!(
        finish_branch_request(
            &store,
            BRANCH,
            &jobs[1],
            generation_of(&jobs[1]),
            RequestOutcome::Failed
        )
        .await
        .unwrap(),
        RequestFinished::Recorded
    );
    let retried = store.get_job(&jobs[1]).await.unwrap().unwrap();
    assert_eq!(retried.state, JobState::Pending);
    assert_eq!(retried.error_kind.as_deref(), Some(REQUEST_BRANCH_FAILED));
}

#[tokio::test]
async fn only_the_listing_branch_claims_or_answers_a_request() {
    let (store, member, work) = library_with_listing(Profile::FirstParty, "B0REQUEST3").await;
    add_branch(&store, OTHER_BRANCH, member).await;
    let plan = plan_branch_request(&store, &work, None, JobTrigger::Api)
        .await
        .unwrap();
    let job_id = admit(&store, plan).await;

    // An unknown node is refused outright.
    assert!(claim_branch_requests(&store, "stranger")
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        finish_branch_request(&store, "stranger", &job_id, 1, RequestOutcome::Accepted)
            .await
            .unwrap(),
        RequestFinished::Refused
    );
    // Another trusted branch sees nothing and cannot settle it.
    assert!(claim_branch_requests(&store, OTHER_BRANCH)
        .await
        .unwrap()
        .unwrap()
        .is_empty());
    let claimed = claim_branch_requests(&store, BRANCH)
        .await
        .unwrap()
        .unwrap();
    let generation = claimed[0].generation;
    assert_eq!(
        finish_branch_request(
            &store,
            OTHER_BRANCH,
            &job_id,
            generation,
            RequestOutcome::Accepted
        )
        .await
        .unwrap(),
        RequestFinished::Lost
    );
    // A stale generation is lost too.
    assert_eq!(
        finish_branch_request(
            &store,
            BRANCH,
            &job_id,
            generation + 1,
            RequestOutcome::Accepted
        )
        .await
        .unwrap(),
        RequestFinished::Lost
    );
    assert_eq!(
        store.get_job(&job_id).await.unwrap().unwrap().state,
        JobState::Running
    );
}

#[tokio::test]
async fn a_branch_sends_a_requested_members_book_to_the_library_that_asked() {
    for profile in Profile::ALL {
        let name = profile.name();
        let store = store(profile).await;
        let owner = store
            .create_user(UserRole::Owner, Some("Owner"), None)
            .await
            .unwrap();
        let member = store
            .create_user(UserRole::Member, Some("Riley"), None)
            .await
            .unwrap();
        hold(&store, owner.id, "owner-acct", "B0OWNBOOK1").await;
        let members = hold(&store, member.id, "riley-acct", "B0MEMBOOK1").await;
        // The link to library-x, sharing and listing members' books (its
        // keys don't matter here).
        crate::entities::cluster_links::ActiveModel {
            owner_cluster_id: Set("library-x".into()),
            owner_origin: Set("https://x.example".into()),
            remote_node_id: Set("node-x".into()),
            spki_pin: Set(None),
            state: Set("active".into()),
            enrolled_at: Set(Utc::now().to_rfc3339()),
            share_books: Set(1),
            include_branches: Set(1),
            ancestry_json: Set("[]".into()),
            work_for_members: Set(0),
            keep_member_copies: Set(0),
            max_jobs: Set(None),
            list_members_books: Set(1),
            person_user_id: Set(None),
        }
        .insert(store.db())
        .await
        .unwrap();
        let request = BranchRequest {
            job: "branch_request-1".into(),
            generation: 1,
            work: LookupWork {
                work: "x-work".into(),
                identifiers: vec![LookupIdentifier {
                    scheme: LookupScheme::Asin,
                    value: "B0MEMBOOK1".into(),
                    marketplace: "us".into(),
                }],
            },
            store: "audible".into(),
            marketplace: "us".into(),
            product_id: "B0MEMBOOK1".into(),
        };
        // Listed only while the link lists members' books.
        assert!(
            crate::control_plane::book_for_request(&store, &request, false)
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );
        let found = crate::control_plane::book_for_request(&store, &request, true)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.uuid, members.uuid, "{name}");
        // Another edition of the work is not the requested one.
        let other = BranchRequest {
            product_id: "B0ELSEWHERE".into(),
            ..request.clone()
        };
        assert!(
            crate::control_plane::book_for_request(&store, &other, true)
                .await
                .unwrap()
                .is_none(),
            "{name}"
        );

        for (uuid, account) in [(&members.uuid, "riley-acct")] {
            store
                .set_acquire_status(uuid, account, AcquireStatus::Acquired, Some("m.m4b"), None)
                .await
                .unwrap();
        }
        let keys = store.acquired_book_keys_page(None, 100).await.unwrap();
        let key = keys.iter().find(|k| k.uuid == members.uuid).unwrap();
        let asked = OwnerShareFilter::load(&store, "library-x").await.unwrap();
        assert!(
            !asked.admits(key),
            "{name}: a member's book is not sent unasked"
        );

        record_link_request(&store, "library-x", &found)
            .await
            .unwrap();
        // Recording twice keeps one row.
        record_link_request(&store, "library-x", &found)
            .await
            .unwrap();
        assert_eq!(
            books_requested_by_link(&store, "library-x")
                .await
                .unwrap()
                .len(),
            1,
            "{name}"
        );
        let asked = OwnerShareFilter::load(&store, "library-x").await.unwrap();
        assert!(asked.admits(key), "{name}: sent to the library that asked");
        // Ordered by the request, so a book already past the sync's cursor
        // still goes up; asking again moves it later.
        let first = asked.requested_at(key).unwrap();
        record_link_request(&store, "library-x", &found)
            .await
            .unwrap();
        let again = OwnerShareFilter::load(&store, "library-x")
            .await
            .unwrap()
            .requested_at(key)
            .unwrap();
        assert!(again >= first, "{name}");
        let other_library = OwnerShareFilter::load(&store, "library-y").await.unwrap();
        assert!(
            !other_library.admits(key),
            "{name}: not sent to another library"
        );

        // The listing rules still apply after the request: an opt-out, or
        // members' books turned off, stops the book being sent.
        crate::control_plane::set_catalog_listing_opt_out(&store, member.id, true)
            .await
            .unwrap();
        let opted_out = OwnerShareFilter::load(&store, "library-x").await.unwrap();
        assert!(!opted_out.admits(key), "{name}: not sent after an opt-out");
        crate::control_plane::set_catalog_listing_opt_out(&store, member.id, false)
            .await
            .unwrap();
        assert!(
            crate::control_plane::set_cluster_link_listing(&store, "library-x", false)
                .await
                .unwrap(),
            "{name}"
        );
        let members_off = OwnerShareFilter::load(&store, "library-x").await.unwrap();
        assert!(
            !members_off.admits(key),
            "{name}: not sent once members' books are off"
        );
    }
}

#[tokio::test]
async fn a_request_waits_for_an_offline_branch_and_ends_once_it_is_revoked() {
    for profile in Profile::ALL {
        let name = profile.name();
        let (store, _member, work) = library_with_listing(profile, "B0SWEEP001").await;
        let job = admit(
            &store,
            plan_branch_request(&store, &work, None, JobTrigger::Api)
                .await
                .unwrap(),
        )
        .await;
        let sweep = |store: LibraryStore| async move {
            crate::control_plane::sweep_job_requirements(
                &store,
                &std::collections::BTreeSet::new(),
                std::time::Duration::from_secs(60),
                Utc::now() + chrono::Duration::days(1),
            )
            .await
            .unwrap()
        };
        // The branch has sent no heartbeat at all: the request still waits.
        sweep(store.clone()).await;
        let waiting = store.get_job(&job).await.unwrap().unwrap();
        assert_eq!(waiting.state, JobState::Pending, "{name}");

        // Revoked: the request ends.
        let mut credential: node_credentials::ActiveModel =
            node_credentials::Entity::find_by_id(BRANCH.to_string())
                .one(store.db())
                .await
                .unwrap()
                .unwrap()
                .into();
        credential.revoked_at = Set(Some(Utc::now().to_rfc3339()));
        credential.update(store.db()).await.unwrap();
        let report = sweep(store.clone()).await;
        assert_eq!(report.failed, 1, "{name}");
        let ended = store.get_job(&job).await.unwrap().unwrap();
        assert_eq!(ended.state, JobState::Failed, "{name}");
    }
}
