//! The authenticated-endpoint probe must succeed on a real mux and must notice
//! one extra endpoint handed through the production guest launch.
//!
//! The extra-endpoint case keeps A's configured channel working. Endpoint B is
//! authenticated with its own challenge in this process, then added to the
//! guest handoff (`preserve_fds` fd 4 on Unix, `JailHandoff` extras on
//! Windows) by `PluginSession::spawn_with`. After A has connected, the same
//! `channel_ident` RPC used by the overlapping-session test reports B.
//! Replaying A's challenge onto B is not this fixture. Replacing A's endpoint
//! with B is not this fixture.
//!
//! The inherit cases have one server reader and one child client. The child
//! sends Open, then Data containing CONNECT. Raw HTTP on the pipe is not a
//! probe.

#![allow(clippy::missing_docs_in_private_items)]

#[path = "native_gateway/channel.rs"]
mod channel;
#[path = "native_gateway/harness.rs"]
mod ng_harness;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use bookclerk_plugin_host::{
    TEST_CHANNEL_IDENT_ENV, TEST_CHANNEL_TAG_FILE, TEST_INJECT_EXTRA_ENDPOINT_ENV,
};
use ng_harness::{open_session, probe, step, Install, Listener};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn probe_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_native_gateway_probe"))
}

/// `true` when the child did not inherit a usable endpoint.
fn endpoint_sealed(outcome: &serde_json::Value) -> bool {
    outcome["unsupported"] != true
        && outcome["collided"] != true
        && outcome["opened_stream"] == false
        && outcome["reached_proxy"] != true
        && outcome["not_inherited"] == true
}

fn spawn_probe(args: &[String]) -> std::process::Output {
    Command::new(probe_bin())
        .args(args)
        .env_remove(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV)
        .output()
        .expect("spawn probe")
}

