//! Branches of branches, end to end in one process: a tree of three
//! libraries, A ← B ← C (B is a branch of A, C a branch of B).
//!
//! C shares its book with B over the node socket; B then shares its own
//! book and the one C gave it with A. Every title on A is attributed to
//! B's owner user there with provenance `node:<B>` (A never learns C), its
//! files land in A's destination through the upload endpoint without
//! staging, no store account or downstream user crosses a link, a second
//! pass resends nothing, a title A holds for that user is not echoed back,
//! "Include books from my own branches" off withdraws C's title from A,
//! sharing off marks B's titles on A, C leaving marks its titles on B, and
//! A refuses to join below B (a cycle).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bookclerk_library::control_plane::{
    accept_catalog_batch, begin_catalog, join_cycle, library_ancestry, list_branch_titles,
    load_cluster_link_keys, load_cluster_row, load_node, record_node_left,
    remove_detached_branch_titles, set_cluster_link_sharing, CatalogAccept, CatalogBegin,
    ClusterLink, BRANCH_TITLE_DETACHED, BRANCH_TITLE_SHARED, DETACHED_LEFT,
    DETACHED_STOPPED_SHARING, DETACHED_WITHDRAWN,
};
use bookclerk_library::{AcquireStatus, LibraryStore, NewBook, UserRole};
use bookclerk_node_client::{
    refuse_cycle, HostClient, HostClientConfig, HostClientHandle, JoinError,
};
use bookclerk_storage::StorageBackend;
use tokio::sync::watch;

use super::super::remote_bindings::RemoteCatalog;
use super::super::tests::{
    connected, host_store, join, owner_account, public_origin, serve, timing, Captured,
    FixedSampler,
};
use super::super::TransportRunner;
use super::*;
use crate::job_handler::{JobCommand, JobExecCtx, JobTransport};

/// A link client that claims no jobs.
struct NoJobs;

impl JobTransport for NoJobs {
    async fn execute(&self, _cmd: JobCommand, _ctx: JobExecCtx) -> anyhow::Result<String> {
        anyhow::bail!("this test claims no jobs")
    }
}

/// A [`BranchShelf`] over a local directory.
struct DirShelf(PathBuf);

