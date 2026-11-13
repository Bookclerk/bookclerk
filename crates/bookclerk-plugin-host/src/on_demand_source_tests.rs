//! Wake, idle sleep and retirement of [`OnDemandSource`] against an
//! in-process storefront double.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_library::{LibraryStore, SourceScope};
use bookclerk_source::{
    CatalogHit, ContentSource, FetchOptions, ImportCredentialsOptions, LoginOptions,
    PortalAuthMode, PurchaseHintOpts, ScanOptions, ScanSummary, SourceAccount, SourceBrand,
    SourceError, SourceFetch, SourcePurchaseHint,
};

use super::{storefront_activates_on_demand, AwakeSource, OnDemandSource};
use crate::activation::{Guest, GuestStarter, OnDemandStatus};

/// Brand the double reports.
const BRAND: SourceBrand = SourceBrand {
    id: "fakestore",
    name: "Fake Store",
    bg: "#000000",
    fg: "#ffffff",
    accent: "#888888",
    icon_url: "",
};

/// Storefront double that counts the calls it answers.
struct FakeSource {
    /// Cleared when the double is told its process exited.
    running: AtomicBool,
    /// Calls this guest answered.
    calls: AtomicUsize,
}

impl FakeSource {
    /// A running double.
    fn new() -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            calls: AtomicUsize::new(0),
        })
    }

    /// Counts one answered call.
    fn answered(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl ContentSource for FakeSource {
    fn id(&self) -> &str {
        "fakestore"
    }

    fn plugin_key(&self) -> &str {
        "local:fakestore"
    }

    fn display_name(&self) -> &str {
        "Fake Store"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["fake"]
    }

    fn portal_auth_mode(&self) -> PortalAuthMode {
        PortalAuthMode::Password
    }

    fn portal_brand(&self) -> SourceBrand {
        BRAND
    }

    fn sort_key(&self) -> u32 {
        7
    }

    fn answers_catalog_detail_by_isbn(&self) -> bool {
        true
    }

    async fn login(
        &self,
        _scope: &SourceScope,
        _opts: LoginOptions,
    ) -> bookclerk_source::Result<SourceAccount> {
        self.answered();
        Err(SourceError::api("login refused by the double"))
    }

    async fn import_credentials(
        &self,
        _scope: &SourceScope,
        _path: &std::path::Path,
        _opts: ImportCredentialsOptions,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        self.answered();
        Ok(Vec::new())
    }

    async fn revoke_credentials(
        &self,
        _scope: &SourceScope,
        _account_id: &str,
    ) -> bookclerk_source::Result<()> {
        self.answered();
        Ok(())
    }

    async fn list_accounts(
        &self,
        _scope: &SourceScope,
    ) -> bookclerk_source::Result<Vec<SourceAccount>> {
        panic!("the proxy lists accounts from the library");
    }

    async fn scan(
        &self,
        _scope: &SourceScope,
        _opts: ScanOptions,
    ) -> bookclerk_source::Result<ScanSummary> {
        self.answered();
        Ok(ScanSummary::default())
    }

    async fn fetch_title(
        &self,
        _scope: &SourceScope,
        _account_id: &str,
        _title_id: &str,
        _opts: &FetchOptions,
    ) -> bookclerk_source::Result<SourceFetch> {
        self.answered();
        Ok(SourceFetch {
            parts: Vec::new(),
            m4b_path: None,
            cover_path: None,
            chapters: Vec::new(),
            pdf_url: None,
        })
    }

    async fn inspect_title(
        &self,
        _scope: &SourceScope,
        _account_id: &str,
        title_id: &str,
        _opts: &FetchOptions,
    ) -> bookclerk_source::Result<serde_json::Value> {
        self.answered();
        Ok(serde_json::json!({ "title": title_id }))
    }

    async fn catalog_detail(
        &self,
        product_id: &str,
    ) -> bookclerk_source::Result<Option<CatalogHit>> {
        self.answered();
        Ok(Some(CatalogHit {
            product_id: product_id.to_string(),
            ..CatalogHit::default()
        }))
    }

    async fn catalog_detail_by_isbn(
        &self,
        isbn: &str,
    ) -> bookclerk_source::Result<Option<CatalogHit>> {
        self.catalog_detail(isbn).await
    }

    async fn purchase_hint(
        &self,
        _opts: &PurchaseHintOpts,
    ) -> bookclerk_source::Result<Option<SourcePurchaseHint>> {
        self.answered();
        Ok(None)
    }

    async fn list_deals(&self, _limit: usize) -> bookclerk_source::Result<Vec<CatalogHit>> {
        self.answered();
        Ok(Vec::new())
    }
}

#[async_trait]
impl Guest for FakeSource {
    fn guest_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn guest_pid(&self) -> Option<u32> {
        self.guest_running().then_some(5150)
    }

    async fn stop_guest(&self) -> std::result::Result<(), String> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Guests a [`FakeStarter`] started, shared with the test.
type Started = Arc<std::sync::Mutex<Vec<Arc<FakeSource>>>>;

/// Starter double that hands out fresh [`FakeSource`]s.
struct FakeStarter {
    /// Every guest started on demand, in order.
    started: Started,
}

#[async_trait]
impl GuestStarter<dyn AwakeSource> for FakeStarter {
    async fn start_guest(&self) -> std::result::Result<Arc<dyn AwakeSource>, String> {
        let guest = FakeSource::new();
        self.started
            .lock()
            .expect("started guests")
            .push(Arc::clone(&guest));
        Ok(guest)
    }
}

/// Proxy over a first double that sleeps after `idle`.
fn proxy(idle: Duration) -> (Arc<OnDemandSource>, Arc<FakeSource>, Started) {
    let first = FakeSource::new();
    let started: Started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = OnDemandSource::wrap(
        Arc::clone(&first) as Arc<dyn AwakeSource>,
        "store-instance",
        Box::new(FakeStarter {
            started: Arc::clone(&started),
        }),
        idle,
    );
    (proxy, first, started)
}

/// A library scope backed by a fresh SQLite file.
async fn scope(dir: &tempfile::TempDir) -> SourceScope {
    let db = bookclerk_plugin_database_sqlite::open(&dir.path().join("library.db"))
        .await
        .expect("open sqlite");
    bookclerk_library::apply_host_schema(&db)
        .await
        .expect("schema");
    LibraryStore::from_connection(db).scope("local:fakestore", "fakestore")
}

/// Waits until `proxy` has stopped its guest.
async fn wait_asleep(proxy: &OnDemandSource) {
    for _ in 0..100 {
        if !proxy.is_awake() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("idle storefront did not sleep");
}

#[tokio::test]
async fn a_sleeping_storefront_keeps_its_identity_and_lists_accounts_without_waking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scope = scope(&dir).await;
    let (proxy, first, started) = proxy(Duration::from_millis(30));
    wait_asleep(&proxy).await;
    assert!(!first.guest_running());
    assert_eq!(proxy.id(), "fakestore");
    assert_eq!(proxy.plugin_key(), "local:fakestore");
    assert_eq!(proxy.plugin_instance_id(), Some("store-instance"));
    assert_eq!(proxy.display_name(), "Fake Store");
    assert_eq!(proxy.aliases(), &["fake"]);
    assert_eq!(proxy.portal_auth_mode(), PortalAuthMode::Password);
    assert_eq!(proxy.portal_brand(), BRAND);
    assert_eq!(proxy.sort_key(), 7);
    assert!(proxy.answers_catalog_detail_by_isbn());
    assert!(proxy.config_options().is_empty());
    assert_eq!(ContentSource::guest_pid(proxy.as_ref()), None);
    assert!(proxy
        .list_accounts(&scope)
        .await
        .expect("accounts")
        .is_empty());
    assert_eq!(proxy.wakes(), 0);
    assert!(started.lock().expect("started").is_empty());
}

#[tokio::test]
async fn storefront_calls_wake_one_guest_and_reach_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scope = scope(&dir).await;
    let (proxy, _first, started) = proxy(Duration::from_millis(30));
    wait_asleep(&proxy).await;
    let opts = FetchOptions {
        download: bookclerk_source::DownloadOptions::default(),
        cache_dir: dir.path().to_path_buf(),
        files_dir: dir.path().to_path_buf(),
        max_cache_bytes: None,
        cancel: None,
    };

    let login = proxy
        .login(&scope, LoginOptions::default())
        .await
        .expect_err("the double refuses login");
    assert!(login.to_string().contains("refused"), "{login}");
    assert!(proxy
        .login_with_oauth_progress(&scope, LoginOptions::default(), &|_| {})
        .await
        .is_err());
    proxy
        .import_credentials(&scope, dir.path(), ImportCredentialsOptions::default())
        .await
        .expect("import");
    proxy
        .revoke_credentials(&scope, "acct")
        .await
        .expect("revoke");
    proxy
        .scan(&scope, ScanOptions::default())
        .await
        .expect("scan");
    proxy
        .fetch_title(&scope, "acct", "title-1", &opts)
        .await
        .expect("fetch");
    assert_eq!(
        proxy
            .inspect_title(&scope, "acct", "title-1", &opts)
            .await
            .expect("inspect")["title"],
        "title-1"
    );
    assert!(proxy
        .catalog_detail("B0001")
        .await
        .expect("detail")
        .is_some());
    assert!(proxy
        .catalog_detail_by_isbn("9780000000001")
        .await
        .expect("isbn")
        .is_some());
    assert!(proxy
        .purchase_hint(&PurchaseHintOpts::default())
        .await
        .expect("hint")
        .is_none());
    assert_eq!(
        proxy
            .purchase_hints(&[PurchaseHintOpts::default(), PurchaseHintOpts::default()])
            .await
            .expect("hints")
            .len(),
        2
    );
    assert!(proxy.list_deals(5).await.expect("deals").is_empty());

    assert_eq!(proxy.wakes(), 1);
    let guests = started.lock().expect("started").clone();
    assert_eq!(guests.len(), 1);
    assert_eq!(guests[0].calls.load(Ordering::SeqCst), 13);
    assert_eq!(ContentSource::guest_pid(proxy.as_ref()), Some(5150));
    assert_eq!(proxy.idle_timeout(), Duration::from_millis(30));
}

#[tokio::test]
async fn a_retired_storefront_stops_its_guest_and_never_wakes() {
    let (proxy, first, started) = proxy(Duration::from_secs(60));
    proxy.retire().await;
    assert!(proxy.is_retired());
    assert!(!first.guest_running());
    let err = proxy.list_deals(1).await.expect_err("retired");
    assert!(err.to_string().contains("was stopped"), "{err}");
    assert!(started.lock().expect("started").is_empty());
    assert!(!proxy.sleep_if_idle().await, "nothing left to stop");
}

/// A manifest with `entrypoints` and `extra` TOML appended.
fn manifest(runtime: &str, entrypoints: &str, extra: &str) -> crate::PluginManifest {
    let command = if runtime == "native" {
        "command = \"./probe\"\n"
    } else {
        ""
    };
    let text = format!(
        "api_version = 3\nid = \"probe\"\nversion = \"0.1.0\"\nruntime = \"{runtime}\"\n{command}\
         entrypoints = [{entrypoints}]\n\n[capabilities.network]\nmode = \"deny\"\n\n{extra}"
    );
    bookclerk_plugin_manifest::parse(&text).expect("manifest")
}

/// `[media]` for a storefront or metadata plugin.
const MEDIA: &str = "[media]\nworks = [\"book\"]\nlists = [\"audio\"]\n";

#[test]
fn only_plain_storefronts_sleep() {
    assert!(storefront_activates_on_demand(&manifest(
        "native",
        "\"storefront\"",
        MEDIA
    )));
    assert!(!storefront_activates_on_demand(&manifest(
        "native",
        "\"storefront\", \"metadata\"",
        MEDIA
    )));
    assert!(!storefront_activates_on_demand(&manifest(
        "native",
        "\"storage\"",
        ""
    )));
}
