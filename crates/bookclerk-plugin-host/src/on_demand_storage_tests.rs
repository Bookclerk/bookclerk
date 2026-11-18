//! Wake, idle sleep, stream holds and retirement of [`OnDemandStorage`]
//! against an in-process storage double backed by a local directory.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_storage::{
    ListRequest, LocalFsBackend, MoveOptions, ObjectMeta, StorageBackend, WriteCondition,
};
use bytes::Bytes;
use tokio::io::AsyncReadExt;

use super::{storage_activates_on_demand, AwakeStorage, OnDemandStorage};
use crate::activation::{Guest, GuestStarter, OnDemandStatus};
use crate::rpc_session::PluginSession;

/// Storage double: a local directory behind a guest that can stop.
struct FakeStorage {
    /// Cleared when the double is stopped.
    running: AtomicBool,
    /// Backend over the shared test directory.
    backend: LocalFsBackend,
}

impl FakeStorage {
    /// A running double over `root`.
    fn new(root: &std::path::Path) -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            backend: LocalFsBackend::new(root.to_path_buf()).expect("local backend"),
        })
    }
}

#[async_trait]
impl Guest for FakeStorage {
    fn guest_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn guest_pid(&self) -> Option<u32> {
        self.guest_running().then_some(7070)
    }

    async fn stop_guest(&self) -> std::result::Result<(), String> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl AwakeStorage for FakeStorage {
    fn backend(&self) -> &dyn StorageBackend {
        &self.backend
    }

    fn session(&self) -> Option<Arc<PluginSession>> {
        None
    }
}

/// A named storage call the wake test drives against a sleeping proxy.
type NamedCall<'a> = (
    &'static str,
    std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>>,
);

/// Guests a [`FakeStarter`] started, shared with the test.
type Started = Arc<std::sync::Mutex<Vec<Arc<FakeStorage>>>>;

/// Starter double that hands out fresh [`FakeStorage`]s over one directory.
struct FakeStarter {
    /// Directory every double stores into.
    root: std::path::PathBuf,
    /// Every guest started on demand, in order.
    started: Started,
}

#[async_trait]
impl GuestStarter<dyn AwakeStorage> for FakeStarter {
    async fn start_guest(&self) -> std::result::Result<Arc<dyn AwakeStorage>, String> {
        let guest = FakeStorage::new(&self.root);
        self.started
            .lock()
            .expect("started guests")
            .push(Arc::clone(&guest));
        Ok(guest)
    }
}

/// Proxy over a first double in `dir` that sleeps after `idle`.
fn proxy(
    dir: &tempfile::TempDir,
    idle: Duration,
) -> (Arc<OnDemandStorage>, Arc<FakeStorage>, Started) {
    let first = FakeStorage::new(dir.path());
    let started: Started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = OnDemandStorage::wrap(
        Arc::clone(&first) as Arc<dyn AwakeStorage>,
        "storage-instance",
        Box::new(FakeStarter {
            root: dir.path().to_path_buf(),
            started: Arc::clone(&started),
        }),
        idle,
    );
    (proxy, first, started)
}

/// Waits until `proxy` has stopped its guest.
async fn wait_asleep(proxy: &OnDemandStorage) {
    for _ in 0..200 {
        if !proxy.is_awake() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("idle storage did not sleep");
}

#[tokio::test]
async fn a_sleeping_proxy_keeps_its_namespace_identity_without_waking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let reference = LocalFsBackend::new(dir.path().to_path_buf()).expect("local backend");
    let (proxy, first, started) = proxy(&dir, Duration::from_millis(30));
    wait_asleep(&proxy).await;
    assert!(!first.guest_running());
    assert_eq!(proxy.name(), reference.name());
    assert_eq!(proxy.instance_id(), reference.instance_id());
    assert_eq!(proxy.scan_placement(), reference.scan_placement());
    assert_eq!(
        proxy.supports_server_copy(),
        reference.supports_server_copy()
    );
    assert_eq!(proxy.clone_box().instance_id(), reference.instance_id());
    assert!(started.lock().expect("started").is_empty());
    assert_eq!(proxy.wakes(), 0);
}

