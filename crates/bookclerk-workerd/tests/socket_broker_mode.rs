//! `bookclerk-workerd` started as a plugin's socket broker
//! (`BOOKCLERK_SOCKET_BROKER=1`), driven over stdio the way the host will.
//!
//! Local WebSocket fixtures only; no public Internet and no `workerd`.

// Test code: unwrap/expect are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bookclerk_plugin_abi::{
    connect_socket_broker, Result, SocketAutoResponse, SocketClosed, SocketConnectRequest,
    SocketFrame, SocketHandler, SocketMessage, SocketMessageOk, Sockets,
};
use bookclerk_plugin_manifest::{EgressPolicy, NetworkMode};
use bookclerk_workerd::grant::GRANT_POLICY_ENV;
use bookclerk_workerd::socket_broker::SOCKET_BROKER_ENV;
use futures::{SinkExt as _, StreamExt as _};
use tokio::net::TcpListener;
use tokio::process::Command;
use tokio_tungstenite::tungstenite::Message;

/// Host handler double: records deliveries and wakes the test.
#[derive(Default)]
struct Host {
    /// Messages and closes in arrival order.
    seen: Mutex<Vec<String>>,
    /// Wakes the test.
    notify: tokio::sync::Notify,
}

#[async_trait::async_trait(?Send)]
impl SocketHandler for Host {
    async fn message(&self, event: SocketMessage) -> Result<SocketMessageOk> {
        self.seen.lock().unwrap().push(format!(
            "message {:?} {}",
            event.frame,
            event.attachment.len()
        ));
        self.notify.notify_one();
        Ok(SocketMessageOk::default())
    }

    async fn closed(&self, event: SocketClosed) -> Result<()> {
        self.seen
            .lock()
            .unwrap()
            .push(format!("closed {} {}", event.code, event.clean));
        self.notify.notify_one();
        Ok(())
    }
}

/// Waits until the host has seen `count` events.
async fn wait_for(host: &Host, count: usize) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while host.seen.lock().unwrap().len() < count {
            host.notify.notified().await;
        }
    })
    .await
    .expect("delivery in time");
}

/// A plugin root: the `sockets` fixture, with `[sockets]` declared when
/// `declare` is true.
fn plugin_root(declare: bool) -> tempfile::TempDir {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sockets");
    let root = tempfile::tempdir().expect("tmpdir");
    let mut manifest = std::fs::read_to_string(fixture.join("plugin.toml")).expect("manifest");
    if declare {
        manifest.push_str("\n[sockets]\n");
    }
    std::fs::write(root.path().join("plugin.toml"), manifest).expect("write manifest");
    std::fs::create_dir_all(root.path().join("modules")).expect("modules dir");
    std::fs::copy(
        fixture.join("modules/index.js"),
        root.path().join("modules/index.js"),
    )
    .expect("copy module");
    root
}

/// The grant the host would inject: loopback by address.
fn grant() -> String {
    serde_json::to_string(&EgressPolicy {
        mode: NetworkMode::Outbound,
        domains: vec!["127.0.0.1".into()],
        address_cidrs: vec!["127.0.0.1/32".into()],
        ..EgressPolicy::deny()
    })
    .expect("policy json")
}

/// The broker process for `root`.
fn broker_command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    command
        .env("BOOKCLERK_PLUGIN_ROOT", root)
        .env(SOCKET_BROKER_ENV, "1")
        .env(GRANT_POLICY_ENV, grant())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    command
}

#[tokio::test(flavor = "current_thread")]
async fn the_broker_holds_a_socket_for_the_host() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut socket = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");
        socket.send(Message::text("2")).await.expect("heartbeat");
        let answer = socket.next().await.expect("answer").expect("frame");
        socket.send(Message::text("hello")).await.expect("message");
        // Wait for the host's close, then answer it.
        while let Some(Ok(message)) = socket.next().await {
            if message.is_close() {
                break;
            }
        }
        while socket.next().await.is_some() {}
        answer
    });

    let root = plugin_root(true);
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = broker_command(root.path()).spawn().expect("spawn broker");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let host = Arc::new(Host::default());
            let (sockets, rpc) =
                connect_socket_broker(stdout, stdin, Arc::clone(&host) as Arc<dyn SocketHandler>);
            tokio::task::spawn_local(rpc);

            let info = tokio::time::timeout(
                Duration::from_secs(30),
                sockets.connect(SocketConnectRequest {
                    url: format!("ws://127.0.0.1:{port}/socket.io/"),
                    auto_responses: vec![SocketAutoResponse {
                        request: SocketFrame::Text("2".into()),
                        response: SocketFrame::Text("3".into()),
                    }],
                    attachment: vec![1, 2, 3],
                    ..SocketConnectRequest::default()
                }),
            )
            .await
            .expect("connect in time")
            .expect("connect");
            assert!(info.socket.starts_with('s'));

            wait_for(&host, 1).await;
            assert_eq!(host.seen.lock().unwrap()[0], "message Text(\"hello\") 3");
            assert_eq!(sockets.list().await.expect("list").len(), 1);

            sockets
                .close(info.socket, 0, String::new())
                .await
                .expect("close");
            wait_for(&host, 2).await;
            assert_eq!(host.seen.lock().unwrap()[1], "closed 1000 true");
            assert!(sockets.list().await.expect("list").is_empty());

            drop(sockets);
            let _ = child.start_kill();
        })
        .await;
    let answer = server.await.expect("server");
    assert_eq!(
        answer,
        Message::text("3"),
        "the broker answered the heartbeat"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_plugin_without_sockets_cannot_start_a_broker() {
    let root = plugin_root(false);
    let output = broker_command(root.path())
        .stderr(Stdio::piped())
        .output()
        .await
        .expect("run broker");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("declares no [sockets]"), "{stderr}");
}
