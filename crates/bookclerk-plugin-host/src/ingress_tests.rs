//! The ingress guest's lifecycle against a session double: serve, health,
//! refusal and withdrawal.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use bookclerk_plugin_abi::{NoOrigin, Origin, PluginErrorCode, SocketFrame};

use super::*;
use crate::deployment::{serve_ingress, LiveDeploymentRuntime, SpawnHealth};

/// What the session double answers, and what it was asked.
#[derive(Default)]
struct FakeSession {
    /// Fail `health` with this text.
    health_fails: Option<String>,
    /// Fail `start` with this text.
    start_fails: Option<String>,
    /// Fail `stop` with this text.
    stop_fails: Option<String>,
    /// Cleared once `stop` ran.
    running: AtomicBool,
    /// Calls in order.
    calls: Mutex<Vec<&'static str>>,
}

impl FakeSession {
    /// A running double.
    fn running() -> Self {
        Self {
            running: AtomicBool::new(true),
            ..Self::default()
        }
    }

    /// The calls so far.
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl IngressSession for FakeSession {
    async fn start(&self) -> Result<IngressStarted, String> {
        self.calls.lock().unwrap().push("start");
        match &self.start_fails {
            Some(err) => Err(err.clone()),
            None => Ok(IngressStarted {
                origin: "https://relay.example.net".into(),
            }),
        }
    }

    async fn health(&self) -> Result<IngressHealth, String> {
        self.calls.lock().unwrap().push("health");
        match &self.health_fails {
            Some(err) => Err(err.clone()),
            None => Ok(IngressHealth {
                serving: true,
                detail: None,
            }),
        }
    }

    async fn stop(&self) -> Result<(), String> {
        self.calls.lock().unwrap().push("stop");
        self.running.store(false, Ordering::SeqCst);
        match &self.stop_fails {
            Some(err) => Err(err.clone()),
            None => Ok(()),
        }
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn deliver(&self, _event: SocketEvent) -> Result<SocketMessageOk, String> {
        self.calls.lock().unwrap().push("deliver");
        Ok(SocketMessageOk::default())
    }
}

/// Daemon double: admits while `admit` is set, refuses an origin when
/// `refuse` is set, and records reports.
#[derive(Default)]
struct FakeHost {
    /// Whether the instance is admitted.
    admit: AtomicBool,
    /// Refuse the reported origin.
    refuse: bool,
    /// Reports in order.
    seen: Mutex<Vec<String>>,
}

#[async_trait]
impl IngressHost for FakeHost {
    async fn admit(&self, plugin_instance_id: &str) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("admit {plugin_instance_id}"));
        if self.admit.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("not the library's active ingress".into())
        }
    }

    fn origin(&self) -> Arc<dyn LibraryOrigin> {
        Arc::new(FakeLibrary::default())
    }

    async fn started(
        &self,
        plugin_instance_id: &str,
        started: IngressStarted,
    ) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("started {plugin_instance_id} {}", started.origin));
        if self.refuse {
            Err("the origin is not https".into())
        } else {
            Ok(())
        }
    }

    async fn stopped(&self, plugin_instance_id: &str) {
        self.seen
            .lock()
            .unwrap()
            .push(format!("stopped {plugin_instance_id}"));
    }
}

/// A runtime over an in-memory library.
async fn runtime(files: &tempfile::TempDir) -> LiveDeploymentRuntime {
    let db = bookclerk_plugin_database_sqlite::open(&files.path().join("library.db"))
        .await
        .unwrap();
    bookclerk_library::apply_host_schema(&db).await.unwrap();
    let store = bookclerk_library::LibraryStore::from_connection(db);
    LiveDeploymentRuntime::new(
        Arc::new(tokio::sync::RwLock::new(bookclerk_config::Config::default())),
        Arc::new(tokio::sync::RwLock::new(store)),
        Arc::new(tokio::sync::RwLock::new(
            bookclerk_source::SourceRegistry::new(),
        )),
        Arc::new(tokio::sync::RwLock::new(
            bookclerk_integrations::IntegrationRegistry::new(),
        )),
        Arc::new(tokio::sync::RwLock::new(
            crate::DestinationRegistry::default(),
        )),
    )
}