#[tokio::test]
async fn every_storage_call_wakes_a_sleeping_guest_and_reaches_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, _first, started) = proxy(&dir, Duration::from_millis(30));
    let meta = ObjectMeta::default;
    let calls: Vec<NamedCall<'_>> = vec![
        (
            "put",
            Box::pin(async {
                proxy
                    .put("a/one.txt", Bytes::from_static(b"one"), meta())
                    .await
                    .expect("put");
            }),
        ),
        (
            "get",
            Box::pin(async {
                assert_eq!(proxy.get("a/one.txt").await.expect("get"), "one");
            }),
        ),
        (
            "exists",
            Box::pin(async {
                assert!(proxy.exists("a/one.txt").await.expect("exists"));
            }),
        ),
        (
            "probe",
            Box::pin(async {
                assert_eq!(proxy.probe("a/one.txt").await.expect("probe").size, 3);
            }),
        ),
        (
            "head",
            Box::pin(async {
                assert!(proxy.head("a/missing.txt").await.expect("head").is_none());
            }),
        ),
        (
            "copy",
            Box::pin(async {
                proxy.copy("a/one.txt", "a/two.txt").await.expect("copy");
            }),
        ),
        (
            "rename",
            Box::pin(async {
                proxy
                    .rename("a/two.txt", "a/three.txt")
                    .await
                    .expect("rename");
            }),
        ),
        (
            "list",
            Box::pin(async {
                assert_eq!(proxy.list("a/").await.expect("list").len(), 2);
            }),
        ),
        (
            "list_page",
            Box::pin(async {
                let page = proxy.list_page("a/", None, 10).await.expect("list_page");
                assert_eq!(page.objects.len(), 2);
            }),
        ),
        (
            "list_audio",
            Box::pin(async {
                assert!(proxy.list_audio("a/").await.expect("list_audio").is_empty());
            }),
        ),
        (
            "touch_file",
            Box::pin(async {
                proxy
                    .touch_file("a/one.txt", None, Some(std::time::SystemTime::now()))
                    .await
                    .expect("touch_file");
            }),
        ),
        (
            "put_file",
            Box::pin(async {
                let source = dir.path().join("source.bin");
                tokio::fs::write(&source, b"file").await.expect("write");
                proxy
                    .put_file("b/file.bin", &source, meta())
                    .await
                    .expect("put_file");
            }),
        ),
        (
            "put_stream",
            Box::pin(async {
                let body = Box::pin(std::io::Cursor::new(b"stream".to_vec()));
                let put = proxy
                    .put_stream("b/stream.bin", body, meta())
                    .await
                    .expect("put_stream");
                assert_eq!(put.bytes_written, 6);
            }),
        ),
        (
            "move_object",
            Box::pin(async {
                let moved = proxy
                    .move_object("a/one.txt", "c/one.txt", &MoveOptions::default())
                    .await
                    .expect("move_object");
                assert!(!moved.already_moved);
            }),
        ),
        (
            "move_many",
            Box::pin(async {
                let pairs = [("c/one.txt".to_string(), "c/two.txt".to_string())];
                let results = proxy
                    .move_many(&pairs, &MoveOptions::default())
                    .await
                    .expect("move_many");
                assert!(results[0].error.is_none());
            }),
        ),
        (
            "move_prefix",
            Box::pin(async {
                let page = proxy
                    .move_prefix("c/", "d/", None, 10, &MoveOptions::default())
                    .await
                    .expect("move_prefix");
                assert_eq!(page.results.len(), 1);
            }),
        ),
        (
            "head_many",
            Box::pin(async {
                let keys = ["d/two.txt".to_string(), "d/missing.txt".to_string()];
                let probes = proxy.head_many(&keys).await.expect("head_many");
                assert!(probes[0].is_some() && probes[1].is_none());
            }),
        ),
        (
            "put_stream_conditional",
            Box::pin(async {
                let body = Box::pin(std::io::Cursor::new(b"new".to_vec()));
                proxy
                    .put_stream_conditional("e/new.txt", body, meta(), &WriteCondition::IfNoneMatch)
                    .await
                    .expect("put_stream_conditional");
            }),
        ),
        (
            "list_hinted",
            Box::pin(async {
                let page = proxy
                    .list_hinted(&ListRequest {
                        prefix: "d/".into(),
                        limit: 10,
                        with_signatures: true,
                        ..ListRequest::default()
                    })
                    .await
                    .expect("list_hinted");
                assert_eq!(page.page.objects.len(), 1);
            }),
        ),
        (
            "delete_many",
            Box::pin(async {
                let results = proxy
                    .delete_many(&["e/new.txt".to_string()])
                    .await
                    .expect("delete_many");
                assert!(results[0].error.is_none());
            }),
        ),
        (
            "delete",
            Box::pin(async {
                proxy.delete("a/three.txt").await.expect("delete");
            }),
        ),
    ];
    let mut expected_wakes = 0;
    for (name, call) in calls {
        wait_asleep(&proxy).await;
        call.await;
        expected_wakes += 1;
        assert_eq!(proxy.wakes(), expected_wakes, "{name} wakes the guest");
    }
    assert_eq!(
        started.lock().expect("started").len(),
        expected_wakes as usize
    );
}

