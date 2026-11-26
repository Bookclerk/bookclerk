//! A jailed workerd isolate reaches the network only through the host.
//!
//! The isolate's front door (`bookclerk-workerd`) and the pinned `workerd`
//! run in a `LocalIpc` jail: no IP socket of their own. The isolate's
//! `fetch()` and `connect()` leave as CONNECT tunnels over the host link,
//! and the host checks each one against the grant before it dials. This test
//! spawns an isolate the way the daemon does and checks both halves:
//!
//! - granted destinations work (an HTTP fetch and a raw TCP socket), and an
//!   ungranted port is refused for either;
//! - the SDK's `connect()` reaches a server-first protocol on a framed
//!   tunnel and `startTls()` makes the launcher's relay start a TLS handshake
//!   with the server, whose self-signed certificate it then refuses (the
//!   relay trusts Mozilla's roots only);
//! - on Linux, neither process holds an IP socket and both run under the
//!   seccomp filter that refuses to create one.

// Test code: unwrap/expect and plain casts are fine here.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "native_gateway/harness.rs"]
mod ng_harness;

use std::time::Duration;

use bookclerk_config::{Config, Isolation, Paths};
use bookclerk_plugin_host::{
    consent_request, discover_plugins, DiscoveredPlugin, PluginGrantStore, PluginSession,
    SessionServices, HOST_SHARED_ACCOUNT,
};
use bookclerk_plugin_sdk::{CliArg, CliInvokeParams};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const PLUGIN_ID: &str = "isolate_egress_probe";

/// The isolate: `fetch <url>` and `tcp <port>`, each answering one line.
const MODULE: &str = r#"
import { BookclerkEntrypoint, CliEntrypoint, cliArgs, connect as sdkConnect, jsonPayload } from "@bookclerk/plugin-sdk/workerd";
import { connect } from "cloudflare:sockets";

const CLI = { commands: [
  { name: "fetch", about: "GET a URL", args: [{ name: "url", long: "url", kind: "string", required: true, positional: false }] },
  { name: "tcp", about: "echo over a socket", args: [{ name: "port", long: "port", kind: "string", required: true, positional: false }] },
  { name: "starttls", about: "upgrade a socket", args: [{ name: "port", long: "port", kind: "string", required: true, positional: false }] },
] };

async function line(reader) {
  let out = "";
  while (!out.endsWith("\n")) {
    const { value, done } = await reader.read();
    if (done) throw new Error(`closed after ${JSON.stringify(out)}`);
    out += new TextDecoder().decode(value);
  }
  return out;
}

async function starttls(port) {
  const plain = sdkConnect({ hostname: "127.0.0.1", port }, { secureTransport: "starttls" });
  const reader = plain.readable.getReader();
  const writer = plain.writable.getWriter();
  const greeting = await line(reader);
  await writer.write(new TextEncoder().encode("STARTTLS\r\n"));
  const go = await line(reader);
  reader.releaseLock();
  writer.releaseLock();
  const secure = plain.startTls();
  await secure.opened;
  return `${greeting}${go}secure`;
}

async function run(params) {
  const args = cliArgs(params);
  if (params.command === "starttls") return starttls(Number(args.port));
  if (params.command === "fetch") {
    const res = await fetch(args.url);
    const refusal = res.headers.get("x-bookclerk-error");
    return `${res.status} ${refusal ? refusal + " " : ""}${await res.text()}`;
  }
  const socket = connect({ hostname: "127.0.0.1", port: Number(args.port) }, { secureTransport: "off" });
  await socket.opened;
  const writer = socket.writable.getWriter();
  await writer.write(new TextEncoder().encode("ping"));
  const { value } = await socket.readable.getReader().read();
  await socket.close();
  return new TextDecoder().decode(value);
}

export class Cli extends CliEntrypoint {
  async describe() { return CLI; }
  async invoke(params) {
    try {
      const out = await run(params);
      return { exitCode: 0, stdout: out, stderr: "", payload: jsonPayload(null) };
    } catch (err) {
      return { exitCode: 1, stdout: "", stderr: String(err && err.message ? err.message : err), payload: jsonPayload(null) };
    }
  }
}

export default class Probe extends BookclerkEntrypoint {
  async describe() { return { displayName: "Isolate egress probe" }; }
}
"#;