fn parse_probe(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "probe stdout is not JSON ({err}): stdout {} stderr {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// Stop the server once the child has exited.
///
/// A sealed endpoint never connects. Aborting the accept avoids sitting in
/// the server's own timeout after the child is already gone. A leaked
/// endpoint has already read the status line before the child exits; the
/// release lets that server task finish, and the abort is only the fallback.
async fn finish_server(
    release_tx: tokio::sync::oneshot::Sender<()>,
    mut server_task: tokio::task::JoinHandle<()>,
) {
    let _ = release_tx.send(());
    tokio::select! {
        result = &mut server_task => {
            result.expect("server");
        }
        () = tokio::time::sleep(Duration::from_millis(200)) => {
            server_task.abort();
            let _ = server_task.await;
        }
    }
}

/// Serve one authenticated mux. `release` keeps the writer alive until the
/// child has finished reading the status line.
async fn serve_authenticated(
    reader: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    writer: impl tokio::io::AsyncWrite + Unpin + Send + 'static,
    release: tokio::sync::oneshot::Receiver<()>,
) {
    let mux = bookclerk_plugin_sdk::mux::Mux::server(reader, writer);
    let accepted = tokio::time::timeout(Duration::from_secs(5), mux.accept()).await;
    let Ok(Ok(mut stream)) = accepted else {
        drop(mux);
        return;
    };
    let mut buf = vec![0_u8; 512];
    let n = stream.read(&mut buf).await.expect("read CONNECT");
    assert!(
        buf[..n].starts_with(b"CONNECT "),
        "expected mux CONNECT data, got {}",
        String::from_utf8_lossy(&buf[..n])
    );
    stream
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .expect("write 200");
    stream.shutdown().await.expect("flush 200");
    let _ = release.await;
    drop(stream);
    drop(mux);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplied_authenticated_endpoint_opens_a_stream() {
    let outcome = unix_fixture(true).await;
    assert_eq!(
        outcome["opened_stream"], true,
        "probe missed a supplied authenticated endpoint: {outcome}"
    );
    assert_eq!(outcome["reached_proxy"], true, "{outcome}");
    assert!(
        !endpoint_sealed(&outcome),
        "negative assertion did not notice the inherited endpoint: {outcome}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleared_inherit_does_not_open_a_stream() {
    let outcome = unix_fixture(false).await;
    assert!(
        endpoint_sealed(&outcome),
        "cleared inherit still looked usable: {outcome}"
    );
}

#[cfg(unix)]
#[allow(unsafe_code)]
async fn unix_fixture(inherit: bool) -> serde_json::Value {
    use std::os::fd::{FromRawFd, IntoRawFd};

    let (client, server) = tokio::net::UnixStream::pair().expect("socketpair");
    let client = client.into_std().expect("into_std");
    let fd = client.into_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD");
    let flags = if inherit {
        flags & !libc::FD_CLOEXEC
    } else {
        flags | libc::FD_CLOEXEC
    };
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags) },
        0,
        "F_SETFD"
    );
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (server_read, server_write) = server.into_split();
    let server_task = tokio::spawn(serve_authenticated(server_read, server_write, release_rx));
    let args = vec![
        "--endpoint-connect".to_string(),
        format!("fd:{fd}"),
        String::new(),
        "127.0.0.1".to_string(),
        "9".to_string(),
    ];
    let output = tokio::task::spawn_blocking(move || spawn_probe(&args))
        .await
        .expect("join");
    let outcome = parse_probe(&output);
    finish_server(release_tx, server_task).await;
    drop(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) });
    outcome
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplied_authenticated_endpoint_opens_a_stream() {
    let outcome = windows_fixture(true).await;
    assert_eq!(
        outcome["opened_stream"], true,
        "probe missed a supplied authenticated endpoint: {outcome}"
    );
    assert_eq!(outcome["reached_proxy"], true, "{outcome}");
    assert!(
        !endpoint_sealed(&outcome),
        "negative assertion did not notice the inherited endpoint: {outcome}"
    );
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cleared_inherit_does_not_open_a_stream() {
    let outcome = windows_fixture(false).await;
    assert!(
        endpoint_sealed(&outcome),
        "cleared inherit still looked usable: {outcome}"
    );
}

#[cfg(windows)]
#[allow(unsafe_code)]
async fn windows_fixture(inherit: bool) -> serde_json::Value {
    let pipes = bookclerk_sandbox::StdioEnds::pair_overlapped().expect("pipes");
    pipes
        .guest_stdin
        .set_inheritable(inherit)
        .expect("read inherit");
    pipes
        .guest_stdout
        .set_inheritable(inherit)
        .expect("write inherit");
    let read_value = pipes.guest_stdin.handle_value();
    let write_value = pipes.guest_stdout.handle_value();
    let server_read = pipe_from_owned(pipes.host_stdout.into_owned_handle());
    let server_write = pipe_from_owned(pipes.host_stdin.into_owned_handle());
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(serve_authenticated(server_read, server_write, release_rx));
    let args = vec![
        "--endpoint-connect".to_string(),
        format!("handle:{read_value}"),
        format!("handle:{write_value}"),
        "127.0.0.1".to_string(),
        "9".to_string(),
    ];
    let output = tokio::task::spawn_blocking(move || spawn_probe(&args))
        .await
        .expect("join");
    let outcome = parse_probe(&output);
    finish_server(release_tx, server_task).await;
    // Keep the guest ends alive until the child has inherited or failed.
    drop(pipes.guest_stdin);
    drop(pipes.guest_stdout);
    outcome
}

/// Clears the host-only endpoint env even when the test panics.
struct ClearHostEndpointEnv;

impl Drop for ClearHostEndpointEnv {
    fn drop(&mut self) {
        std::env::remove_var(TEST_INJECT_EXTRA_ENDPOINT_ENV);
        std::env::remove_var(TEST_CHANNEL_IDENT_ENV);
    }
}

fn challenge_accepted_count() -> usize {
    bookclerk_sandbox::snapshot_spawn_diagnostics()
        .matches("socket proxy challenge accepted")
        .count()
}

async fn wait_until_challenge_accepted(before: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if challenge_accepted_count() > before {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "endpoint B did not accept its own challenge (before={before}, now={})",
            challenge_accepted_count()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Parent copies of B. Dropped after the guest jail has duplicated them.
struct HeldClient {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
    #[cfg(windows)]
    read: bookclerk_sandbox::DuplexHalf,
    #[cfg(windows)]
    write: bookclerk_sandbox::DuplexHalf,
}

/// Start B, write B's challenge, and return the inject spec for the host.
#[allow(unsafe_code)] // `OwnedFd::from_raw_fd` keeps the Unix client socket alive.
async fn prepare_authenticated_extra(
    challenge: &[u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN],
) -> (
    bookclerk_workerd::socket_proxy::ProxyServer,
    String,
    HeldClient,
) {
    #[cfg(unix)]
    {
        use std::os::fd::{FromRawFd, IntoRawFd};

        let (mut client, server) = tokio::net::UnixStream::pair().expect("socketpair");
        let proxy = bookclerk_workerd::socket_proxy::spawn_link_with_challenge_tag(
            server,
            bookclerk_plugin_manifest::EgressPolicy::deny(),
            Arc::new(AtomicBool::new(false)),
            *challenge,
            "endpoint-b",
        )
        .expect("proxy b");
        client
            .write_all(challenge)
            .await
            .expect("write B challenge");
        client.flush().await.expect("flush B challenge");
        let client = client.into_std().expect("std");
        let fd = client.into_raw_fd();
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
        (proxy, format!("fd:{fd}"), HeldClient { fd: owned })
    }
    #[cfg(windows)]
    {
        let pipes = bookclerk_sandbox::StdioEnds::pair_overlapped().expect("pipes b");
        pipes
            .guest_stdin
            .set_inheritable(false)
            .expect("read inherit");
        pipes
            .guest_stdout
            .set_inheritable(false)
            .expect("write inherit");
        let server_read = pipe_from_owned(pipes.host_stdout.into_owned_handle());
        let server_write = pipe_from_owned(pipes.host_stdin.into_owned_handle());
        let proxy = bookclerk_workerd::socket_proxy::spawn_halves_with_challenge_tag(
            server_read,
            server_write,
            bookclerk_plugin_manifest::EgressPolicy::deny(),
            Arc::new(AtomicBool::new(false)),
            *challenge,
            "endpoint-b",
        )
        .expect("proxy b");
        let dup = bookclerk_sandbox::duplicate_owned_handle(pipes.guest_stdout.as_raw_handle())
            .expect("dup B write");
        let mut writer = pipe_from_owned(dup);
        writer
            .write_all(challenge)
            .await
            .expect("write B challenge");
        writer.flush().await.expect("flush B challenge");
        drop(writer);
        let spec = format!(
            "handle:{},handle:{}",
            pipes.guest_stdin.handle_value(),
            pipes.guest_stdout.handle_value()
        );
        (
            proxy,
            spec,
            HeldClient {
                read: pipes.guest_stdin,
                write: pipes.guest_stdout,
            },
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = challenge;
        panic!("extra endpoint injection is implemented for unix and windows");
    }
}

/// A's normal session keeps its own channel, and `channel_ident` still sees B.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn injected_endpoint_is_visible_after_authentication() {
    let _guard = ClearHostEndpointEnv;
    std::env::set_var(TEST_CHANNEL_IDENT_ENV, "1");
    std::env::remove_var(TEST_INJECT_EXTRA_ENDPOINT_ENV);

    let challenge_b = [0x5a_u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    let before = challenge_accepted_count();
    let (proxy_b, spec, parent) = prepare_authenticated_extra(&challenge_b).await;
    wait_until_challenge_accepted(before).await;
    std::env::set_var(TEST_INJECT_EXTRA_ENDPOINT_ENV, &spec);

    let listener = Listener::bind(true).await;
    let install = Install::new(listener.port);
    let session = install.spawn().await;
    #[cfg(unix)]
    {
        let HeldClient { fd } = parent;
        drop(fd);
    }
    #[cfg(windows)]
    {
        let HeldClient { read, write } = parent;
        drop((read, write));
    }
    #[cfg(not(any(unix, windows)))]
    drop(parent);

    open_session(&session).await;
    let connected = probe(&session, "connect", listener.port, "extra-endpoint").await;
    assert_eq!(
        connected["ok"], true,
        "A lost its configured channel: {connected}"
    );
    assert!(
        listener.wait_for_accepts(1).await,
        "A's connect did not reach the grant"
    );

    let challenge_a = probe(&session, "session_challenge", 0, "").await;
    assert_eq!(challenge_a["ok"], true, "{challenge_a}");
    let hex_a = challenge_a["hex"].as_str().unwrap_or("");
    assert_eq!(
        hex_a.len(),
        64,
        "A challenge hex was not 32 bytes: {challenge_a}"
    );
    assert_ne!(
        hex_a,
        hex_encode(&challenge_b),
        "A and B shared a session challenge"
    );

    let ident = probe(&session, "channel_ident", 0, "").await;
    let dir = session.session_dir().expect("session dir");
    let file_tag = std::fs::read_to_string(dir.join(TEST_CHANNEL_TAG_FILE))
        .unwrap_or_else(|err| panic!("channel tag in {}: {err}", dir.display()));
    let file_tag = file_tag.trim();
    assert_eq!(
        ident["tag"].as_str().unwrap_or(""),
        file_tag,
        "the configured channel did not answer A's tag: {ident}"
    );
    assert_ne!(file_tag, "endpoint-b");
    let tags = channel::observed_channel_tags(&ident);
    let observed: Vec<&str> = tags.iter().map(String::as_str).collect();
    assert!(
        observed.contains(&file_tag),
        "configured tag missing from the observed set: {ident}"
    );
    assert!(
        observed.contains(&"endpoint-b"),
        "handed endpoint B was not observed: {ident}"
    );
    assert_eq!(
        channel::channel_endpoint_isolation(&ident, "endpoint-b"),
        Ok(false),
        "absence assertion did not detect B: {ident}"
    );
    assert!(
        channel::foreign_channel_absent(&observed, "endpoint-z"),
        "a tag outside the observed set must still count as absent: {ident}"
    );
    let env = probe(&session, "env_keys", 0, "").await;
    let published = |name: &str| {
        env["keys"]
            .as_array()
            .is_some_and(|keys| keys.iter().any(|key| key == name))
    };
    assert!(
        published(bookclerk_plugin_sdk::TEST_EXTRA_ENDPOINT_ENV),
        "guest env did not publish the extra endpoint read slot"
    );
    #[cfg(windows)]
    assert!(
        published(bookclerk_plugin_sdk::TEST_EXTRA_ENDPOINT_WRITE_ENV),
        "guest env did not publish the extra endpoint write slot"
    );
    step(&format!(
        "leak channel configured={} tags={observed:?} extra_status={} extra_error={} isolation=false",
        ident["tag"], ident["extra_status"], ident["extra_error"]
    ));
    drop(proxy_b);
    drop(session);
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn pipe_from_owned(
    handle: std::os::windows::io::OwnedHandle,
) -> tokio::net::windows::named_pipe::NamedPipeClient {
    use std::os::windows::io::IntoRawHandle;
    unsafe {
        tokio::net::windows::named_pipe::NamedPipeClient::from_raw_handle(handle.into_raw_handle())
    }
    .expect("overlapped pipe")
}