/// Serves `session` for instance `relay` with `host`.
async fn serve(
    runtime: &LiveDeploymentRuntime,
    host: &Arc<dyn IngressHost>,
    session: &Arc<FakeSession>,
) -> std::result::Result<(), SpawnHealth> {
    let guest = Arc::new(IngressGuest::new(
        Arc::clone(session) as Arc<dyn IngressSession>,
        "relay",
    ));
    serve_ingress(runtime, host, guest, "relay").await
}

#[tokio::test]
async fn a_served_guest_runs_until_it_is_no_longer_admitted() {
    let files = tempfile::tempdir().unwrap();
    let runtime = runtime(&files).await;
    let fake = Arc::new(FakeHost::default());
    fake.admit.store(true, Ordering::SeqCst);
    let host: Arc<dyn IngressHost> = fake.clone();
    runtime.set_ingress_host(Arc::clone(&host));
    let session = Arc::new(FakeSession::running());

    serve(&runtime, &host, &session).await.unwrap();
    assert_eq!(session.calls(), ["health", "start"]);
    assert_eq!(
        fake.seen.lock().unwrap().clone(),
        ["started relay https://relay.example.net"]
    );
    // Still admitted: it keeps running.
    assert!(!runtime.ingress_withdrawn("relay").await);
    assert!(!runtime.ingress_withdrawn("another").await);

    // Turned off: the next check stops it and tells the daemon.
    fake.admit.store(false, Ordering::SeqCst);
    assert!(runtime.ingress_withdrawn("relay").await);
    assert_eq!(session.calls(), ["health", "start", "stop"]);
    assert_eq!(
        fake.seen.lock().unwrap().last().map(String::as_str),
        Some("stopped relay")
    );
    assert!(!runtime.ingress_withdrawn("relay").await);
}

#[tokio::test]
async fn a_guest_that_fails_to_come_up_is_not_tracked() {
    let files = tempfile::tempdir().unwrap();
    let runtime = runtime(&files).await;
    let host: Arc<dyn IngressHost> = Arc::new(FakeHost::default());

    let unhealthy = Arc::new(FakeSession {
        health_fails: Some("no tunnel".into()),
        ..FakeSession::running()
    });
    match serve(&runtime, &host, &unhealthy).await {
        Err(SpawnHealth::HealthFailed { detail }) => assert_eq!(detail, "no tunnel"),
        other => panic!("{other:?}"),
    }
    assert_eq!(unhealthy.calls(), ["health"]);

    let no_start = Arc::new(FakeSession {
        start_fails: Some("provider refused".into()),
        ..FakeSession::running()
    });
    match serve(&runtime, &host, &no_start).await {
        Err(SpawnHealth::HealthFailed { detail }) => assert_eq!(detail, "provider refused"),
        other => panic!("{other:?}"),
    }
    assert!(!runtime.ingress_withdrawn("relay").await);
}

#[tokio::test]
async fn a_refused_origin_stops_the_guest() {
    let files = tempfile::tempdir().unwrap();
    let runtime = runtime(&files).await;
    let fake = Arc::new(FakeHost {
        refuse: true,
        ..FakeHost::default()
    });
    let host: Arc<dyn IngressHost> = fake.clone();
    let session = Arc::new(FakeSession {
        stop_fails: Some("already gone".into()),
        ..FakeSession::running()
    });
    match serve(&runtime, &host, &session).await {
        Err(SpawnHealth::HealthFailed { detail }) => {
            assert_eq!(detail, "the origin is not https");
        }
        other => panic!("{other:?}"),
    }
    // A failing stop is logged; the daemon still hears it stopped.
    assert_eq!(session.calls(), ["health", "start", "stop"]);
    assert_eq!(
        fake.seen.lock().unwrap().last().map(String::as_str),
        Some("stopped relay")
    );
}

#[tokio::test]
async fn a_stopped_guest_is_not_asked_again_and_events_reach_it() {
    let session = Arc::new(FakeSession::default());
    let guest = Arc::new(IngressGuest::new(
        Arc::clone(&session) as Arc<dyn IngressSession>,
        "relay",
    ));
    guest.watch_health(Duration::from_millis(5));
    guest.stop(None).await;
    assert!(!session.calls().contains(&"stop"));
    let event = SocketEvent::Closed(bookclerk_plugin_abi::SocketClosed::default());
    guest.deliver_socket_event(event).await.unwrap();
    assert_eq!(session.calls(), ["deliver"]);
}

