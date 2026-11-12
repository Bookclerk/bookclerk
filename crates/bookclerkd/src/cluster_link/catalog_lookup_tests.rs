//! A library asks its branch about a work, end to end in one process: A
//! asks, B pulls the question over the node socket, answers from its own
//! library, and A records the answer as a `branch:<B>` listing of that
//! work in its listing pool. B's member's book is listed until the link's
//! "List my members' books" setting is off.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    ask_branches, branch_listings_for_works, load_cluster_link_keys, set_cluster_link_listing,
    ClusterLink,
};
use bookclerk_library::{LibraryStore, NewBook, UserRole};
use bookclerk_node_client::{HostClient, HostClientConfig, HostClientHandle};
use sea_orm::ConnectionTrait as _;
use tokio::sync::watch;

use super::super::remote_bindings::RemoteCatalog;
use super::super::tests::{
    connected, host_store, join, owner_account, public_origin, serve, timing, FixedSampler,
};
use super::super::TransportRunner;
use super::*;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// The ASIN both libraries hold.
const ASIN: &str = "B0E2E00001";

/// A link client that claims no jobs.
struct NoJobs;

impl JobTransport for NoJobs {
    async fn execute(&self, _cmd: JobCommand, _ctx: JobExecCtx) -> anyhow::Result<String> {
        anyhow::bail!("this test claims no jobs")
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
fn audible_book(plugin: &str, account_id: &str) -> NewBook {
    NewBook {
        uuid: None,
        product_id: ASIN.into(),
        plugin_key: plugin.into(),
        source: "audible".into(),
        account_id: account_id.into(),
        asin: Some(ASIN.into()),
        isbn: None,
        marketplace: "us".into(),
        title: "Shared Title".into(),
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

/// Records [`ASIN`] in `store` on `account_id` (linked to `user` when
/// given) and returns its work id.
async fn hold(store: &LibraryStore, user: Option<i64>, account_id: &str) -> String {
    let plugin = "path:file:///tmp/audible";
    store
        .upsert_account(plugin, account_id, "us", None, true, "test")
        .await
        .unwrap();
    if let Some(user) = user {
        let identity = store
            .ensure_local_portal_identity(user, None)
            .await
            .unwrap();
        store
            .link_account(identity.id, plugin, account_id, "test")
            .await
            .unwrap();
    }
    let record = store
        .scope(plugin, "audible")
        .upsert_book(&audible_book(plugin, account_id))
        .await
        .unwrap();
    store.work_id_for_book(&record.uuid).await.unwrap().unwrap()
}

#[tokio::test]
async fn a_branch_answers_its_librarys_question_over_the_socket() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    // A: the library, with Riley, who owns branch B and holds the work.
    let a_files = tempfile::tempdir().unwrap();
    let (a_state, a) = crate::node_api::tests::test_state(&a_files).await;
    let a_addr = serve(crate::api::router(Arc::clone(&a_state), None)).await;
    let a_origin = public_origin(&a_state, &format!("http://127.0.0.1:{}", a_addr.port())).await;
    let (riley, _token) = owner_account(&a, b"riley-token-v1").await;
    let work = hold(&a, Some(riley), "a-store-acct").await;

    // B: a branch whose member Sam holds the same edition.
    let b_files = tempfile::tempdir().unwrap();
    let b = host_store(b_files.path()).await;
    let sam = b
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap();
    hold(&b, Some(sam.id), "sam-acct").await;
    let link = join(&a, &b, riley, &a_origin, None).await.unwrap();
    assert!(link.list_members_books, "on by default");

    let asked = ask_branches(&a, std::slice::from_ref(&work)).await.unwrap();
    assert_eq!((asked.branches, asked.asked), (1, 1));

    let (handle, _stop) = start(&b, &link).await;
    let answerer = CatalogAnswerer {
        library: b.clone(),
        cluster_id: link.owner_cluster_id.clone(),
        handle,
    };
    assert_eq!(answerer.answer_once().await.unwrap(), 1);
    assert_eq!(answerer.answer_once().await.unwrap(), 0, "nothing left");

    let listings = branch_listings_for_works(&a, std::slice::from_ref(&work))
        .await
        .unwrap();
    assert_eq!(listings.len(), 1, "{listings:?}");
    assert_eq!(listings[0].node_id, link.remote_node_id);
    assert_eq!(listings[0].work_id, work);
    assert_eq!(
        (listings[0].store.as_str(), listings[0].product_id.as_str()),
        ("audible", ASIN)
    );

    // Members' books off: B answers "missing" and A's listing expires.
    assert!(set_cluster_link_listing(&b, &link.owner_cluster_id, false)
        .await
        .unwrap());
    a.db()
        .execute_unprepared("UPDATE catalog_lookups SET answered_at = '1970-01-01T00:00:00Z'")
        .await
        .unwrap();
    assert_eq!(
        ask_branches(&a, std::slice::from_ref(&work))
            .await
            .unwrap()
            .asked,
        1
    );
    assert_eq!(answerer.answer_once().await.unwrap(), 1);
    assert!(branch_listings_for_works(&a, std::slice::from_ref(&work))
        .await
        .unwrap()
        .is_empty());
}

#[test]
fn large_answer_batches_are_split_into_frames_that_fit() {
    use bookclerk_node_proto::{
        Availability, Body, CatalogMetadata, Frame, LookupAnswer, LookupEdition, MAX_FRAME_BYTES,
    };
    let long = "x".repeat(2000);
    let answers: Vec<LookupAnswer> = (0..100)
        .map(|n| LookupAnswer {
            work: format!("work-{n}"),
            editions: (0..8)
                .map(|e| LookupEdition {
                    store: "audible".into(),
                    marketplace: "us".into(),
                    product_id: format!("B0{n:04}{e:04}"),
                    availability: Availability::Owned,
                    formats: Vec::new(),
                    metadata: CatalogMetadata {
                        title: long.clone(),
                        subtitle: Some(long.clone()),
                        publisher: Some(long.clone()),
                        ..CatalogMetadata::default()
                    },
                })
                .collect(),
        })
        .collect();
    let frames = found_frames(answers, vec!["gone".into()]);
    assert!(frames.len() > 1, "split");
    assert_eq!(frames[0].1, vec!["gone".to_string()], "missing rides first");
    assert!(frames[1..].iter().all(|(_, missing)| missing.is_empty()));
    assert_eq!(frames.iter().map(|(a, _)| a.len()).sum::<usize>(), 100);
    for (answers, missing) in frames {
        let text = Frame::new("req-1", Body::CatalogFound { answers, missing }).to_text();
        assert!(text.len() <= MAX_FRAME_BYTES, "{} bytes", text.len());
    }
    assert_eq!(found_frames(Vec::new(), Vec::new()).len(), 1);
}
