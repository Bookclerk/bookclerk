//! Wake, idle sleep and retirement of [`OnDemandIntegration`] against an
//! in-process guest double.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_integrations::{
    EventSubscription, Integration, IntegrationContext, IntegrationHealth,
};
use bookclerk_plugin_abi::{DomainEvent, EventResult};

use super::{activates_on_demand, AwakeGuest, OnDemandIntegration};
use crate::activation::{Guest, GuestStarter, OnDemandStatus};

/// Guest double that counts deliveries and can be told it exited.
struct FakeGuest {
    /// Cleared by `stop` or by a test simulating a crash.
    running: AtomicBool,
    /// Deliveries this guest received.
    delivered: AtomicUsize,
    /// How long each delivery takes.
    delivery_time: Duration,
    /// Set when a delivery finished on a guest that had been stopped.
    stopped_mid_delivery: AtomicBool,
    /// When set, each delivery waits (at most [`GATE_LIMIT`]) on `release`.
    held: AtomicBool,
    /// Opens held deliveries.
    release: tokio::sync::Notify,
    /// Whether the guest exports `remoteLibrary`.
    library: bool,
    /// New-user polls this guest answered.
    polls: AtomicUsize,
    /// `externalUserId`s the host acknowledged, in order.
    acked: std::sync::Mutex<Vec<String>>,
    /// `oidcClients` asks this guest answered.
    oidc_asks: AtomicUsize,
    /// Set when `start` was handed a new-user hook.
    started_with_hook: AtomicBool,
    /// While set, `oidcClients` fails.
    oidc_fails: AtomicBool,
}

impl FakeGuest {
    /// A running guest whose deliveries take `delivery_time`.
    fn new(delivery_time: Duration) -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            delivered: AtomicUsize::new(0),
            delivery_time,
            stopped_mid_delivery: AtomicBool::new(false),
            held: AtomicBool::new(false),
            release: tokio::sync::Notify::new(),
            library: false,
            polls: AtomicUsize::new(0),
            acked: std::sync::Mutex::new(Vec::new()),
            oidc_asks: AtomicUsize::new(0),
            started_with_hook: AtomicBool::new(false),
            oidc_fails: AtomicBool::new(false),
        })
    }

    /// A running remote-library guest.
    fn library() -> Arc<Self> {
        let mut guest = Arc::into_inner(Self::new(Duration::ZERO)).expect("sole owner");
        guest.library = true;
        Arc::new(guest)
    }
}

#[async_trait]
impl Integration for FakeGuest {
    fn id(&self) -> &str {
        "fake"
    }

    fn plugin_key(&self) -> &str {
        "local:fake"
    }