#[tokio::test]
async fn a_change_feed_read_never_wakes_a_guest_without_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, _first, started) = proxy(&dir, Duration::from_millis(30));
    wait_asleep(&proxy).await;
    assert!(!proxy.capabilities().changes);
    assert!(proxy.changes(None, 10).await.expect("changes").is_none());
    assert!(started.lock().expect("started").is_empty());
}

/// Starter double whose guests watch `root`, on a local disk or (with
/// `kind`) on a file system the watch refuses.
struct WatchingStarter {
    /// Directory every double watches.
    root: std::path::PathBuf,
    /// File-system kind every double assumes.
    kind: bookclerk_storage::fs_kind::FsKind,
    /// Every guest started on demand, in order.
    started: Started,
}

impl WatchingStarter {
    /// A running double watching `root` as a file system of `kind`.
    fn guest(root: &std::path::Path, kind: bookclerk_storage::fs_kind::FsKind) -> Arc<FakeStorage> {
        Arc::new(FakeStorage {
            running: AtomicBool::new(true),
            backend: LocalFsBackend::new(root.to_path_buf())
                .expect("local backend")
                .with_fs_kind(kind)
                .with_watch(),
        })
    }
}

#[async_trait]
impl GuestStarter<dyn AwakeStorage> for WatchingStarter {
    async fn start_guest(&self) -> std::result::Result<Arc<dyn AwakeStorage>, String> {
        let guest = Self::guest(&self.root, self.kind);
        self.started
            .lock()
            .expect("started guests")
            .push(Arc::clone(&guest));
        Ok(guest)
    }
}

/// Proxy over a watching double in `dir` that sleeps after `idle`.
fn watching_proxy(
    dir: &tempfile::TempDir,
    kind: bookclerk_storage::fs_kind::FsKind,
    idle: Duration,
) -> (Arc<OnDemandStorage>, Arc<FakeStorage>, Started) {
    let first = WatchingStarter::guest(dir.path(), kind);
    let started: Started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = OnDemandStorage::wrap(
        Arc::clone(&first) as Arc<dyn AwakeStorage>,
        "storage-instance",
        Box::new(WatchingStarter {
            root: dir.path().to_path_buf(),
            kind,
            started: Arc::clone(&started),
        }),
        idle,
    );
    (proxy, first, started)
}

#[tokio::test]
async fn a_live_watch_keeps_the_guest_awake_until_a_reply_says_otherwise() {
    use bookclerk_storage::fs_kind::FsKind;
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, first, _started) = watching_proxy(&dir, FsKind::Local, Duration::from_millis(30));
    assert!(proxy.capabilities().changes);
    let page = proxy
        .changes(None, 10)
        .await
        .expect("changes")
        .expect("a feed");
    assert!(page.complete && page.live_watch, "{page:?}");
    // Well past the idle timeout: the live watch keeps the guest up.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(first.guest_running(), "a live watch keeps the guest");

    // A cursor the feed cannot vouch for: the reply is incomplete, so the
    // hold is released and the guest sleeps.
    let stale = proxy
        .changes(Some("w1.another-watcher.0"), 10)
        .await
        .expect("changes")
        .expect("a feed");
    assert!(!stale.complete);
    wait_asleep(&proxy).await;
    assert!(!first.guest_running());
}

#[tokio::test]
async fn a_watch_that_is_not_live_never_holds_the_guest() {
    use bookclerk_storage::fs_kind::FsKind;
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, first, _started) = watching_proxy(&dir, FsKind::Remote, Duration::from_millis(30));
    let page = proxy
        .changes(None, 10)
        .await
        .expect("changes")
        .expect("a feed");
    assert!(!page.complete && !page.live_watch, "{page:?}");
    wait_asleep(&proxy).await;
    assert!(!first.guest_running());
}

#[tokio::test]
async fn a_held_guest_that_exits_is_released_and_a_changes_error_releases_too() {
    use bookclerk_storage::fs_kind::FsKind;
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, first, started) = watching_proxy(&dir, FsKind::Local, Duration::from_millis(30));
    proxy.changes(None, 10).await.expect("changes");
    // The guest dies on its own. A call wakes a replacement, which sleeps
    // when idle: the hold went with the guest it was for.
    first.running.store(false, Ordering::SeqCst);
    assert!(!proxy.exists("nothing").await.expect("exists"));
    let second = Arc::clone(&started.lock().expect("started")[0]);
    wait_asleep(&proxy).await;
    assert!(!second.guest_running());

    // A fresh guest takes the hold, and a `changes` error releases it.
    proxy.changes(None, 10).await.expect("changes");
    let second = Arc::clone(&started.lock().expect("started")[1]);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(second.guest_running(), "the new guest is held");
    assert!(proxy.changes(Some("not a cursor"), 10).await.is_err());
    wait_asleep(&proxy).await;
    assert!(!second.guest_running());
}