impl BranchShelf for DirShelf {
    async fn primary(&self) -> Option<Box<dyn StorageBackend>> {
        Some(Box::new(
            bookclerk_storage::LocalFsBackend::new(self.0.clone()).ok()?,
        ))
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

/// A store account of this install and one acquired book of it, its
/// files (primary, cover, plus an unrelated file) written under `shelf`.
async fn own_book(
    store: &LibraryStore,
    shelf: &Path,
    source: &str,
    account_id: &str,
    product_id: &str,
) -> String {
    let plugin = format!("path:file:///tmp/{source}");
    store
        .upsert_account(&plugin, account_id, "us", None, true, "test")
        .await
        .unwrap();
    let book = store
        .scope(&plugin, source)
        .upsert_book(&new_book(&plugin, source, account_id, product_id))
        .await
        .unwrap();
    let primary = format!("Author/{product_id}/{product_id}.m4b");
    let dir = shelf.join(format!("Author/{product_id}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{product_id}.m4b")),
        payload(product_id, 200_000),
    )
    .unwrap();
    std::fs::write(
        dir.join(format!("{product_id}.jpg")),
        payload(product_id, 900),
    )
    .unwrap();
    std::fs::write(dir.join("notes.txt"), b"not part of the title").unwrap();
    store
        .set_acquire_status(
            &book.uuid,
            account_id,
            AcquireStatus::Acquired,
            Some(&primary),
            None,
        )
        .await
        .unwrap();
    primary
}

/// A library row.
fn new_book(plugin: &str, source: &str, account_id: &str, product_id: &str) -> NewBook {
    NewBook {
        uuid: None,
        product_id: product_id.into(),
        plugin_key: plugin.into(),
        source: source.into(),
        account_id: account_id.into(),
        asin: None,
        isbn: None,
        marketplace: "us".into(),
        title: format!("Title {product_id}"),
        authors: Some("Author".into()),
        narrators: None,
        series: None,
        series_index: None,
        series_asin: None,
        purchased_at: None,
        publisher: None,
        length_minutes: Some(61),
        is_abridged: false,
        content_kind: "book".into(),
        categories: None,
        subtitle: None,
        published_at: None,
    }
}

/// Distinct bytes per title.
fn payload(product_id: &str, len: usize) -> Vec<u8> {
    let seed = product_id
        .bytes()
        .fold(7u8, |acc, b| acc.wrapping_mul(31) ^ b);
    (0..len)
        .map(|i| (i as u8).wrapping_add(seed).wrapping_mul(13))
        .collect()
}

/// `(state, reason)` of the title `product_id` on `store`.
async fn title_state(store: &LibraryStore, product_id: &str) -> (String, Option<String>) {
    let row = list_branch_titles(store, None, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|row| row.product_id == product_id)
        .unwrap_or_else(|| panic!("no shared title {product_id}"));
    (row.state, row.detached_reason)
}

/// Every file under `dir`, relative, sorted (the local backend's own
/// metadata sidecars left out).
fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if !path.to_string_lossy().ends_with(".bookclerk-meta.json") {
                out.push(
                    path.strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    out.sort();
    out
}

#[test]
fn a_sibling_shares_the_primarys_stem_up_to_a_dot() {
    for sibling in ["b1.m4b", "b1.jpg", "b1.chapters.json", "b1.cue"] {
        assert!(is_sibling(sibling, "b1"), "{sibling}");
    }
    for other in ["b10.m4b", "b1-cover.jpg", "b1", "sub/b1.jpg", "notes.txt"] {
        assert!(!is_sibling(other, "b1"), "{other}");
    }
}

#[tokio::test]
async fn a_tree_of_libraries_shares_books_one_hop_at_a_time() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let logs = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(logs.clone())
        .finish();
    let _logging = tracing::subscriber::set_default(subscriber);
    // A: the library at the root, with Riley, who owns B.
    let a_files = tempfile::tempdir().unwrap();
    let (a_state, a) = crate::node_api::tests::test_state(&a_files).await;
    let a_addr = serve(crate::api::router(Arc::clone(&a_state), None)).await;
    let a_origin = public_origin(&a_state, &format!("http://127.0.0.1:{}", a_addr.port())).await;
    let (riley, _token) = owner_account(&a, b"riley-token-v1").await;
    let a_out = tempfile::tempdir().unwrap();
    crate::node_api::uploads::set_test_destination(a_files.path(), a_out.path());

    // B: a library that is A's branch, with Sam, who owns C. Its own
    // destination is where its own books and C's arrive.
    let b_files = tempfile::tempdir().unwrap();
    let (b_state, b) = crate::node_api::tests::test_state(&b_files).await;
    let b_addr = serve(crate::api::router(Arc::clone(&b_state), None)).await;
    let b_origin = public_origin(&b_state, &format!("http://127.0.0.1:{}", b_addr.port())).await;
    let sam = b
        .create_user(UserRole::Member, Some("Sam"), None)
        .await
        .unwrap();
    let b_out = tempfile::tempdir().unwrap();
    crate::node_api::uploads::set_test_destination(b_files.path(), b_out.path());
    own_book(&b, b_out.path(), "libro", "b-store-acct", "b1").await;
    // A title B holds that A also holds for Riley: it came from A.
    own_book(&b, b_out.path(), "test", "riley-acct", "echo1").await;
    a.scope(super::super::tests::PLUGIN, "test")
        .upsert_book(&new_book(
            super::super::tests::PLUGIN,
            "test",
            "riley-acct",
            "echo1",
        ))
        .await
        .unwrap();

    // C: a bare install, B's branch.
    let c_files = tempfile::tempdir().unwrap();
    let c = host_store(c_files.path()).await;
    let c_shelf = tempfile::tempdir().unwrap();
    own_book(&c, c_shelf.path(), "audible", "c-store-acct", "c1").await;

    let b_link = join(&a, &b, riley, &a_origin, None).await.unwrap();
    let c_link = join(&b, &c, sam.id, &b_origin, None).await.unwrap();
    assert!(
        b_link.share_books && b_link.include_branches,
        "both on by default"
    );

    // C → B.
    let (c_handle, _c_stop) = start(&c, &c_link).await;
    let c_sync = CatalogSync {
        library: c.clone(),
        cluster_id: c_link.owner_cluster_id.clone(),
        handle: c_handle,
        shelf: DirShelf(c_shelf.path().to_path_buf()),
    };
    let report = c_sync.sync_once().await.unwrap();
    assert_eq!((report.sent, report.upserted, report.delivered), (1, 1, 1));
    assert_eq!(
        tree(b_out.path())
            .into_iter()
            .filter(|key| key.starts_with("Author/c1/"))
            .collect::<Vec<_>>(),
        vec!["Author/c1/c1.jpg", "Author/c1/c1.m4b"],
        "C's title arrived on B under its own keys, without unrelated files"
    );
    let on_b = list_branch_titles(&b, None, 10).await.unwrap();
    assert_eq!(on_b.len(), 1);
    assert_eq!(
        on_b[0].provenance,
        format!("node:{}", c_link.remote_node_id)
    );
    assert_eq!(on_b[0].owner_user_id, Some(sam.id), "Sam's on B");
    assert!(on_b[0].delivered);

    // B → A: B's own book and the one C shared; the echo is skipped.
    let (b_handle, _b_stop) = start(&b, &b_link).await;
    let b_sync = CatalogSync {
        library: b.clone(),
        cluster_id: b_link.owner_cluster_id.clone(),
        handle: b_handle,
        shelf: DirShelf(b_out.path().to_path_buf()),
    };
    // What crosses the link: store, product id, metadata, files only.
    for entry in b_sync.entries(true).await.unwrap() {
        let shelf = b_sync.shelf.primary().await.unwrap();
        let title = b_sync.title(shelf.as_ref(), &entry).await.unwrap();
        let json = serde_json::to_value(&title).unwrap();
        for key in json.as_object().unwrap().keys() {
            assert!(
                [
                    "files",
                    "metadata",
                    "pdf_key",
                    "primary_key",
                    "product_id",
                    "store"
                ]
                .contains(&key.as_str()),
                "{key} would cross"
            );
        }
        for file in json["files"].as_array().unwrap() {
            for key in file.as_object().unwrap().keys() {
                assert!(
                    ["key", "size", "content_type"].contains(&key.as_str()),
                    "{key}"
                );
            }
        }
        let text = json.to_string();
        for private in [
            "b-store-acct",
            "c-store-acct",
            "riley-acct",
            "Sam",
            "Riley",
            c_link.remote_node_id.as_str(),
            b_link.remote_node_id.as_str(),
        ] {
            assert!(!text.contains(private), "{private} would cross: {text}");
        }
    }
    let report = b_sync.sync_once().await.unwrap();
    assert_eq!(report.sent, 3, "b1, c1 and echo1 are named");
    assert_eq!((report.upserted, report.skipped), (2, 1));
    assert_eq!(report.delivered, 2);
    assert_eq!(
        tree(a_out.path()),
        vec![
            "Author/b1/b1.jpg",
            "Author/b1/b1.m4b",
            "Author/c1/c1.jpg",
            "Author/c1/c1.m4b",
        ],
        "both titles reached A's destination; the echo did not"
    );
    for key in ["Author/b1/b1.m4b", "Author/c1/c1.m4b"] {
        assert_eq!(
            std::fs::read(a_out.path().join(key)).unwrap(),
            std::fs::read(b_out.path().join(key)).unwrap()
        );
    }
    // Upload reuse: every file went through the upload endpoint under the
    // catalog item (C's to B, B's two to A), and nothing was staged under
    // A's files dir.
    let captured = String::from_utf8_lossy(&logs.0.lock().unwrap()).into_owned();
    let received = captured
        .lines()
        .filter(|line| line.contains("a host's uploaded file reached every destination"))
        .collect::<Vec<_>>();
    assert_eq!(received.len(), 6, "{received:#?}");
    assert!(received
        .iter()
        .all(|line| line.contains(&format!("job_id={}", bookclerk_node_proto::CATALOG_ITEM))));
    assert!(
        !captured.contains("Bookclerk-Upload "),
        "an upload token reached the logs"
    );
    let big = payload("c1", 200_000);
    for file in tree(a_files.path()) {
        assert_ne!(
            std::fs::read(a_files.path().join(&file)).unwrap(),
            big,
            "A staged {file}"
        );
    }

    // One hop: every row on A is B's, as Riley's; A never learns C.
    let on_a = list_branch_titles(&a, None, 10).await.unwrap();
    assert_eq!(on_a.len(), 2);
    for row in &on_a {
        assert_eq!(row.provenance, format!("node:{}", b_link.remote_node_id));
        assert_eq!(row.owner_user_id, Some(riley));
        assert!(row.delivered);
    }
    assert!(load_node(&a, &c_link.remote_node_id).await.is_err());
    assert!(on_a.iter().all(|row| row.product_id != "echo1"));

    // Dedup and resume: nothing new, nothing resent.
    let again = b_sync.sync_once().await.unwrap();
    assert_eq!((again.sent, again.upserted, again.delivered), (0, 0, 0));
    assert_eq!(list_branch_titles(&a, None, 10).await.unwrap().len(), 2);

    // A cannot become B's branch (or C's): B's ancestry names A.
    let a_id = load_cluster_row(&a).await.unwrap().unwrap().cluster_id;
    let b_ancestry = library_ancestry(&b).await.unwrap();
    assert!(b_ancestry.contains(&a_id), "{b_ancestry:?}");
    assert_eq!(join_cycle(&a, &b_ancestry).await.unwrap(), Some(a_id));
    assert!(matches!(
        refuse_cycle(&a, &b_ancestry).await,
        Err(JoinError::Cycle)
    ));
    assert!(refuse_cycle(
        &c,
        &[load_cluster_row(&b).await.unwrap().unwrap().cluster_id]
    )
    .await
    .is_ok());

    // "Include books from my own branches" off: C's title is withdrawn
    // from A; B's own stays; the files stay. The pass from the start this
    // opens is cut off after its first title (the link dropped): the next
    // session resumes after it, and its last batch still ends the pass.
    set_cluster_link_sharing(&b, &b_link.owner_cluster_id, true, false)
        .await
        .unwrap();
    let b_node = b_link.remote_node_id.as_str();
    let CatalogBegin::Opened(cut) = begin_catalog(&a, b_node, true, false).await.unwrap() else {
        panic!("expected a session");
    };
    assert_eq!(cut.cursor, "", "the new settings restart the pass");
    let first = b_sync.entries(false).await.unwrap().remove(0);
    let shelf = b_sync.shelf.primary().await.unwrap();
    let first_title = b_sync.title(shelf.as_ref(), &first).await.unwrap();
    assert!(matches!(
        accept_catalog_batch(&a, b_node, cut.fence, &first.order, false, &[first_title])
            .await
            .unwrap(),
        CatalogAccept::Accepted { .. }
    ));
    let report = b_sync.sync_once().await.unwrap();
    assert_eq!(
        report.sent, 1,
        "the pass resumed after the first of b1 and echo1"
    );
    assert_eq!(
        title_state(&a, "c1").await,
        (
            BRANCH_TITLE_DETACHED.into(),
            Some(DETACHED_WITHDRAWN.into())
        )
    );
    assert_eq!(
        title_state(&a, "b1").await,
        (BRANCH_TITLE_SHARED.into(), None)
    );
    assert!(a_out.path().join("Author/c1/c1.m4b").exists());

    // Sharing off: B's titles on A are marked, nothing new arrives, and
    // the owner removes the entries in bulk; the files stay.
    set_cluster_link_sharing(&b, &b_link.owner_cluster_id, false, false)
        .await
        .unwrap();
    own_book(&b, b_out.path(), "libro", "b-store-acct", "b2").await;
    let report = b_sync.sync_once().await.unwrap();
    assert!(!report.shared);
    assert_eq!(report.sent, 0);
    assert_eq!(
        title_state(&a, "b1").await,
        (
            BRANCH_TITLE_DETACHED.into(),
            Some(DETACHED_STOPPED_SHARING.into())
        )
    );
    assert!(list_branch_titles(&a, None, 10)
        .await
        .unwrap()
        .iter()
        .all(|row| row.product_id != "b2"));
    assert_eq!(remove_detached_branch_titles(&a, None).await.unwrap(), 2);
    assert!(list_branch_titles(&a, None, 10).await.unwrap().is_empty());
    assert!(a_out.path().join("Author/b1/b1.m4b").exists());

    // C leaves B: its title on B is marked as from a branch that left.
    assert!(record_node_left(&b, &c_link.remote_node_id).await.unwrap());
    assert_eq!(
        title_state(&b, "c1").await,
        (BRANCH_TITLE_DETACHED.into(), Some(DETACHED_LEFT.into()))
    );
    assert!(
        bookclerk_library::control_plane::forwardable_branch_titles(&b)
            .await
            .unwrap()
            .is_empty(),
        "B no longer passes C's title on"
    );
}

/// A local destination whose listing of one folder fails while `fail`
/// names it.
#[derive(Clone)]
struct FlakyList {
    /// The destination itself.
    inner: bookclerk_storage::LocalFsBackend,
    /// The prefix whose listing fails, if any.
    fail: Arc<std::sync::Mutex<Option<String>>>,
}

#[async_trait::async_trait]
impl StorageBackend for FlakyList {
    fn name(&self) -> &'static str {
        "flaky-list"
    }

