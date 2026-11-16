//! Unit tests for [`super::SocketBroker`] that start no broker process.

use super::*;
use bookclerk_config::Paths;
use bookclerk_plugin_abi::PluginErrorCode;

/// A broker handle for a `[sockets]` plugin installed under `files`.
fn idle_broker(files: &std::path::Path) -> SocketBroker {
    let paths = Paths::from_files_dir(files.to_path_buf());
    let root = paths.files_dir.join("plugins").join("sockets");
    std::fs::create_dir_all(&root).expect("install dir");
    std::fs::write(root.join("guest.sh"), "#!/bin/sh\n").expect("guest");
    std::fs::write(
        root.join("plugin.toml"),
        "api_version = 3\nid = \"sockets\"\nentrypoints = [\"remoteLibrary\"]\n\
         runtime = \"native\"\ncommand = \"./guest.sh\"\n\n\
         [capabilities.network]\nmode = \"outbound\"\ntcp = [{ host = \"abs.example.com\", ports = [443] }]\n\n\
         [sockets]\n",
    )
    .expect("plugin.toml");
    let config = Config {
        paths: Some(paths),
        ..Config::default()
    };
    let plugin = crate::discover_plugins(&config)
        .expect("discover")
        .into_iter()
        .find(|found| found.manifest.id == "sockets")
        .expect("sockets plugin");
    SocketBroker::new(plugin, config, None)
}

#[tokio::test]
async fn an_idle_broker_answers_without_starting_a_process() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let err = broker
        .send("s1".into(), SocketFrame::Text("x".into()))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::NotFound);
    assert!(err.message.contains("`s1`"));
    let err = broker
        .set_attachment("s1".into(), vec![1])
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::NotFound);
    broker
        .close("s1".into(), 1000, String::new())
        .await
        .expect("closing an unknown socket is not an error");
    assert!(broker.list().await.expect("list").is_empty());
    assert!(broker.open_sockets().is_empty());
    assert!(broker.inner.link.lock().await.is_none(), "nothing started");
}

#[test]
fn the_broker_plan_is_the_front_door_alone() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    // Without a built front door the plan refuses, like any guest spawn.
    match broker_plan(&broker.inner.plugin) {
        Ok(plan) => {
            assert_eq!(plan.runtime, GuestRuntimeKind::Workerd);
            assert!(plan.native_backend.is_none());
            assert!(plan.args.is_empty());
        }
        Err(err) => assert!(err.to_string().contains("workerd"), "{err}"),
    }
}

#[tokio::test]
async fn a_dropped_sink_is_not_kept_alive() {
    /// Sink double that counts deliveries.
    struct Counter(std::sync::atomic::AtomicUsize);
    #[async_trait]
    impl SocketSink for Counter {
        async fn deliver_socket_event(
            &self,
            _event: SocketEvent,
        ) -> std::result::Result<SocketMessageOk, String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(SocketMessageOk::default())
        }
    }
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let sink: Arc<dyn SocketSink> = Arc::new(Counter(std::sync::atomic::AtomicUsize::new(0)));
    broker.set_sink(Arc::downgrade(&sink));
    assert!(broker.inner.sink().is_some());
    drop(sink);
    assert!(broker.inner.sink().is_none());
    // Delivering with a retired sink is a quiet no-op.
    deliver(
        &Arc::downgrade(&broker.inner),
        SocketEvent::Closed(SocketClosed {
            socket: "s1".into(),
            code: CLOSE_ABNORMAL,
            reason: None,
            clean: false,
            attachment: Vec::new(),
        }),
    )
    .await;
}

#[test]
fn sockets_is_handed_out_only_when_declared_and_granted() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let manifest = broker.inner.plugin.manifest.clone();
    let mut grant = crate::consent_request(&manifest, broker.inner.plugin.plugin_key());
    assert!(granted(Some(broker.clone()), &manifest, &grant).is_some());
    assert!(granted(None, &manifest, &grant).is_none());
    let mut undeclared = manifest.clone();
    undeclared.sockets = None;
    assert!(granted(Some(broker.clone()), &undeclared, &grant).is_none());
    grant.bindings.retain(|binding| binding != "sockets");
    assert!(granted(Some(broker), &manifest, &grant).is_none());
}

/// Sink double that records each event as text and can refuse them.
#[derive(Default)]
struct Recorder {
    /// Events in arrival order.
    seen: Mutex<Vec<String>>,
    /// Refuse every event after recording it.
    refuse: bool,
}