/// The probe's `plugin.toml`: fetch to loopback on `http_port` and a TCP
/// grant for `tcp_port` and `tls_port`.
fn manifest(http_port: u16, tcp_port: u16, tls_port: u16) -> String {
    format!(
        "api_version = 3\n\
         id = \"{PLUGIN_ID}\"\n\
         name = \"Isolate egress probe\"\n\
         version = \"0.0.0\"\n\
         runtime = \"workerd\"\n\
         entrypoints = [\"cli\"]\n\
         \n\
         [workerd]\n\
         compatibility_date = \"2026-08-01\"\n\
         main_module = \"index.js\"\n\
         modules_dir = \"modules\"\n\
         \n\
         [[modules]]\n\
         name = \"index.js\"\n\
         path = \"index.js\"\n\
         type = \"js\"\n\
         \n\
         [capabilities.network]\n\
         mode = \"outbound\"\n\
         domains = [\"127.0.0.1:{http_port}\"]\n\
         tcp = [{{ host = \"127.0.0.1\", ports = [{tcp_port}, {tls_port}] }}]\n\
         address_cidrs = [\"127.0.0.1/32\"]\n"
    )
}

/// One install with the probe isolate and its grant approved.
struct Install {
    _files: tempfile::TempDir,
    config: Config,
}

impl Install {
    fn new(http_port: u16, tcp_port: u16, tls_port: u16) -> Self {
        let files = tempfile::tempdir().expect("tempdir");
        let paths = Paths::from_files_dir(files.path().to_path_buf());
        let root = paths.files_dir.join("plugins").join(PLUGIN_ID);
        std::fs::create_dir_all(root.join("modules")).expect("install dir");
        std::fs::write(root.join("modules/index.js"), MODULE).expect("module");
        std::fs::write(
            root.join("plugin.toml"),
            manifest(http_port, tcp_port, tls_port),
        )
        .expect("manifest");
        let mut config = Config {
            paths: Some(paths),
            ..Default::default()
        };
        config.plugins.isolation = Isolation::Required;
        let plugin = find(&config);
        let mut grants = PluginGrantStore::default();
        grants.upsert(consent_request(&plugin.manifest, plugin.plugin_key()));
        grants
            .save(&config.paths().files_dir)
            .expect("write plugin-grants.json");
        Self {
            _files: files,
            config,
        }
    }
}

fn find(config: &Config) -> DiscoveredPlugin {
    discover_plugins(config)
        .expect("discover")
        .into_iter()
        .find(|found| found.manifest.id == PLUGIN_ID)
        .expect("probe plugin discovered")
}