    fn clone_box(&self) -> Box<dyn StorageBackend> {
        Box::new(self.clone())
    }

    async fn put(
        &self,
        key: &str,
        data: bytes::Bytes,
        meta: bookclerk_storage::ObjectMeta,
    ) -> bookclerk_storage::Result<()> {
        self.inner.put(key, data, meta).await
    }

    async fn get(&self, key: &str) -> bookclerk_storage::Result<bytes::Bytes> {
        self.inner.get(key).await
    }

    async fn exists(&self, key: &str) -> bookclerk_storage::Result<bool> {
        self.inner.exists(key).await
    }

    async fn list(
        &self,
        prefix: &str,
    ) -> bookclerk_storage::Result<Vec<bookclerk_storage::ObjectInfo>> {
        if self.fail.lock().unwrap().as_deref() == Some(prefix) {
            return Err(bookclerk_storage::StorageError::Io(std::io::Error::other(
                "the destination is briefly unreachable",
            )));
        }
        self.inner.list(prefix).await
    }

    async fn probe(&self, key: &str) -> bookclerk_storage::Result<bookclerk_storage::ObjectProbe> {
        self.inner.probe(key).await
    }

    async fn copy(&self, from: &str, to: &str) -> bookclerk_storage::Result<()> {
        self.inner.copy(from, to).await
    }