#[async_trait]
impl SocketSink for Recorder {
    async fn deliver_socket_event(
        &self,
        event: SocketEvent,
    ) -> std::result::Result<SocketMessageOk, String> {
        let line = match event {
            SocketEvent::Message(message) => format!("message {}", message.socket),
            SocketEvent::Closed(closed) => format!(
                "closed {} {} {} {}",
                closed.socket,
                closed.code,
                closed.clean,
                String::from_utf8_lossy(&closed.attachment)
            ),
        };
        self.seen.lock().expect("seen").push(line);
        if self.refuse {
            Err("the guest refused".into())
        } else {
            Ok(SocketMessageOk::default())
        }
    }
}

#[test]
fn a_lost_broker_reports_every_open_socket_as_abnormally_closed() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let recorder = Arc::new(Recorder::default());
    let sink: Arc<dyn SocketSink> = recorder.clone();
    broker.set_sink(Arc::downgrade(&sink));
    let info = |socket: &str, attachment: &[u8]| SocketInfo {
        socket: socket.into(),
        attachment: attachment.to_vec(),
        ..SocketInfo::default()
    };
    broker.inner.record_open(1, &info("s1", b"cursor-1"));
    broker.inner.record_open(1, &info("s2", b""));
    // A replacement broker's socket is not the lost broker's to report.
    broker.inner.record_open(2, &info("s3", b"other"));

    report_lost(&Arc::downgrade(&broker.inner), 1);

    assert_eq!(
        *recorder.seen.lock().expect("seen"),
        vec![
            "closed g1_s1 1006 false cursor-1".to_string(),
            "closed g1_s2 1006 false ".to_string(),
        ]
    );
    assert_eq!(
        broker.open_sockets(),
        vec!["g2_s3".to_string()],
        "only the lost broker's sockets are cleared"
    );
    // Nothing left to report: a second loss delivers nothing.
    report_lost(&Arc::downgrade(&broker.inner), 1);
    assert_eq!(recorder.seen.lock().expect("seen").len(), 2);
}

#[test]
fn a_lost_broker_with_no_handle_left_reports_nothing() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let weak = Arc::downgrade(&broker.inner);
    drop(broker);
    report_lost(&weak, 1);
}

#[test]
fn a_refused_loss_report_is_logged_not_retried() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let recorder = Arc::new(Recorder {
        refuse: true,
        ..Recorder::default()
    });
    let sink: Arc<dyn SocketSink> = recorder.clone();
    broker.set_sink(Arc::downgrade(&sink));
    broker.inner.record_open(
        1,
        &SocketInfo {
            socket: "s1".into(),
            ..SocketInfo::default()
        },
    );
    report_lost(&Arc::downgrade(&broker.inner), 1);
    assert_eq!(recorder.seen.lock().expect("seen").len(), 1);
    assert!(broker.open_sockets().is_empty());
}

#[tokio::test]
async fn the_sockets_binding_forwards_to_the_broker() {
    let files = tempfile::tempdir().expect("files");
    let sockets = BrokerSockets::new(idle_broker(files.path()));
    let err = sockets
        .send("s1".into(), SocketFrame::Binary(vec![1]))
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::NotFound);
    let err = sockets
        .set_attachment("s1".into(), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(err.code, PluginErrorCode::NotFound);
    sockets
        .close("s1".into(), 1000, "done".into())
        .await
        .expect("closing an unknown socket is not an error");
    assert!(sockets.list().await.expect("list").is_empty());
    assert!(sockets.broker.open_sockets().is_empty());
}

#[test]
fn a_close_that_beats_its_connect_answer_does_not_leave_the_socket_open() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let info = |socket: &str| SocketInfo {
        socket: socket.into(),
        attachment: b"a".to_vec(),
        ..SocketInfo::default()
    };
    // In order: open, then closed.
    broker.inner.record_open(1, &info("s1"));
    assert_eq!(broker.open_sockets(), vec!["g1_s1".to_string()]);
    assert!(broker.inner.holds(1));
    assert!(broker.inner.record_closed(1, "s1"), "nothing left open");
    // Out of order: the close lands before the connect answer.
    assert!(broker.inner.record_closed(1, "s2"));
    broker.inner.record_open(1, &info("s2"));
    assert!(
        broker.open_sockets().is_empty(),
        "the late answer is dropped"
    );
    // The early close was used up; a later socket with that id is kept.
    broker.inner.record_open(1, &info("s2"));
    assert_eq!(broker.open_sockets(), vec!["g1_s2".to_string()]);
    // A newer broker reusing an id is not closed by the older one.
    broker.inner.record_open(2, &info("s2"));
    broker.inner.record_closed(1, "s2");
    assert!(broker.inner.holds(2), "the newer broker keeps its socket");
    broker.inner.record_closed(2, "s2");
    // A broker that ends forgets its early closes.
    assert!(broker.inner.record_closed(3, "s4"));
    assert!(broker.inner.take_generation(3).is_empty());
    broker.inner.record_open(3, &info("s4"));
    assert!(broker.inner.holds(3));
}