#[tokio::test]
async fn the_health_check_runs_on_its_interval() {
    let session = Arc::new(FakeSession {
        health_fails: Some("down".into()),
        ..FakeSession::running()
    });
    let guest = Arc::new(IngressGuest::new(
        Arc::clone(&session) as Arc<dyn IngressSession>,
        "relay",
    ));
    guest.watch_health(Duration::from_millis(5));
    for _ in 0..200 {
        if session.calls().contains(&"health") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(session.calls().contains(&"health"));
    guest.stop(None).await;
}

#[test]
fn health_answers_are_logged_by_kind() {
    log_health(
        "relay",
        Ok(IngressHealth {
            serving: true,
            detail: None,
        }),
    );
    log_health(
        "relay",
        Ok(IngressHealth {
            serving: false,
            detail: Some("tunnel down".into()),
        }),
    );
    log_health("relay", Err("guest gone".into()));
}

/// The library's ends of the sockets a [`FakeLibrary`] accepted.
type LibraryEnds = (
    tokio::sync::mpsc::Receiver<crate::LocalOutbound>,
    tokio::sync::mpsc::Sender<crate::LocalInbound>,
);

/// A library double: upgrades when `accept`, otherwise refuses with `401`.
#[derive(Default)]
struct FakeLibrary {
    /// Whether upgrades are accepted.
    accept: bool,
    /// The library's ends of accepted sockets, kept open.
    kept: Mutex<Vec<LibraryEnds>>,
}

#[async_trait(?Send)]
impl Origin for FakeLibrary {
    async fn fetch(
        &self,
        request: CheckedOriginRequest,
        body: Option<Pin<Box<dyn AsyncRead + Send>>>,
    ) -> AbiResult<OriginReply> {
        NoOrigin.fetch(request, body).await
    }
}

#[async_trait(?Send)]
impl LibraryOrigin for FakeLibrary {
    async fn upgrade(&self, _request: CheckedOriginRequest) -> AbiResult<OriginUpgrade> {
        if !self.accept {
            return Ok(OriginUpgrade::Refused(OriginRefusal {
                status: 401,
                headers: Vec::new(),
                body: b"sign in".to_vec(),
            }));
        }
        let (outbound, drained) = tokio::sync::mpsc::channel(1);
        let (library, inbound) = tokio::sync::mpsc::channel(1);
        self.kept.lock().unwrap().push((drained, library));
        Ok(OriginUpgrade::Accepted(LocalSocket {
            runtime: tokio::runtime::Handle::current(),
            url: "https://library.example/api/live".into(),
            protocol: None,
            outbound,
            inbound,
        }))
    }
}

/// An upgrade request.
fn upgrade_request() -> CheckedOriginRequest {
    CheckedOriginRequest {
        method: "GET".into(),
        target: "/api/live".into(),
        headers: Vec::new(),
        client: Some("198.51.100.7".parse().unwrap()),
    }
}

#[tokio::test]
async fn an_origin_without_sockets_refuses_upgrades() {
    let origin = BrokeredOrigin::new(
        Arc::new(FakeLibrary {
            accept: true,
            ..FakeLibrary::default()
        }),
        None,
    );
    let err = origin
        .connect(upgrade_request(), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::Forbidden);
}

#[tokio::test]
async fn upgrades_become_sockets_and_refusals_pass_through() {
    let files = tempfile::tempdir().expect("files");
    let broker = crate::socket_broker::tests::idle_broker(files.path());
    let accepting = BrokeredOrigin::new(
        Arc::new(FakeLibrary {
            accept: true,
            ..FakeLibrary::default()
        }),
        Some(broker.clone()),
    );
    let connected = accepting
        .connect(upgrade_request(), b"pair".to_vec())
        .await
        .unwrap();
    let OriginConnected::Socket(info) = connected else {
        panic!("expected a socket, got {connected:?}");
    };
    assert_eq!(info.socket, "o1");
    assert_eq!(info.attachment, b"pair");
    assert_eq!(broker.open_sockets(), vec!["o1".to_string()]);
    broker
        .send("o1".into(), SocketFrame::Text("x".into()))
        .await
        .expect("the socket is open");

    let refusing = BrokeredOrigin::new(Arc::new(FakeLibrary::default()), Some(broker.clone()));
    let connected = refusing
        .connect(upgrade_request(), Vec::new())
        .await
        .unwrap();
    assert!(matches!(
        connected,
        OriginConnected::Refused(OriginRefusal { status: 401, .. })
    ));
    assert_eq!(broker.open_sockets().len(), 1, "a refusal opens nothing");
}
