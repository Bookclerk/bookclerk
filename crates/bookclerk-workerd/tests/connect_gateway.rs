//! Workerd `fetch()` / `connect()` through Bookclerk's egress worker.
//!
//! Local TCP/HTTP fixtures only — no public Internet.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bookclerk_plugin_abi::{
    connect_plugin, Destination, DestinationClient, HostBindings, Invocation, PluginClient,
    PRODUCT_API_VERSION,
};
use bookclerk_plugin_manifest::{EgressPolicy, NetworkMode, TcpGrant};
use bookclerk_workerd::grant::GRANT_POLICY_ENV;
use bookclerk_workerd::pin::binary_name;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

fn find_workerd() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("BOOKCLERK_WORKERD_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    let launcher = PathBuf::from(env!("CARGO_BIN_EXE_bookclerk-workerd"));
    launcher
        .parent()
        .map(|dir| dir.join(binary_name()))
        .filter(|p| p.is_file())
}

async fn open_storage(client: &PluginClient) -> DestinationClient {
    client
        .open(
            &Invocation {
                id: "sockets".into(),
                ..Default::default()
            },
            HostBindings::default(),
        )
        .await
        .expect("open")
        .storage
        .expect("guest exports `storage`")
}

async fn get_text(dest: &DestinationClient, key: &str) -> Result<String, String> {
    let got = dest.get(key, None).await.map_err(|err| err.to_string())?;
    let mut buf = Vec::new();
    let mut body = got.body;
    body.read_to_end(&mut buf)
        .await
        .map_err(|err| err.to_string())?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    // Stream `/destination/get` returns plugin throws as HTTP 200 JSON-RPC.
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(|c| c.as_str()).unwrap_or("error");
            let message = err.get("message").and_then(|m| m.as_str()).unwrap_or(&text);
            return Err(format!("{code}: {message}"));
        }
    }
    Ok(text)
}

fn assert_denied(err: &str, needles: &[&str]) {
    let lower = err.to_ascii_lowercase();
    assert!(
        needles
            .iter()
            .any(|n| lower.contains(&n.to_ascii_lowercase())),
        "denied error {err:?} did not mention {needles:?}"
    );
}

async fn echo_once(listener: TcpListener) {
    let (mut s, _) = listener.accept().await.expect("echo accept");
    let mut buf = [0_u8; 64];
    let n = s.read(&mut buf).await.expect("echo read");
    s.write_all(&buf[..n]).await.expect("echo write");
}

async fn http_once(listener: TcpListener, response: &[u8]) {
    let (mut s, _) = listener.accept().await.expect("http accept");
    let mut buf = vec![0_u8; 4096];
    let _ = s.read(&mut buf).await;
    s.write_all(response).await.expect("http write");
}

fn policy(echo_port: u16, allow_public_redirects: bool) -> EgressPolicy {
    EgressPolicy {
        mode: NetworkMode::Outbound,
        domains: vec!["127.0.0.1".into()],
        max_redirects: 10,
        subrequests: Some(50),
        allow_undeclared_public_redirects: allow_public_redirects,
        tcp: vec![TcpGrant {
            host: "127.0.0.1".into(),
            ports: vec![echo_port],
        }],
        address_cidrs: vec!["127.0.0.1/32".into()],
    }
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_connect_and_fetch_share_policy() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
        );
    };
    let echo = TcpListener::bind("127.0.0.1:0").await.expect("echo bind");
    let echo_port = echo.local_addr().expect("echo addr").port();
    tokio::spawn(echo_once(echo));

    let http = TcpListener::bind("127.0.0.1:0").await.expect("http bind");
    let http_port = http.local_addr().expect("http addr").port();
    let http_body = b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\nping";
    tokio::spawn(async move { http_once(http, http_body).await });

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sockets");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let grant = serde_json::to_string(&policy(echo_port, false)).expect("policy json");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &fixture)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("TMPDIR", tmp.path())
                .env(GRANT_POLICY_ENV, &grant)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn bookclerk-workerd");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            let desc = tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out")
                .expect("describe");
            assert_eq!(desc.api_version, PRODUCT_API_VERSION);
            let dest = open_storage(&client).await;

            let echoed = get_text(&dest, &format!("tcp:127.0.0.1:{echo_port}"))
                .await
                .expect("approved tcp");
            assert!(echoed.contains("ping"), "{echoed}");

            let denied = get_text(&dest, "tcp:127.0.0.1:9")
                .await
                .expect_err("wrong port");
            // Policy throw after CONNECT accept() often surfaces as disconnect.
            assert_denied(&denied, &["tcp", "failed", "network connection lost"]);

            let fetched = get_text(&dest, &format!("fetch:http://127.0.0.1:{http_port}/"))
                .await
                .expect("approved fetch");
            assert!(fetched.starts_with("200\n"), "{fetched}");
            assert!(fetched.contains("ping"), "{fetched}");

            let private = get_text(&dest, "fetch:http://10.0.0.1/")
                .await
                .expect_err("private fetch");
            assert_denied(&private, &["10.0.0.1", "failed", "permitted"]);

            let _ = child.kill().await;
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn workerd_redirect_to_private_denied_even_with_public_redirect_flag() {
    let Some(workerd) = find_workerd() else {
        panic!(
            "pinned workerd binary missing; run `cargo ensure-workerd`. Do not set BOOKCLERK_SKIP_WORKERD."
        );
    };
    let http = TcpListener::bind("127.0.0.1:0").await.expect("http bind");
    let http_port = http.local_addr().expect("http addr").port();
    let location = b"HTTP/1.1 302 Found\r\nlocation: http://10.0.0.1/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
    tokio::spawn(async move { http_once(http, location).await });

    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sockets");
    let tmp = tempfile::tempdir().expect("tmpdir");
    let grant = serde_json::to_string(&policy(1, true)).expect("policy json");
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let mut child = Command::new(env!("CARGO_BIN_EXE_bookclerk-workerd"))
                .env("BOOKCLERK_PLUGIN_ROOT", &fixture)
                .env("BOOKCLERK_WORKERD_BIN", &workerd)
                .env("TMPDIR", tmp.path())
                .env(GRANT_POLICY_ENV, &grant)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true)
                .spawn()
                .expect("spawn bookclerk-workerd");
            let stdin = child.stdin.take().expect("stdin");
            let stdout = child.stdout.take().expect("stdout");
            let (client, rpc) = connect_plugin(stdout, stdin, 64 * 1024);
            tokio::task::spawn_local(rpc);
            tokio::time::timeout(Duration::from_secs(90), client.describe())
                .await
                .expect("describe timed out")
                .expect("describe");
            let dest = open_storage(&client).await;
            let err = get_text(&dest, &format!("fetch:http://127.0.0.1:{http_port}/"))
                .await
                .expect_err("redirect to RFC1918");
            assert_denied(&err, &["10.0.0.1", "permitted", "failed"]);
            let _ = child.kill().await;
        })
        .await;
}