#[tokio::test]
async fn an_open_read_stream_keeps_the_guest_awake_until_it_is_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, _first, started) = proxy(&dir, Duration::from_millis(30));
    proxy
        .put(
            "book.m4b",
            Bytes::from_static(b"audio bytes"),
            ObjectMeta::default(),
        )
        .await
        .expect("put");
    wait_asleep(&proxy).await;
    let (probe, mut body) = proxy.get_stream("book.m4b", None).await.expect("stream");
    assert_eq!(probe.size, 11);
    let reader = Arc::clone(&started.lock().expect("started")[0]);
    // Well past the idle timeout: the open stream keeps the guest up.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(reader.guest_running(), "a read in progress keeps the guest");
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).await.expect("read");
    assert_eq!(bytes, b"audio bytes");
    drop(body);
    wait_asleep(&proxy).await;
    assert!(!reader.guest_running());
}

#[tokio::test]
async fn a_retired_proxy_stops_its_guest_and_refuses_calls() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, first, started) = proxy(&dir, Duration::from_secs(60));
    proxy.retire().await;
    assert!(proxy.is_retired());
    assert!(!first.guest_running());
    let err = proxy.exists("x").await.expect_err("retired");
    assert!(err.to_string().contains("was stopped"), "{err}");
    let err = proxy.session().await.err().expect("retired session");
    assert!(err.contains("was stopped"), "{err}");
    assert!(started.lock().expect("started").is_empty());
    assert!(!proxy.sleep_if_idle().await, "nothing left to stop");
}

#[tokio::test]
async fn a_guest_without_a_session_cannot_lend_one() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (proxy, _first, _started) = proxy(&dir, Duration::from_secs(60));
    let err = proxy.session().await.err().expect("no session");
    assert!(err.contains("has no session"), "{err}");
    assert_eq!(proxy.idle_timeout(), Duration::from_secs(60));
    assert_eq!(proxy.guest_pid(), Some(7070));
}

/// Starter double whose every start fails.
struct FailingStarter;

#[async_trait]
impl GuestStarter<dyn AwakeStorage> for FailingStarter {
    async fn start_guest(&self) -> std::result::Result<Arc<dyn AwakeStorage>, String> {
        Err("jail refused".into())
    }
}

#[tokio::test]
async fn a_failed_wake_is_a_storage_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let proxy = OnDemandStorage::wrap(
        FakeStorage::new(dir.path()) as Arc<dyn AwakeStorage>,
        "storage-instance",
        Box::new(FailingStarter),
        Duration::from_millis(30),
    );
    wait_asleep(&proxy).await;
    let err = proxy.get("x").await.expect_err("start fails");
    assert!(
        err.to_string().contains("could not start: jail refused"),
        "{err}"
    );
}

/// A native manifest with `entrypoints` appended.
fn manifest(runtime: &str, entrypoints: &str) -> crate::PluginManifest {
    let command = if runtime == "native" {
        "command = \"./probe\"\n"
    } else {
        ""
    };
    let text = format!(
        "api_version = 3\nid = \"probe\"\nversion = \"0.1.0\"\nruntime = \"{runtime}\"\n{command}\
         entrypoints = [{entrypoints}]\n\n[capabilities.network]\nmode = \"deny\"\n"
    );
    bookclerk_plugin_manifest::parse(&text).expect("manifest")
}

#[test]
fn python_storage_plugins_sleep() {
    let text = "api_version = 3\nid = \"probe\"\nversion = \"0.1.0\"\n\
runtime = \"workerd\"\nentrypoints = [\"storage\"]\n\n[capabilities.network]\n\
mode = \"deny\"\n\n[workerd]\ncompatibility_date = \"2026-08-01\"\n\
compatibility_flags = [\"python_workers\", \"disable_python_external_sdk\"]\n\
main_module = \"plugin.py\"\n\n[[modules]]\nname = \"plugin.py\"\n\
path = \"plugin.py\"\ntype = \"python\"\n";
    let manifest = bookclerk_plugin_manifest::parse(text).expect("manifest");
    assert!(manifest.declares_python());
    assert!(storage_activates_on_demand(&manifest));
}

#[test]
fn only_plain_storage_plugins_sleep() {
    assert!(storage_activates_on_demand(&manifest(
        "native",
        "\"storage\""
    )));
    assert!(!storage_activates_on_demand(&manifest(
        "native",
        "\"storage\", \"oidc\""
    )));
}
