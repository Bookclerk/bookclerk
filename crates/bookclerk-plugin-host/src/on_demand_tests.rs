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

use super::{activates_on_demand, AwakeGuest, GuestStarter, OnDemandIntegration};

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
        })
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

    async fn start(&self, _ctx: IntegrationContext) -> bookclerk_integrations::Result<()> {
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
}

impl AwakeGuest for FakeGuest {
    fn guest_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn guest_pid(&self) -> Option<u32> {
        self.guest_running().then_some(4242)
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
}

/// Longest a gated start waits, so a regression fails instead of hanging.
const GATE_LIMIT: Duration = Duration::from_secs(5);

#[async_trait]
impl GuestStarter for FakeStarter {
    async fn start_guest(
        &self,
        _ctx: IntegrationContext,
    ) -> std::result::Result<Arc<dyn AwakeGuest>, String> {
        if let Some(gate) = &self.gate {
            let _ = tokio::time::timeout(GATE_LIMIT, gate.notified()).await;
        }
        if let Some(detail) = &self.fail {
            return Err(detail.clone());
        }
        let guest = FakeGuest::new(self.delivery_time);
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
        }),
        IntegrationContext::default(),
        idle,
    );
    (proxy, first, started)
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
async fn a_call_in_flight_keeps_the_guest_past_the_idle_timeout() {
    let (proxy, first, started) = proxy(Duration::from_millis(30), Duration::ZERO, None);
    first.held.store(true, Ordering::SeqCst);
    let delivering = {
        let proxy = Arc::clone(&proxy);
        tokio::spawn(async move { proxy.deliver_domain_event(event()).await })
    };
    wait_for("the delivery to start", || proxy.activity().in_flight == 1).await;
    // Past the idle timeout, an idle check with the call still pending must
    // leave the guest running.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(proxy.activity().in_flight, 1, "the call is still pending");
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
    wait_for("all eight calls", || proxy.activity().in_flight == 8).await;
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
    assert!(activates_on_demand(&manifest("native", "\"covers\"", "")));
}

#[test]
fn stateful_sign_in_and_registry_plugins_stay_resident() {
    assert!(!activates_on_demand(&manifest(
        "native",
        "\"remoteLibrary\"",
        ""
    )));
    assert!(!activates_on_demand(&manifest("native", "\"oidc\"", "")));
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
        .fetch_covers(bookclerk_plugin_abi::CoverFetchParams::default())
        .await
        .expect_err("no covers");
    assert!(covers.to_string().contains("does not fetch covers"));
    // A per-run value, not a literal: the double rejects every login.
    let password = format!("probe-{}", std::process::id());
    let login = proxy
        .authenticate_user("reader", &password)
        .await
        .expect_err("no login");
    assert!(login.to_string().contains("credential login"));
    let scan = proxy.scan_library(false).await.expect_err("no scan");
    assert!(scan.to_string().contains("library scan"));
    assert!(proxy
        .provided_oidc_clients()
        .await
        .expect("oidc")
        .is_empty());
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
    assert!(!proxy.supports_covers());
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
    let slot = proxy.guest.lock().await;
    let waiting = {
        let proxy = Arc::clone(&proxy);
        tokio::spawn(async move { proxy.deliver_domain_event(event()).await })
    };
    wait_for("the call to wait on the slot", || {
        proxy.activity().in_flight == 1
    })
    .await;
    proxy.retired.store(true, Ordering::SeqCst);
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
        proxy.guest.try_lock().is_err()
    })
    .await;
    proxy.retired.store(true, Ordering::SeqCst);
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
