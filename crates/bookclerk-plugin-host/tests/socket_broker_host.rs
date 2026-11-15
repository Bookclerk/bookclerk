//! The host's socket broker: `bookclerk-workerd` started in broker mode
//! through the ordinary spawn path, holding a WebSocket for a plugin and
//! handing what arrives to the instance's sink.
//!
//! Needs `bookclerk-workerd`, `bookclerk-jail` and the pinned `workerd`
//! beside the test binary (`cargo test --workspace` builds the first two).
//! Local WebSocket server only; no public Internet.

#![cfg(target_os = "linux")]
// Test code: unwrap/expect are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bookclerk_config::{Config, Paths};
use bookclerk_plugin_abi::{SocketConnectRequest, SocketFrame};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, DiscoveredPlugin, PluginGrantStore, SocketBroker,
    SocketEvent, SocketSink,
};
use futures::{SinkExt as _, StreamExt as _};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// Sink double: records events and wakes the test.
#[derive(Default)]
struct Recorder {
    /// Events in arrival order, as text.
    seen: Mutex<Vec<String>>,
    /// Wakes the test.
    notify: tokio::sync::Notify,
}

#[async_trait]
impl SocketSink for Recorder {
    async fn deliver_socket_event(&self, event: SocketEvent) -> Result<(), String> {
        let line = match event {
            SocketEvent::Message(message) => format!(
                "message {:?} {}",
                message.frame,
                String::from_utf8_lossy(&message.attachment)
            ),
            SocketEvent::Closed(closed) => format!(
                "closed {} {}",
                closed.code,
                String::from_utf8_lossy(&closed.attachment)
            ),
        };
        self.seen.lock().unwrap().push(line);
        self.notify.notify_one();
        Ok(())
    }
}

impl Recorder {
    /// Waits until `count` events arrived.
    async fn wait_for(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(60), async {
            while self.seen.lock().unwrap().len() < count {
                self.notify.notified().await;
            }
        })
        .await
        .expect("events in time");
    }
}

/// Files dir with a `sockets` plugin installed and granted, allowed to
/// reach `127.0.0.1:port`; `grant_sockets` false withholds the `sockets`
/// grant token.
fn install(files: &Path, port: u16, grant_sockets: bool) -> (Config, DiscoveredPlugin) {
    let paths = Paths::from_files_dir(files.to_path_buf());
    let root = paths.files_dir.join("plugins").join("sockets");
    std::fs::create_dir_all(&root).expect("install dir");
    let guest = root.join("guest.sh");
    std::fs::write(&guest, "#!/bin/sh\ncat >/dev/null\n").expect("guest");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&guest, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let manifest = format!(
        "api_version = 3\nid = \"sockets\"\nversion = \"1.0.0\"\n\
         entrypoints = [\"remoteLibrary\"]\nruntime = \"native\"\ncommand = \"./guest.sh\"\n\n\
         [capabilities.network]\nmode = \"outbound\"\n\
         tcp = [{{ host = \"127.0.0.1\", ports = [{port}] }}]\n\
         address_cidrs = [\"127.0.0.1/32\"]\n\n[sockets]\n"
    );
    std::fs::write(root.join("plugin.toml"), manifest).expect("plugin.toml");
    let config = Config {
        paths: Some(paths),
        ..Config::default()
    };
    let plugin = discover_plugins(&config)
        .expect("discover")
        .into_iter()
        .find(|found| found.manifest.id == "sockets")
        .expect("sockets plugin");
    let mut grant = consent_request(&plugin.manifest, plugin.plugin_key());
    if !grant_sockets {
        grant.bindings.retain(|binding| binding != "sockets");
    }
    let mut grants = PluginGrantStore::default();
    grants.upsert(grant);
    grants
        .save(&config.paths().files_dir)
        .expect("write plugin-grants.json");
    (config, plugin)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_broker_holds_a_socket_and_stops_when_it_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");
        let sent = socket.next().await.expect("guest frame").expect("frame");
        socket.send(Message::text("hello")).await.expect("message");
        socket.close(None).await.expect("close");
        while socket.next().await.is_some() {}
        sent
    });

    let files = tempfile::tempdir().expect("files");
    let (config, plugin) = install(files.path(), port, true);
    let broker = SocketBroker::new(plugin, config, None);
    let recorder = Arc::new(Recorder::default());
    let sink: Arc<dyn SocketSink> = recorder.clone();
    broker.set_sink(Arc::downgrade(&sink));

    let info = broker
        .connect(SocketConnectRequest {
            url: format!("ws://127.0.0.1:{port}/socket.io/"),
            attachment: b"cursor-1".to_vec(),
            ..SocketConnectRequest::default()
        })
        .await
        .unwrap_or_else(|err| panic!("connect through the broker: {err}"));
    assert_eq!(broker.open_sockets(), vec![info.socket.clone()]);
    assert_eq!(broker.list().await.expect("list").len(), 1);
    broker
        .set_attachment(info.socket.clone(), b"cursor-2".to_vec())
        .await
        .expect("attachment");
    broker
        .send(info.socket.clone(), SocketFrame::Text("40".into()))
        .await
        .expect("send");

    recorder.wait_for(2).await;
    let seen = recorder.seen.lock().unwrap().clone();
    assert_eq!(seen[0], "message Text(\"hello\") cursor-2");
    // A close frame with no code is reported as 1005.
    assert_eq!(seen[1], "closed 1005 cursor-2");
    assert!(broker.open_sockets().is_empty());
    assert_eq!(
        server.await.expect("server"),
        Message::text("40"),
        "the guest's frame reached the server"
    );

    // With no socket open the broker process stops; the next connect starts
    // a fresh one, which a refused target does not keep open.
    let refused = broker
        .connect(SocketConnectRequest {
            url: format!("ws://127.0.0.1:{}/", port.wrapping_add(1).max(1)),
            ..SocketConnectRequest::default()
        })
        .await;
    assert!(refused.is_err(), "an ungranted port is refused");
    assert!(broker.open_sockets().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_plugin_without_sockets_gets_no_broker() {
    let files = tempfile::tempdir().expect("files");
    let (config, mut plugin) = install(files.path(), 1, true);
    plugin.manifest.sockets = None;
    let broker = SocketBroker::new(plugin, config, None);
    let err = broker
        .connect(SocketConnectRequest {
            url: "ws://127.0.0.1:1/".into(),
            ..SocketConnectRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Forbidden);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_grant_without_the_sockets_token_starts_no_broker() {
    let files = tempfile::tempdir().expect("files");
    let (config, plugin) = install(files.path(), 1, false);
    let broker = SocketBroker::new(plugin, config, None);
    let err = broker
        .connect(SocketConnectRequest {
            url: "ws://127.0.0.1:1/".into(),
            ..SocketConnectRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(err.code, bookclerk_plugin_abi::PluginErrorCode::Forbidden);
    assert!(err.message.contains("sockets"), "{}", err.message);
}
