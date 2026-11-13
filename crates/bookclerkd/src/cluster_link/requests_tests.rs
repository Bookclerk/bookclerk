//! A member of a library asks for a book its branch listed, end to end in
//! one process: A queues the request, B claims it over the node socket,
//! finds its member's copy, queues the download and records the request so
//! its catalog sync sends the book to A, and A records the answer on the
//! job. Once B stops listing members' books, a request is answered "not
//! listed" and ends.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use bookclerk_library::control_plane::{
    ask_branches, books_requested_by_link, load_cluster_link_keys, plan_branch_request,
    set_cluster_link_download, set_cluster_link_listing, ClusterLink, OwnerShareFilter,
    REQUEST_ACCEPTED, REQUEST_NOT_LISTED,
};
use bookclerk_library::{
    BookRecord, EnqueueJobSpec, EnqueueOutcome, JobKind, JobState, JobTrigger, LibraryStore,
    NewBook, UserRole,
};
use bookclerk_node_client::{HostClient, HostClientConfig, HostClientHandle};
use tokio::sync::watch;

use super::super::catalog_lookup::CatalogAnswerer;
use super::super::remote_bindings::RemoteCatalog;
use super::super::tests::{
    connected, host_store, join, owner_account, public_origin, serve, timing, FixedSampler,
};
use super::super::TransportRunner;
use super::*;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// The ASIN both libraries hold.
const ASIN: &str = "B0REQ00001";

/// The store plugin key both libraries use.
const PLUGIN: &str = "path:file:///tmp/audible";

/// A link client that claims no jobs.
struct NoJobs;

impl JobTransport for NoJobs {
    async fn execute(&self, _cmd: JobCommand, _ctx: JobExecCtx) -> anyhow::Result<String> {
        anyhow::bail!("this test claims no jobs")
    }
}

/// Records the books it was asked to download.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<String>>>);

impl RequestAcquirer for Recorder {
    async fn acquire(&self, book: &BookRecord) -> anyhow::Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned"))?
            .push(book.uuid.clone());
        Ok(())
    }
}

/// Starts `link`'s client and waits until it is welcomed.
async fn start(host: &LibraryStore, link: &ClusterLink) -> (HostClientHandle, watch::Sender<bool>) {
    let keys = load_cluster_link_keys(host, link).await.unwrap();
    let (client, handle) = HostClient::new(
        HostClientConfig {
            link: link.clone(),
            keys,
            software_version: "1.0.0".into(),
            incarnation: uuid::Uuid::new_v4().to_string(),
            max_jobs: 1,
            share: None,
            claim_kinds: Vec::new(),
            claims_after_refresh: false,
            timing: timing(Duration::from_millis(200)),
        },
        TransportRunner::new(NoJobs, Arc::new(RemoteCatalog::default())),
        Box::new(FixedSampler),
    )
    .unwrap();
    let (stop, shutdown) = watch::channel(false);
    tokio::spawn(client.run(shutdown));
    connected(&handle, 1).await;
    (handle, stop)
}

