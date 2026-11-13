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
}

impl FakeGuest {
    /// A running guest whose deliveries take `delivery_time`.
    fn new(delivery_time: Duration) -> Arc<Self> {
        Arc::new(Self {
            running: AtomicBool::new(true),
            delivered: AtomicUsize::new(0),
            delivery_time,
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
}

#[async_trait]
impl GuestStarter for FakeStarter {
    async fn start_guest(
        &self,
        _ctx: IntegrationContext,
    ) -> std::result::Result<Arc<dyn AwakeGuest>, String> {
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
    let first = FakeGuest::new(delivery_time);
    let started = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = OnDemandIntegration::wrap(
        Arc::clone(&first) as Arc<dyn AwakeGuest>,
        "instance-1",
        Box::new(FakeStarter {
            started: Arc::clone(&started),
            delivery_time,
            fail: fail.map(str::to_string),
        }),
        IntegrationContext::default(),
        idle,
    );
    (proxy, first, started)
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
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!proxy.is_awake(), "idle guest should have been stopped");
    assert!(!first.guest_running());
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
    let (proxy, _first, started) = proxy(Duration::from_millis(40), Duration::ZERO, None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!proxy.is_awake());
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
    let (proxy, first, started) =
        proxy(Duration::from_millis(30), Duration::from_millis(250), None);
    proxy
        .deliver_domain_event(event())
        .await
        .expect("delivered");
    assert_eq!(first.delivered.load(Ordering::SeqCst), 1);
    assert!(started.lock().expect("started").is_empty());
}

#[tokio::test]
async fn concurrent_calls_on_a_sleeping_proxy_start_one_guest() {
    let (proxy, _first, started) =
        proxy(Duration::from_millis(40), Duration::from_millis(20), None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let calls = (0..8).map(|_| {
        let proxy = Arc::clone(&proxy);
        tokio::spawn(async move { proxy.deliver_domain_event(event()).await })
    });
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
    let (proxy, _first, _started) = proxy(
        Duration::from_millis(40),
        Duration::ZERO,
        Some("spawn refused"),
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
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

/// `[[events.consumers]]` for one event type.
const EVENTS: &str = "[[events.consumers]]\ntype = \"book_acquired\"\nschema_versions = [1]\n";

#[test]
fn trigger_and_lookup_plugins_activate_on_demand() {
    assert!(activates_on_demand(&manifest("native", "", EVENTS)));
    assert!(activates_on_demand(&manifest("native", "\"cli\"", EVENTS)));
    assert!(activates_on_demand(&manifest("native", "\"metadata\"", "")));
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
        EVENTS
    )));
    assert!(!activates_on_demand(&manifest("native", "\"storage\"", "")));
}