    async fn start(&self, ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
        if ctx.on_external_user.is_some() {
            self.started_with_hook.store(true, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn stop(&self) -> bookclerk_integrations::Result<()> {
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn deliver_domain_event(
        &self,
        _event: DomainEvent,
    ) -> bookclerk_integrations::Result<EventResult> {
        tokio::time::sleep(self.delivery_time).await;
        if self.held.load(Ordering::SeqCst) {
            tokio::time::timeout(GATE_LIMIT, self.release.notified())
                .await
                .expect("the test releases a held delivery");
        }
        if !self.running.load(Ordering::SeqCst) {
            self.stopped_mid_delivery.store(true, Ordering::SeqCst);
        }
        self.delivered.fetch_add(1, Ordering::SeqCst);
        Ok(EventResult::Ack)
    }

    fn event_subscriptions(&self) -> Vec<EventSubscription> {
        vec![EventSubscription::new("book.acquired", vec![1])]
    }

    async fn health(&self) -> bookclerk_integrations::Result<IntegrationHealth> {
        Ok(IntegrationHealth {
            id: "fake".into(),
            enabled: true,
            ok: true,
            detail: Some("awake".into()),
        })
    }

    fn supports_library_scan(&self) -> bool {
        self.library
    }

    async fn provided_oidc_clients(
        &self,
    ) -> bookclerk_integrations::Result<Vec<bookclerk_integrations::ProvidedOidcClient>> {
        self.oidc_asks.fetch_add(1, Ordering::SeqCst);
        if self.oidc_fails.load(Ordering::SeqCst) {
            return Err(bookclerk_integrations::IntegrationError::message(
                "oidcClients failed",
            ));
        }
        Ok(vec![bookclerk_integrations::ProvidedOidcClient {
            client_id: "fake".into(),
            display_name: "Fake".into(),
            callback_path: "/cb".into(),
            public_client: true,
            default_scopes: vec!["openid".into()],
            issue_refresh_token: false,
            origin_config_key: String::new(),
        }])
    }
}

#[async_trait]
impl super::ExternalUserPoll for FakeGuest {
    async fn poll_external_users(
        &self,
    ) -> bookclerk_integrations::Result<Vec<bookclerk_integrations::ExternalUser>> {
        let n = self.polls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![bookclerk_integrations::ExternalUser {
            provider: String::new(),
            external_user_id: format!("user-{n}"),
            display_name: None,
            access_token: None,
        }])
    }

    async fn ack_external_users(
        &self,
        external_user_ids: Vec<String>,
    ) -> bookclerk_integrations::Result<()> {
        self.acked.lock().expect("acked").extend(external_user_ids);
        Ok(())
    }

    async fn fetch_oidc_clients(
        &self,
    ) -> bookclerk_integrations::Result<Vec<bookclerk_integrations::ProvidedOidcClient>> {
        Integration::provided_oidc_clients(self).await
    }
}

#[async_trait]
impl crate::SocketSink for FakeGuest {
    async fn deliver_socket_event(
        &self,
        event: crate::SocketEvent,
    ) -> std::result::Result<bookclerk_plugin_abi::SocketMessageOk, String> {
        self.delivered.fetch_add(1, Ordering::SeqCst);
        // A `poll` text frame stands for a message announcing a new user.
        let poll_events = matches!(
            event,
            crate::SocketEvent::Message(bookclerk_plugin_abi::SocketMessage {
                frame: bookclerk_plugin_abi::SocketFrame::Text(ref text),
                ..
            }) if text == "poll"
        );
        Ok(bookclerk_plugin_abi::SocketMessageOk { poll_events })
    }
}

#[async_trait]
impl Guest for FakeGuest {
    fn guest_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn guest_pid(&self) -> Option<u32> {
        self.guest_running().then_some(4242)
    }

    async fn stop_guest(&self) -> std::result::Result<(), String> {
        Integration::stop(self).await.map_err(|err| err.to_string())
    }
}

/// Starter double that hands out fresh [`FakeGuest`]s and remembers them.
struct FakeStarter {
    /// Every guest started on demand, in order.
    started: Started,
    /// Delivery time for the guests it starts.
    delivery_time: Duration,
    /// When set, starting fails with this text.
    fail: Option<String>,
    /// When set, each start waits (at most [`GATE_LIMIT`]) for the test to
    /// open it.
    gate: Option<Arc<tokio::sync::Notify>>,
    /// Whether the guests it starts export `remoteLibrary`.
    library: bool,
}

/// Time between new-user polls in these tests, long enough that only the
/// tests that wait for a poll see one.
const POLL_INTERVAL: Duration = Duration::from_secs(3600);

/// Longest a gated start waits, so a regression fails instead of hanging.
const GATE_LIMIT: Duration = Duration::from_secs(5);

#[async_trait]
impl GuestStarter<dyn AwakeGuest> for FakeStarter {
    async fn start_guest(&self) -> std::result::Result<Arc<dyn AwakeGuest>, String> {
        if let Some(gate) = &self.gate {
            let _ = tokio::time::timeout(GATE_LIMIT, gate.notified()).await;
        }
        if let Some(detail) = &self.fail {
            return Err(detail.clone());
        }
        let guest = if self.library {
            FakeGuest::library()
        } else {
            FakeGuest::new(self.delivery_time)
        };
        self.started
            .lock()
            .expect("started guests")
            .push(Arc::clone(&guest));
        Ok(guest)
    }
}

/// Guests a [`FakeStarter`] started, shared with the test.
type Started = Arc<std::sync::Mutex<Vec<Arc<FakeGuest>>>>;

/// Proxy over a first guest, plus the list of guests started on demand.
fn proxy(
    idle: Duration,
    delivery_time: Duration,
    fail: Option<&str>,
) -> (Arc<OnDemandIntegration>, Arc<FakeGuest>, Started) {
    gated_proxy(idle, delivery_time, fail, None)
}

/// [`proxy`] whose starter waits on `gate` before each start.
fn gated_proxy(
    idle: Duration,
    delivery_time: Duration,
    fail: Option<&str>,
    gate: Option<Arc<tokio::sync::Notify>>,
) -> (Arc<OnDemandIntegration>, Arc<FakeGuest>, Started) {
    let first = FakeGuest::new(delivery_time);
    let started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = OnDemandIntegration::wrap(
        Arc::clone(&first) as Arc<dyn AwakeGuest>,
        "instance-1",
        Box::new(FakeStarter {
            started: Arc::clone(&started),
            delivery_time,
            fail: fail.map(str::to_string),
            gate,
            library: false,
        }),
        Arc::new(std::sync::Mutex::new(IntegrationContext::default())),
        idle,
        POLL_INTERVAL,
    );
    (proxy, first, started)
}

/// Proxy over a remote-library guest that polls for new users every
/// `poll` and sleeps after `idle`.
fn library_proxy(
    idle: Duration,
    poll: Duration,
) -> (Arc<OnDemandIntegration>, Arc<FakeGuest>, Started) {
    let first = FakeGuest::library();
    let started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = OnDemandIntegration::wrap(
        Arc::clone(&first) as Arc<dyn AwakeGuest>,
        "instance-1",
        Box::new(FakeStarter {
            started: Arc::clone(&started),
            delivery_time: Duration::ZERO,
            fail: None,
            gate: None,
            library: true,
        }),
        Arc::new(std::sync::Mutex::new(IntegrationContext::default())),
        idle,
        poll,
    );
    (proxy, first, started)
}

/// A lifecycle context whose new-user hook records each user's id.
fn hooked_context() -> (IntegrationContext, Arc<std::sync::Mutex<Vec<String>>>) {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let ctx = IntegrationContext {
        on_external_user: Some(Arc::new(move |user| {
            assert_eq!(user.provider, "fake", "the alias fills an empty provider");
            sink.lock().expect("seen").push(user.external_user_id);
            Box::pin(async { Ok(()) })
        })),
    };
    (ctx, seen)
}

/// Polls `ready` every 10 ms for at most five seconds.
///
/// # Panics
///
/// Panics with `what` when `ready` never holds.
async fn wait_for(what: &str, ready: impl Fn() -> bool) {
    for _ in 0..500 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

/// Waits until the idle sweep has stopped `first` and emptied the slot.
async fn wait_asleep(proxy: &OnDemandIntegration, first: &FakeGuest) {
    wait_for("the idle sweep", || {
        !first.guest_running() && !proxy.is_awake()
    })
    .await;
}

/// A minimal event to deliver.
fn event() -> DomainEvent {
    DomainEvent {
        event_type: "book.acquired".into(),
        schema_version: 1,
        ..DomainEvent::default()
    }
}

#[tokio::test]
async fn a_sleeping_proxy_keeps_its_identity_and_reports_healthy_without_waking() {
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    assert!(proxy.is_awake());
    wait_asleep(&proxy, &first).await;
    assert_eq!(proxy.plugin_instance_id(), Some("instance-1"));
    assert_eq!(proxy.plugin_key(), "local:fake");
    assert_eq!(proxy.event_subscriptions().len(), 1);
    let health = proxy.health().await.expect("health");
    assert!(health.ok);
    assert_eq!(
        health.detail.as_deref(),
        Some("asleep; starts on its next call")
    );
    assert!(started.lock().expect("started").is_empty());
    assert_eq!(proxy.wakes(), 0);
}

#[tokio::test]
async fn the_next_call_wakes_a_sleeping_guest_and_reuses_it_while_busy() {
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    wait_asleep(&proxy, &first).await;
    for _ in 0..3 {
        assert_eq!(
            proxy
                .deliver_domain_event(event())
                .await
                .expect("delivered"),
            EventResult::Ack
        );
    }
    assert_eq!(proxy.wakes(), 1, "back-to-back calls share one wake");
    let guests = started.lock().expect("started").clone();
    assert_eq!(guests.len(), 1);
    assert_eq!(guests[0].delivered.load(Ordering::SeqCst), 3);
    assert_eq!(proxy.guest_pid(), Some(4242));
}

#[tokio::test]
async fn a_socket_message_wakes_a_sleeping_guest() {
    use crate::SocketSink as _;
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    wait_asleep(&proxy, &first).await;
    let message = bookclerk_plugin_abi::SocketMessage {
        socket: "s1".into(),
        frame: bookclerk_plugin_abi::SocketFrame::Text("42".into()),
        attachment: b"cursor".to_vec(),
    };
    proxy
        .deliver_socket_event(crate::SocketEvent::Message(message))
        .await
        .expect("delivered");
    assert_eq!(proxy.wakes(), 1);
    let guests = started.lock().expect("started").clone();
    assert_eq!(guests[0].delivered.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_call_in_flight_keeps_the_guest_past_the_idle_timeout() {
    let (proxy, first, started) = proxy(Duration::from_millis(30), Duration::ZERO, None);
    first.held.store(true, Ordering::SeqCst);
    let delivering = {
        let proxy = Arc::clone(&proxy);
        tokio::spawn(async move { proxy.deliver_domain_event(event()).await })
    };
    wait_for("the delivery to start", || {
        proxy.activation.in_flight() == 1
    })
    .await;
    // Past the idle timeout, an idle check with the call still pending must
    // leave the guest running.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(proxy.activation.in_flight(), 1, "the call is still pending");
    assert!(!proxy.sleep_if_idle().await, "slept with a call in flight");
    assert!(first.guest_running());
    first.release.notify_one();
    tokio::time::timeout(GATE_LIMIT, delivering)
        .await
        .expect("delivery finishes")
        .expect("join")
        .expect("delivered");
    assert_eq!(first.delivered.load(Ordering::SeqCst), 1);
    assert!(
        !first.stopped_mid_delivery.load(Ordering::SeqCst),
        "the sweep stopped a guest with a call in flight"
    );
    assert!(first.guest_running());
    assert!(started.lock().expect("started").is_empty());
}

#[tokio::test]
async fn concurrent_calls_on_a_sleeping_proxy_start_one_guest() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (proxy, first, started) = gated_proxy(
        Duration::from_millis(40),
        Duration::from_millis(20),
        None,
        Some(Arc::clone(&gate)),
    );
    wait_asleep(&proxy, &first).await;
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let proxy = Arc::clone(&proxy);
            tokio::spawn(async move { proxy.deliver_domain_event(event()).await })
        })
        .collect();
    // Hold the first wake until every call is waiting on it.
    wait_for("all eight calls", || proxy.activation.in_flight() == 8).await;
    gate.notify_one();
    for call in calls {
        call.await.expect("join").expect("delivered");
    }
    assert_eq!(started.lock().expect("started").len(), 1);
}

#[tokio::test]
async fn an_exited_guest_is_replaced_on_the_next_call() {
    let (proxy, first, started) = proxy(Duration::from_secs(60), Duration::ZERO, None);
    first.running.store(false, Ordering::SeqCst);
    proxy
        .deliver_domain_event(event())
        .await
        .expect("delivered");
    assert_eq!(first.delivered.load(Ordering::SeqCst), 0);
    assert_eq!(started.lock().expect("started").len(), 1);
}

#[tokio::test]
async fn a_failed_wake_is_an_error_the_worker_retries() {
    let (proxy, first, _started) = proxy(
        Duration::from_millis(40),
        Duration::ZERO,
        Some("spawn refused"),
    );
    wait_asleep(&proxy, &first).await;
    let err = proxy
        .deliver_domain_event(event())
        .await
        .expect_err("wake fails");
    assert!(err.to_string().contains("spawn refused"), "{err}");
    assert!(!proxy.is_awake());
}

#[tokio::test]
async fn a_stopped_proxy_never_wakes_again() {
    let (proxy, first, started) = proxy(Duration::from_secs(60), Duration::ZERO, None);
    proxy.stop().await.expect("stop");
    assert!(proxy.is_retired());
    assert!(!first.guest_running());
    let err = proxy
        .deliver_domain_event(event())
        .await
        .expect_err("retired");
    assert!(err.to_string().contains("was stopped"), "{err}");
    assert!(started.lock().expect("started").is_empty());
}

#[tokio::test]
async fn starting_a_stopped_proxy_revives_it_for_a_rollback() {
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    proxy.stop().await.expect("stop");
    proxy
        .start(IntegrationContext::default())
        .await
        .expect("restart");
    assert!(!proxy.is_retired());
    assert!(!first.guest_running());
    assert_eq!(
        proxy
            .deliver_domain_event(event())
            .await
            .expect("delivered after the rollback"),
        EventResult::Ack
    );
    let woken = started.lock().expect("started").clone();
    assert_eq!(woken.len(), 1);
    // The sweep kept running through the stop, so the revived proxy sleeps.
    wait_for("the revived guest to sleep", || {
        !woken[0].guest_running() && !proxy.is_awake()
    })
    .await;
}

/// A native manifest with `entrypoints` and `extra` TOML appended.
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

/// `[[events.consumers]]` for one event type.
const EVENTS: &str = "[[events.consumers]]\ntype = \"book_acquired\"\nschema_versions = [1]\n";

#[test]
fn trigger_and_lookup_plugins_activate_on_demand() {
    assert!(activates_on_demand(&manifest("native", "", EVENTS)));
    assert!(activates_on_demand(&manifest("native", "\"cli\"", EVENTS)));
    assert!(activates_on_demand(&manifest(
        "native",
        "\"metadata\"",
        MEDIA
    )));
    assert!(activates_on_demand(&manifest(
        "native",
        "\"mediaDownload\"",
        ""
    )));
}

/// `[workerd]` and a module for a Python isolate.
const PYTHON: &str = "[workerd]\ncompatibility_date = \"2026-08-01\"\n\
compatibility_flags = [\"python_workers\", \"disable_python_external_sdk\"]\n\
main_module = \"plugin.py\"\n\n[[modules]]\nname = \"plugin.py\"\n\
path = \"plugin.py\"\ntype = \"python\"\n";

#[test]
fn python_integrations_activate_on_demand() {
    assert!(activates_on_demand(&manifest(
        "workerd",
        "",
        &format!("{EVENTS}\n{PYTHON}")
    )));
}

#[test]
fn remote_libraries_and_sign_in_providers_activate_on_demand() {
    assert!(activates_on_demand(&manifest(
        "native",
        "\"remoteLibrary\", \"oidc\"",
        EVENTS
    )));
    assert!(activates_on_demand(&manifest("native", "\"oidc\"", "")));
}

#[test]
fn storefront_and_storage_plugins_are_not_integration_proxies() {
    assert!(!activates_on_demand(&manifest(
        "native",
        "\"storefront\"",
        &format!("{MEDIA}\n{EVENTS}")
    )));
    assert!(!activates_on_demand(&manifest("native", "\"storage\"", "")));
}

#[tokio::test]
async fn every_guest_call_wakes_a_sleeping_proxy_and_reaches_the_guest() {
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    wait_asleep(&proxy, &first).await;
    let cancel = || Arc::new(AtomicBool::new(false));

    assert_eq!(
        proxy
            .deliver_domain_event_cancelable(event(), cancel())
            .await
            .expect("delivered"),
        EventResult::Ack
    );
    let cron = bookclerk_plugin_manifest::CronSchedule::parse("*/5 * * * *").expect("cron");
    let scheduled = proxy
        .run_scheduled_cancelable(
            bookclerk_integrations::ScheduledInvocation {
                cron,
                scheduled_time_ms: 0,
                job_id: "job-1".into(),
                attempt: 1,
                deadline_unix_ms: 0,
            },
            cancel(),
        )
        .await
        .expect_err("the double declares no cron");
    assert!(
        scheduled.to_string().contains("declares no cron"),
        "{scheduled}"
    );
    let queued = proxy
        .deliver_queue_batch_cancelable(
            bookclerk_plugin_abi::QueueBatch {
                queue: "echo".into(),
                ..bookclerk_plugin_abi::QueueBatch::default()
            },
            cancel(),
        )
        .await
        .expect_err("the double consumes no queues");
    assert!(
        queued.to_string().contains("consumes no queues"),
        "{queued}"
    );
    for err in [
        proxy
            .metadata_lookup(bookclerk_plugin_abi::MetadataLookupParams::default())
            .await
            .expect_err("no metadata"),
        proxy
            .metadata_search(bookclerk_plugin_abi::MetadataSearchParams::default())
            .await
            .expect_err("no metadata"),
    ] {
        assert!(
            err.to_string().contains("does not provide metadata"),
            "{err}"
        );
    }
    let chapters = proxy
        .metadata_chapters(bookclerk_plugin_abi::MetadataChaptersParams::default())
        .await
        .expect_err("no metadata");
    assert!(chapters.to_string().contains("does not provide metadata"));
    let covers = proxy
        .download_media(bookclerk_plugin_abi::MediaDownloadParams::default())
        .await
        .expect_err("no covers");
    assert!(covers.to_string().contains("does not download media"));
    // A per-run value, not a literal: the double rejects every login.
    let password = format!("probe-{}", std::process::id());
    let login = proxy
        .authenticate_user("reader", &password)
        .await
        .expect_err("no login");
    assert!(login.to_string().contains("credential login"));
    let scan = proxy.scan_library(false).await.expect_err("no scan");
    assert!(scan.to_string().contains("library scan"));
    assert_eq!(proxy.provided_oidc_clients().await.expect("oidc").len(), 1);
    let diagnosis = proxy.diagnose().await.expect("diagnose");
    assert!(
        diagnosis.iter().any(|line| line == "awake"),
        "{diagnosis:?}"
    );

    assert_eq!(
        proxy.wakes(),
        1,
        "every call after the wake reuses the guest"
    );
    assert_eq!(started.lock().expect("started").len(), 1);
    assert_eq!(proxy.display_name(), "fake");
    assert!(!proxy.supports_metadata());
    assert!(!proxy.supports_media_download());
    assert!(!proxy.supports_credential_login());
    assert!(!proxy.supports_library_scan());
    assert!(!proxy.supports_listening_sync());
    assert!(proxy.portal_brand().is_none());
    assert!(proxy.queue_subscriptions().is_empty());
    assert!(proxy.cron_schedules().is_empty());
    assert_eq!(proxy.idle_timeout(), Duration::from_millis(40));
    proxy
        .start(IntegrationContext::default())
        .await
        .expect("start forwards to the running guest");
}

#[tokio::test]
async fn a_call_waiting_on_a_wake_does_not_start_a_guest_after_stop() {
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::from_millis(20), None);
    wait_asleep(&proxy, &first).await;
    // Hold the slot so the call passes the first retirement check and then
    // waits, as it would behind a concurrent `stop`.
    let slot = proxy.activation.slot().lock().await;
    let waiting = {
        let proxy = Arc::clone(&proxy);
        tokio::spawn(async move { proxy.deliver_domain_event(event()).await })
    };
    wait_for("the call to wait on the slot", || {
        proxy.activation.in_flight() == 1
    })
    .await;
    proxy.activation.mark_retired();
    drop(slot);
    let err = waiting
        .await
        .expect("join")
        .expect_err("retired while waiting");
    assert!(err.to_string().contains("was stopped"), "{err}");
    assert!(started.lock().expect("started").is_empty());
}

#[tokio::test]
async fn a_guest_that_finishes_starting_after_stop_is_stopped_unused() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (proxy, first, started) = gated_proxy(
        Duration::from_millis(40),
        Duration::ZERO,
        None,
        Some(Arc::clone(&gate)),
    );
    wait_asleep(&proxy, &first).await;
    let waking = {
        let proxy = Arc::clone(&proxy);
        tokio::spawn(async move { proxy.deliver_domain_event(event()).await })
    };
    // The wake holds the slot while the starter waits on the gate.
    wait_for("the wake to hold the slot", || {
        proxy.activation.slot().try_lock().is_err()
    })
    .await;
    proxy.activation.mark_retired();
    gate.notify_one();
    let err = waking
        .await
        .expect("join")
        .expect_err("retired during the wake");
    assert!(err.to_string().contains("was stopped"), "{err}");
    let guests = started.lock().expect("started").clone();
    assert_eq!(guests.len(), 1);
    assert!(!guests[0].guest_running(), "the late guest was stopped");
    assert_eq!(guests[0].delivered.load(Ordering::SeqCst), 0);
    assert_eq!(proxy.wakes(), 0);
}

#[tokio::test]
async fn the_proxy_polls_a_sleeping_remote_library_through_a_wake() {
    let (proxy, first, started) =
        library_proxy(Duration::from_millis(40), Duration::from_millis(150));
    let (ctx, seen) = hooked_context();
    proxy.start(ctx).await.expect("start");
    assert!(
        !first.started_with_hook.load(Ordering::SeqCst),
        "the guest never gets the hook"
    );
    wait_asleep(&proxy, &first).await;
    assert_eq!(
        first.polls.load(Ordering::SeqCst),
        0,
        "the guest slept unpolled"
    );
    wait_for("the first poll", || seen.lock().expect("seen").len() == 1).await;
    let woken = started.lock().expect("started").clone();
    assert_eq!(woken.len(), 1, "the poll woke one guest");
    assert!(!woken[0].started_with_hook.load(Ordering::SeqCst));
    // The users that reached the hook are acknowledged to the guest that
    // reported them.
    wait_for("the acknowledgement", || {
        woken[0].acked.lock().expect("acked").as_slice() == ["user-0"]
    })
    .await;
    // The guest sleeps again before the next poll, which wakes another.
    wait_for("the polled guest to sleep", || {
        !woken[0].guest_running() && !proxy.is_awake()
    })
    .await;
    wait_for("the second poll", || seen.lock().expect("seen").len() == 2).await;
    assert_eq!(started.lock().expect("started").len(), 2);
    assert_eq!(
        seen.lock().expect("seen").clone(),
        ["user-0", "user-0"],
        "each wake polls a fresh guest"
    );
}

#[tokio::test]
async fn a_pushed_new_user_is_polled_at_once() {
    use crate::SocketSink as _;
    // A poll interval far past the test, so only the guest's ask polls.
    let (proxy, first, started) =
        library_proxy(Duration::from_millis(40), Duration::from_secs(3600));
    let (ctx, seen) = hooked_context();
    proxy.start(ctx).await.expect("start");
    wait_asleep(&proxy, &first).await;
    let message = |text: &str| {
        crate::SocketEvent::Message(bookclerk_plugin_abi::SocketMessage {
            socket: "s1".into(),
            frame: bookclerk_plugin_abi::SocketFrame::Text(text.into()),
            attachment: Vec::new(),
        })
    };
    let quiet = proxy
        .deliver_socket_event(message("42"))
        .await
        .expect("delivered");
    assert!(!quiet.poll_events);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        seen.lock().expect("seen").is_empty(),
        "an ordinary message polls nothing"
    );
    let asks = proxy
        .deliver_socket_event(message("poll"))
        .await
        .expect("delivered");
    assert!(asks.poll_events);
    wait_for("the asked-for poll", || {
        seen.lock().expect("seen").len() == 1
    })
    .await;
    // Each message woke a guest (the first slept again in between); the
    // poll used the guest the asking message woke, starting no other.
    let guests = started.lock().expect("started").clone();
    assert_eq!(guests.len(), 2, "the poll started no guest of its own");
    assert_eq!(guests[0].polls.load(Ordering::SeqCst), 0);
    assert_eq!(guests[1].polls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_ask_with_no_poll_running_is_dropped() {
    let generations = crate::user_poll::PollGeneration::default();
    generations.poll_now();
    let polls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polls);
    crate::user_poll::spawn(
        &generations,
        Duration::from_secs(3600),
        "fake".into(),
        Arc::new(|_| Box::pin(async { Ok(()) })),
        move || {
            counter.fetch_add(1, Ordering::SeqCst);
            async { Some(Ok(Vec::new())) }
        },
        |_ids| async { Ok(()) },
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        polls.load(Ordering::SeqCst),
        0,
        "the earlier ask left nothing behind"
    );
    generations.poll_now();
    wait_for("the asked-for poll", || polls.load(Ordering::SeqCst) == 1).await;
    generations.end();
    generations.poll_now();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        polls.load(Ordering::SeqCst),
        1,
        "an ended loop is not asked"
    );
}