    async fn delete(&self, key: &str) -> bookclerk_storage::Result<()> {
        self.inner.delete(key).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> bookclerk_storage::Result<bookclerk_storage::ListPage> {
        self.inner.list_page(prefix, cursor, limit).await
    }

    async fn get_stream(
        &self,
        key: &str,
        range: Option<bookclerk_storage::ByteRange>,
    ) -> bookclerk_storage::Result<(
        bookclerk_storage::ObjectProbe,
        std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
    )> {
        self.inner.get_stream(key, range).await
    }

    async fn put_stream(
        &self,
        key: &str,
        body: std::pin::Pin<Box<dyn tokio::io::AsyncRead + Send>>,
        meta: bookclerk_storage::ObjectMeta,
    ) -> bookclerk_storage::Result<bookclerk_storage::PutStreamResult> {
        self.inner.put_stream(key, body, meta).await
    }
}

impl BranchShelf for FlakyList {
    async fn primary(&self) -> Option<Box<dyn StorageBackend>> {
        Some(self.clone_box())
    }
}

/// A title whose files cannot be listed for a moment is not taken as gone:
/// the titles after it are still sent, but the pass from the start does
/// not end, so the library withdraws nothing, and the next pass tries
/// that title again.
#[tokio::test]
async fn a_title_that_cannot_be_listed_now_is_not_withdrawn() {
    let _dek = crate::auth::tests::process_dek_lock().await;
    let a_files = tempfile::tempdir().unwrap();
    let (a_state, a) = crate::node_api::tests::test_state(&a_files).await;
    let a_addr = serve(crate::api::router(Arc::clone(&a_state), None)).await;
    let a_origin = public_origin(&a_state, &format!("http://127.0.0.1:{}", a_addr.port())).await;
    let (riley, _token) = owner_account(&a, b"riley-token-v1").await;
    let a_out = tempfile::tempdir().unwrap();
    crate::node_api::uploads::set_test_destination(a_files.path(), a_out.path());

    let b_files = tempfile::tempdir().unwrap();
    let b = host_store(b_files.path()).await;
    let b_out = tempfile::tempdir().unwrap();
    own_book(&b, b_out.path(), "libro", "b-store-acct", "b1").await;
    own_book(&b, b_out.path(), "libro", "b-store-acct", "b2").await;
    let b_link = join(&a, &b, riley, &a_origin, None).await.unwrap();
    let (b_handle, _b_stop) = start(&b, &b_link).await;
    let fail = Arc::new(std::sync::Mutex::new(None));
    let b_sync = CatalogSync {
        library: b.clone(),
        cluster_id: b_link.owner_cluster_id.clone(),
        handle: b_handle,
        shelf: FlakyList {
            inner: bookclerk_storage::LocalFsBackend::new(b_out.path().to_path_buf()).unwrap(),
            fail: Arc::clone(&fail),
        },
    };
    let report = b_sync.sync_once().await.unwrap();
    assert_eq!((report.sent, report.upserted), (2, 2));

    // A new pass from the start ("Include books from my own branches"
    // turned off) cannot list b1's folder: b2 still goes, b1 stays shared.
    set_cluster_link_sharing(&b, &b_link.owner_cluster_id, true, false)
        .await
        .unwrap();
    *fail.lock().unwrap() = Some("Author/b1/".into());
    let report = b_sync.sync_once().await.unwrap();
    assert_eq!(report.sent, 1);
    for product in ["b1", "b2"] {
        assert_eq!(
            title_state(&a, product).await,
            (BRANCH_TITLE_SHARED.into(), None),
            "{product}"
        );
    }
    // The pass did not end and its cursor stayed before b1 (the first
    // title): the next pass tries it again and ends, after which nothing
    // is resent.
    *fail.lock().unwrap() = None;
    let report = b_sync.sync_once().await.unwrap();
    assert_eq!(report.sent, 2, "the pass resumed before b1");
    assert_eq!(
        title_state(&a, "b1").await,
        (BRANCH_TITLE_SHARED.into(), None)
    );
    assert_eq!(b_sync.sync_once().await.unwrap().sent, 0);
}
