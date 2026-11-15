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
        ) -> std::result::Result<(), String> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }
    let files = tempfile::tempdir().expect("files");
    let broker = idle_broker(files.path());
    let sink: Arc<dyn SocketSink> = Arc::new(Counter(std::sync::atomic::AtomicUsize::new(0)));
    broker.set_sink(Arc::downgrade(&sink));
    assert!(broker.inner.sink().is_some());
    drop(sink);
    assert!(broker.inner.sink().is_none());
    // Delivering with no sink is a quiet no-op.
    deliver(
        &broker.inner,
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