async fn invoke(session: &PluginSession, command: &str, name: &str, value: &str) -> (i32, String) {
    let params = CliInvokeParams {
        command: command.into(),
        args: vec![CliArg {
            name: name.into(),
            value: value.into(),
        }],
    };
    let result = tokio::time::timeout(ng_harness::RPC_TIMEOUT, session.cli_invoke(params))
        .await
        .unwrap_or_else(|_| ng_harness::fail_deadline(&format!("{command} timed out")))
        .unwrap_or_else(|err| panic!("{command} RPC failed: {err}"));
    let text = if result.exit_code == 0 {
        result.stdout
    } else {
        result.stderr
    };
    (result.exit_code, text)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jailed_isolate_reaches_granted_hosts_only_through_the_host() {
    let http = TcpListener::bind("127.0.0.1:0").await.expect("http bind");
    let http_port = http.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = http.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = vec![0_u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 4\r\nconnection: close\r\n\r\npong",
                    )
                    .await;
            });
        }
    });
    let echo = TcpListener::bind("127.0.0.1:0").await.expect("echo bind");
    let echo_port = echo.local_addr().expect("addr").port();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = echo.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0_u8; 64];
                if let Ok(n) = stream.read(&mut buf).await {
                    let _ = stream.write_all(&buf[..n]).await;
                }
            });
        }
    });

    // A server-first protocol that upgrades: greets, answers STARTTLS, then
    // reports the first byte of the client's next flight (0x16 is a TLS
    // handshake record, the relay's ClientHello).
    let tls = TcpListener::bind("127.0.0.1:0").await.expect("tls bind");
    let tls_port = tls.local_addr().expect("addr").port();
    let (hello_tx, hello_rx) = tokio::sync::oneshot::channel::<u8>();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = tls.accept().await else {
            return;
        };
        let _ = stream.write_all(b"220 probe ready\r\n").await;
        let mut line = [0_u8; 10];
        if stream.read_exact(&mut line).await.is_err() || &line != b"STARTTLS\r\n" {
            return;
        }
        let _ = stream.write_all(b"220 go ahead\r\n").await;
        let mut first = [0_u8; 1];
        if stream.read_exact(&mut first).await.is_ok() {
            let _ = hello_tx.send(first[0]);
        }
    });

    let install = Install::new(http_port, echo_port, tls_port);
    let plugin = find(&install.config);
    let session = tokio::time::timeout(
        ng_harness::SPAWN_TIMEOUT,
        PluginSession::spawn_with(
            &plugin,
            &install.config,
            serde_json::json!({}),
            HOST_SHARED_ACCOUNT,
            &[],
            SessionServices::default(),
        ),
    )
    .await
    .unwrap_or_else(|_| ng_harness::fail_deadline("spawn timed out"))
    .unwrap_or_else(|err| panic!("spawn through the workerd front door failed: {err}"));

    let (code, text) = invoke(
        &session,
        "fetch",
        "url",
        &format!("http://127.0.0.1:{http_port}/"),
    )
    .await;
    assert_eq!((code, text.as_str()), (0, "200 pong"), "granted fetch");

    let (code, text) = invoke(&session, "tcp", "port", &echo_port.to_string()).await;
    assert_eq!((code, text.as_str()), (0, "ping"), "granted socket");

    // STARTTLS through the SDK: the plain exchange works, then the relay
    // starts TLS with the server and refuses its certificate.
    let (code, text) = invoke(&session, "starttls", "port", &tls_port.to_string()).await;
    assert_ne!(code, 0, "a self-signed server must not verify: {text}");
    assert!(text.contains("TLS handshake failed"), "{text}");
    let first = tokio::time::timeout(Duration::from_secs(10), hello_rx)
        .await
        .expect("the relay started a handshake")
        .expect("server saw the client's flight");
    assert_eq!(
        first, 0x16,
        "the relay's first upgraded byte is a TLS handshake record"
    );

    // A port the grant does not name: the host refuses the tunnel.
    let refused = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("refused bind");
    let refused_port = refused.local_addr().expect("addr").port();
    let (code, text) = invoke(&session, "tcp", "port", &refused_port.to_string()).await;
    assert_ne!(code, 0, "ungranted port must fail: {text}");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), refused.accept())
            .await
            .is_err(),
        "the host must not dial an ungranted port"
    );

    // A fetch to a port the `domains` entry does not grant is refused too.
    let (code, text) = invoke(
        &session,
        "fetch",
        "url",
        &format!("http://127.0.0.1:{refused_port}/"),
    )
    .await;
    assert_eq!(code, 0, "{text}");
    assert!(
        text.starts_with("403 network_not_granted ")
            && text.contains(&format!("\"destination\":\"127.0.0.1:{refused_port}\"")),
        "an ungranted fetch port gets the typed refusal: {text}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(300), refused.accept())
            .await
            .is_err(),
        "the host must not dial an ungranted fetch port"
    );

    #[cfg(target_os = "linux")]
    assert_no_ip_sockets(&session);
}

/// The front door and its pinned `workerd` hold no IP socket and cannot
/// create one.
#[cfg(target_os = "linux")]
fn assert_no_ip_sockets(session: &PluginSession) {
    let launcher = session.guest_pid().expect("launcher pid");
    let tree = ng_harness::ProcessTree::capture();
    let workerd = tree
        .pinned_workerd(launcher)
        .unwrap_or_else(|| panic!("no workerd under {}", tree.describe(launcher)));
    let enforced = std::env::var("BOOKCLERK_SANDBOX_REQUIRE_ENFORCEMENT").as_deref() == Ok("1");
    for pid in [launcher, workerd] {
        let inodes = ip_socket_inodes(pid);
        let held: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .expect("fd dir")
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .filter_map(|target| {
                let text = target.to_string_lossy().into_owned();
                let inode = text.strip_prefix("socket:[")?.strip_suffix(']')?;
                inodes.contains(inode).then_some(text)
            })
            .collect();
        assert!(held.is_empty(), "pid {pid} holds IP sockets: {held:?}");
        if enforced {
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).expect("status");
            assert!(
                status.lines().any(|line| line == "Seccomp:\t2"),
                "pid {pid} is not under a seccomp filter:\n{status}"
            );
        }
    }
}

/// Socket inodes of every IPv4 and IPv6 TCP, UDP and raw socket in `pid`'s
/// network namespace.
#[cfg(target_os = "linux")]
fn ip_socket_inodes(pid: u32) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    for table in ["tcp", "tcp6", "udp", "udp6", "raw", "raw6"] {
        let path = std::path::Path::new("/proc")
            .join(pid.to_string())
            .join("net")
            .join(table);
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            if let Some(inode) = line.split_whitespace().nth(9) {
                out.insert(inode.to_string());
            }
        }
    }
    out
}
