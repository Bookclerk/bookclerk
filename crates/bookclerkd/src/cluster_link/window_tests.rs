//! Window mode end to end in one process: library A holds a book for
//! Riley, who owns window W; W's link has "Use this computer to download"
//! off, so W reads the book over the node socket and shows it in its merged
//! view, until the setting is turned back on.

use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    load_cluster_link_keys, set_cluster_link_download, window_view, ClusterLink,
};
use bookclerk_library::{LibraryStore, NewBook};
use bookclerk_node_client::{HostClient, HostClientConfig, HostClientHandle};
use tokio::sync::watch;

use super::super::remote_bindings::RemoteCatalog;
use super::super::tests::{
    connected, host_store, join, owner_account, public_origin, serve, timing, FixedSampler,
};
use super::super::TransportRunner;
use super::*;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// The ASIN library A holds for Riley.
const ASIN: &str = "B0WINDOW01";

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
            share: None,
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

#[tokio::test]
async fn a_window_reads_the_books_its_library_holds_for_its_person() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let a_files = tempfile::tempdir().unwrap();
    let (a_state, a) = crate::node_api::tests::test_state(&a_files).await;
    let a_addr = serve(crate::api::router(Arc::clone(&a_state), None)).await;
    let a_origin = public_origin(&a_state, &format!("http://127.0.0.1:{}", a_addr.port())).await;
    let (riley, _token) = owner_account(&a, b"riley-token-v1").await;
    let plugin = "path:file:///tmp/audible";
    a.upsert_account(plugin, "riley-acct", "us", None, true, "test")
        .await
        .unwrap();
    let identity = a.ensure_local_portal_identity(riley, None).await.unwrap();
    a.link_account(identity.id, plugin, "riley-acct", "test")
        .await
        .unwrap();
    a.scope(plugin, "audible")
        .upsert_book(&NewBook {
            uuid: None,
            product_id: ASIN.into(),
            plugin_key: plugin.into(),
            source: "audible".into(),
            account_id: "riley-acct".into(),
            asin: Some(ASIN.into()),
            isbn: None,
            marketplace: "us".into(),
            title: "Through the Window".into(),
            authors: Some("Ann Author".into()),
            narrators: None,
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
        })
        .await
        .unwrap();

    let w_files = tempfile::tempdir().unwrap();
    let w = host_store(w_files.path()).await;
    let link = join(&a, &w, riley, &a_origin, None).await.unwrap();
    assert!(link.download, "a new link downloads");
    let (handle, stop) = start(&w, &link).await;
    let reader = WindowReader {
        library: w.clone(),
        cluster_id: link.owner_cluster_id.clone(),
        handle,
    };
    // Downloading: not a window, nothing read.
    assert_eq!(reader.read_once().await.unwrap(), WindowRead::NotAWindow);
    assert!(window_view(&w).await.unwrap().is_empty());

    assert!(set_cluster_link_download(&w, &link.owner_cluster_id, false)
        .await
        .unwrap());
    assert_eq!(
        reader.read_once().await.unwrap(),
        WindowRead::Read {
            stored: 1,
            dropped: 0
        }
    );
    let view = window_view(&w).await.unwrap();
    assert_eq!(view.len(), 1, "{view:?}");
    assert_eq!(view[0].title(), "Through the Window");
    assert_eq!(view[0].editions[0].owner_cluster_id, link.owner_cluster_id);
    assert_eq!(view[0].editions[0].edition.product_id, ASIN);
    // Read again: nothing new, nothing dropped.
    assert_eq!(
        reader.read_once().await.unwrap(),
        WindowRead::Read {
            stored: 1,
            dropped: 0
        }
    );

    // A read under way when the setting changes discards what it wrote,
    // so it cannot put back a list the change cleared.
    let before = super::generation(&link.owner_cluster_id);
    super::notify_window_changed(&link.owner_cluster_id);
    assert_eq!(
        reader.read_pages(before).await.unwrap(),
        WindowRead::Superseded
    );
    assert!(window_view(&w).await.unwrap().is_empty(), "discarded");
    assert_eq!(
        reader.read_once().await.unwrap(),
        WindowRead::Read {
            stored: 1,
            dropped: 0
        }
    );

    // Download back on: the window's copy of the list is cleared.
    set_cluster_link_download(&w, &link.owner_cluster_id, true)
        .await
        .unwrap();
    assert_eq!(reader.read_once().await.unwrap(), WindowRead::NotAWindow);
    set_cluster_link_download(&w, &link.owner_cluster_id, false)
        .await
        .unwrap();
    assert!(window_view(&w).await.unwrap().is_empty(), "cleared");

    // Downloading turned on and off again before the reader looks: the
    // setting itself clears the list, so the old one does not come back.
    reader.read_once().await.unwrap();
    assert_eq!(window_view(&w).await.unwrap().len(), 1);
    set_cluster_link_download(&w, &link.owner_cluster_id, true)
        .await
        .unwrap();
    set_cluster_link_download(&w, &link.owner_cluster_id, false)
        .await
        .unwrap();
    assert!(
        window_view(&w).await.unwrap().is_empty(),
        "cleared without the reader"
    );

    // With the library unreachable, turning downloading on still clears
    // the list (no request is sent).
    reader.read_once().await.unwrap();
    assert_eq!(window_view(&w).await.unwrap().len(), 1);
    stop.send_replace(true);
    assert!(
        !reader.clear_unless_window().await.unwrap(),
        "still a window"
    );
    set_cluster_link_download(&w, &link.owner_cluster_id, true)
        .await
        .unwrap();
    assert!(reader.clear_unless_window().await.unwrap());
    set_cluster_link_download(&w, &link.owner_cluster_id, false)
        .await
        .unwrap();
    assert!(window_view(&w).await.unwrap().is_empty(), "cleared offline");
}