/// An Audible book with [`ASIN`] on `account_id`.
fn audible_book(account_id: &str) -> NewBook {
    NewBook {
        uuid: None,
        product_id: ASIN.into(),
        plugin_key: PLUGIN.into(),
        source: "audible".into(),
        account_id: account_id.into(),
        asin: Some(ASIN.into()),
        isbn: None,
        marketplace: "us".into(),
        title: "Requested Title".into(),
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

/// Records [`ASIN`] in `store` on `account_id`, linked to `user`.
async fn hold(store: &LibraryStore, user: i64, account_id: &str) -> BookRecord {
    store
        .upsert_account(PLUGIN, account_id, "us", None, true, "test")
        .await
        .unwrap();
    let identity = store
        .ensure_local_portal_identity(user, None)
        .await
        .unwrap();
    store
        .link_account(identity.id, PLUGIN, account_id, "test")
        .await
        .unwrap();
    store
        .scope(PLUGIN, "audible")
        .upsert_book(&audible_book(account_id))
        .await
        .unwrap()
}

/// Plans and admits a request for `work` on `library`; returns its job id.
async fn request(library: &LibraryStore, work: &str) -> String {
    let plan = plan_branch_request(library, work, None, JobTrigger::Api)
        .await
        .unwrap();
    match library
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
        other => panic!("expected a new request: {other:?}"),
    }
}

#[tokio::test]
async fn a_branch_takes_its_librarys_request_for_a_members_book() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    // A: the library, with Riley, who owns branch B and holds the work.
    let a_files = tempfile::tempdir().unwrap();
    let (a_state, a) = crate::node_api::tests::test_state(&a_files).await;
    let a_addr = serve(crate::api::router(Arc::clone(&a_state), None)).await;
    let a_origin = public_origin(&a_state, &format!("http://127.0.0.1:{}", a_addr.port())).await;
    let (riley, _token) = owner_account(&a, b"riley-token-v1").await;
    let a_book = hold(&a, riley, "a-store-acct").await;
    let work = a.work_id_for_book(&a_book.uuid).await.unwrap().unwrap();

    // B: a branch whose member Sam holds the same edition, not downloaded.
    let b_files = tempfile::tempdir().unwrap();
    let b = host_store(b_files.path()).await;
    let sam = b
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap();
    let sams = hold(&b, sam.id, "sam-acct").await;
    let link = join(&a, &b, riley, &a_origin, None).await.unwrap();

    // B lists it to A.
    ask_branches(&a, std::slice::from_ref(&work)).await.unwrap();
    let (handle, _stop) = start(&b, &link).await;
    let answerer = CatalogAnswerer {
        library: b.clone(),
        cluster_id: link.owner_cluster_id.clone(),
        handle: handle.clone(),
    };
    assert_eq!(answerer.answer_once().await.unwrap(), 1);

    // A member of A asks for it.
    let job = request(&a, &work).await;
    let recorder = Recorder::default();
    let taker = RequestTaker {
        library: b.clone(),
        cluster_id: link.owner_cluster_id.clone(),
        handle,
        acquirer: recorder.clone(),
    };
    assert_eq!(
        taker.take_once().await.unwrap(),
        TakeReport {
            accepted: 1,
            ..TakeReport::default()
        }
    );
    assert_eq!(
        taker.take_once().await.unwrap(),
        TakeReport::default(),
        "nothing left"
    );
    let done = a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(done.state, JobState::Succeeded);
    assert_eq!(done.progress.as_deref(), Some(REQUEST_ACCEPTED));
    // B queued Sam's copy and records it as requested by A only.
    assert_eq!(*recorder.0.lock().unwrap(), vec![sams.uuid.clone()]);
    let requested = books_requested_by_link(&b, &link.owner_cluster_id)
        .await
        .unwrap();
    assert_eq!(requested.len(), 1);
    let entry = requested.keys().next().unwrap();
    assert_eq!((entry.0.as_str(), entry.3.as_str()), (PLUGIN, "sam-acct"));
    b.set_acquire_status(
        &sams.uuid,
        "sam-acct",
        bookclerk_library::AcquireStatus::Acquired,
        Some("sam.m4b"),
        None,
    )
    .await
    .unwrap();
    let keys = b.acquired_book_keys_page(None, 100).await.unwrap();
    let key = keys.iter().find(|k| k.uuid == sams.uuid).unwrap();
    assert!(OwnerShareFilter::load(&b, &link.owner_cluster_id)
        .await
        .unwrap()
        .admits(key));

    // Members' books off: B no longer lists Sam's copy, so a new request
    // is answered "not listed" and ends.
    assert!(set_cluster_link_listing(&b, &link.owner_cluster_id, false)
        .await
        .unwrap());
    let job = request(&a, &work).await;
    assert_eq!(
        taker.take_once().await.unwrap(),
        TakeReport {
            not_listed: 1,
            ..TakeReport::default()
        }
    );
    let ended = a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(ended.state, JobState::Failed);
    assert_eq!(ended.error_kind.as_deref(), Some(REQUEST_NOT_LISTED));
    assert_eq!(recorder.0.lock().unwrap().len(), 1, "nothing more queued");

    // Members' books back on, but B becomes a window of A (download off):
    // a window is a client of A, not one of its nodes, and refuses the
    // work.
    assert!(set_cluster_link_listing(&b, &link.owner_cluster_id, true)
        .await
        .unwrap());
    assert!(set_cluster_link_download(&b, &link.owner_cluster_id, false)
        .await
        .unwrap());
    let job = request(&a, &work).await;
    assert_eq!(
        taker.take_once().await.unwrap(),
        TakeReport {
            not_listed: 1,
            ..TakeReport::default()
        }
    );
    let refused = a.get_job(&job).await.unwrap().unwrap();
    assert_eq!(refused.error_kind.as_deref(), Some(REQUEST_NOT_LISTED));
    assert_eq!(
        recorder.0.lock().unwrap().len(),
        1,
        "a window queues nothing"
    );
}

#[test]
fn only_a_requests_wake_starts_a_pass() {
    assert!(is_requests_queued(
        &serde_json::json!({"type": "requests.queued"})
    ));
    assert!(!is_requests_queued(
        &serde_json::json!({"type": "catalog.asked"})
    ));
    assert!(!is_requests_queued(&serde_json::json!({})));
}