#[tokio::test]
async fn stopping_the_proxy_ends_its_user_poll() {
    let (proxy, first, started) =
        library_proxy(Duration::from_millis(40), Duration::from_millis(60));
    let (ctx, seen) = hooked_context();
    proxy.start(ctx).await.expect("start");
    proxy.stop().await.expect("stop");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(seen.lock().expect("seen").is_empty());
    assert_eq!(first.polls.load(Ordering::SeqCst), 0);
    assert!(started.lock().expect("started").is_empty());
}

#[tokio::test]
async fn a_proxy_without_a_remote_library_starts_no_user_poll() {
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    let (ctx, seen) = hooked_context();
    proxy.start(ctx).await.expect("start");
    assert!(!first.started_with_hook.load(Ordering::SeqCst));
    wait_asleep(&proxy, &first).await;
    assert!(seen.lock().expect("seen").is_empty());
    assert!(started.lock().expect("started").is_empty());
}

#[tokio::test]
async fn sign_in_clients_are_read_from_the_guest_once() {
    let (proxy, first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    let clients = proxy.provided_oidc_clients().await.expect("clients");
    assert_eq!(clients.len(), 1);
    wait_asleep(&proxy, &first).await;
    let again = proxy.provided_oidc_clients().await.expect("cached clients");
    assert_eq!(again[0].client_id, "fake");
    assert_eq!(first.oidc_asks.load(Ordering::SeqCst), 1);
    assert!(
        started.lock().expect("started").is_empty(),
        "a later ask does not wake the guest"
    );
}

#[tokio::test]
async fn a_failed_sign_in_client_read_is_retried_on_the_next_ask() {
    let (proxy, first, _started) = proxy(Duration::from_secs(60), Duration::ZERO, None);
    first.oidc_fails.store(true, Ordering::SeqCst);
    let failed = proxy.provided_oidc_clients().await.expect("fallback");
    assert!(failed.is_empty(), "a failure reads as no clients");
    first.oidc_fails.store(false, Ordering::SeqCst);
    assert_eq!(
        proxy.provided_oidc_clients().await.expect("clients").len(),
        1
    );
    assert_eq!(
        proxy.provided_oidc_clients().await.expect("cached").len(),
        1
    );
    assert_eq!(
        first.oidc_asks.load(Ordering::SeqCst),
        2,
        "only the success is kept"
    );
}

/// A grant holding the given database bindings.
fn grant_with_databases(databases: &[&str]) -> crate::PluginGrant {
    crate::PluginGrant {
        bindings: databases
            .iter()
            .map(|name| format!("{}{name}", crate::consent::DATABASE_BINDING_PREFIX))
            .collect(),
        ..crate::PluginGrant::empty()
    }
}

#[test]
fn a_remote_library_sleeps_only_with_every_database_granted() {
    use super::state_survives_sleep;
    let library = manifest(
        "native",
        "\"remoteLibrary\"",
        "[[databases]]\nbinding = \"LISTENING_STATE\"\n",
    );
    assert!(state_survives_sleep(
        &library,
        &grant_with_databases(&["LISTENING_STATE"])
    ));
    assert!(!state_survives_sleep(&library, &grant_with_databases(&[])));
    let stateless = manifest("native", "\"remoteLibrary\"", "");
    assert!(
        !state_survives_sleep(&stateless, &grant_with_databases(&[])),
        "a remote library with no database keeps its memory in the process"
    );
    let trigger = manifest("native", "", EVENTS);
    assert!(state_survives_sleep(&trigger, &grant_with_databases(&[])));
}