#[tokio::test]
async fn an_event_waits_for_the_sink_the_deployment_installs() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let recorder = Arc::new(Recorder::default());
    let sink: Arc<dyn SocketSink> = recorder.clone();
    let inner = Arc::downgrade(&broker.inner);
    let delivery = tokio::spawn(async move {
        deliver(
            &inner,
            SocketEvent::Message(SocketMessage {
                socket: "s1".into(),
                ..SocketMessage::default()
            }),
        )
        .await;
    });
    tokio::task::yield_now().await;
    assert!(recorder.seen.lock().expect("seen").is_empty());
    broker.set_sink(Arc::downgrade(&sink));
    delivery.await.expect("delivery");
    assert_eq!(*recorder.seen.lock().expect("seen"), vec!["message s1"]);
}

#[tokio::test]
async fn an_event_waiting_for_a_sink_ends_when_the_broker_is_dropped() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let inner = Arc::downgrade(&broker.inner);
    let delivery = tokio::spawn(async move {
        deliver(
            &inner,
            SocketEvent::Message(SocketMessage {
                socket: "s1".into(),
                ..SocketMessage::default()
            }),
        )
        .await;
    });
    tokio::task::yield_now().await;
    assert!(!delivery.is_finished(), "it waits while the broker lives");
    // A failed deployment start drops every handle; the wait ends.
    drop(broker);
    tokio::time::timeout(std::time::Duration::from_secs(5), delivery)
        .await
        .expect("the wait ended")
        .expect("delivery");
}

#[test]
fn colliding_ids_from_two_brokers_are_tracked_apart() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let recorder = Arc::new(Recorder::default());
    let sink: Arc<dyn SocketSink> = recorder.clone();
    broker.set_sink(Arc::downgrade(&sink));
    let info = |attachment: &[u8]| SocketInfo {
        socket: "same".into(),
        attachment: attachment.to_vec(),
        ..SocketInfo::default()
    };
    // The dying broker's socket and its replacement's share an id.
    broker.inner.record_open(1, &info(b"old"));
    broker.inner.record_open(2, &info(b"new"));
    broker.inner.record_attachment(2, "same", b"new-2");

    report_lost(&Arc::downgrade(&broker.inner), 1);

    assert_eq!(
        *recorder.seen.lock().expect("seen"),
        vec!["closed g1_same 1006 false old".to_string()]
    );
    assert!(broker.inner.holds(2), "the replacement's socket is kept");
    // The guest sees two distinct ids for the two sockets.
    assert_eq!(broker.open_sockets(), vec!["g2_same".to_string()]);
    assert_eq!(
        broker_socket("g1_same"),
        Some((1, "same")),
        "a command for the lost socket names the lost broker, so the replacement refuses it"
    );
    assert_eq!(
        broker.inner.take_generation(2),
        vec![("same".to_string(), b"new-2".to_vec())]
    );
}

#[tokio::test(start_paused = true)]
async fn an_event_still_reaches_a_sink_installed_long_after_it_arrived() {
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let recorder = Arc::new(Recorder::default());
    let sink: Arc<dyn SocketSink> = recorder.clone();
    let inner = Arc::downgrade(&broker.inner);
    let delivery = tokio::spawn(async move {
        deliver(
            &inner,
            SocketEvent::Message(SocketMessage {
                socket: "s1".into(),
                ..SocketMessage::default()
            }),
        )
        .await;
    });
    // A slow start: far longer than any old time limit.
    tokio::time::sleep(std::time::Duration::from_secs(600)).await;
    assert!(!delivery.is_finished(), "the event is still waiting");
    broker.set_sink(Arc::downgrade(&sink));
    delivery.await.expect("delivery");
    assert_eq!(*recorder.seen.lock().expect("seen"), vec!["message s1"]);
}

#[test]
fn guest_socket_ids_round_trip_and_reject_foreign_ids() {
    let id = guest_socket(7, "s0000abcd-3");
    assert_eq!(id, "g7_s0000abcd-3");
    assert_eq!(broker_socket(&id), Some((7, "s0000abcd-3")));
    bookclerk_plugin_abi::sockets::check_socket_id(&guest_socket(u64::MAX, &"x".repeat(30)))
        .expect("the longest guest id is still a valid socket id");
    for foreign in ["s1", "g_s1", "gx_s1", "g1_", "g1"] {
        assert_eq!(broker_socket(foreign), None, "{foreign}");
    }
}
