//! The authenticated-endpoint probe must succeed on a real mux and must notice
//! an intentional inherit leak.
//!
//! A second fixture supplies one session's real proxy endpoint, with one
//! server reader, and checks that the channel-absence predicate fails. That
//! spawn is not the jail and does not change its handle list.
//!
//! Each case has one server reader and one child client. The server has already
//! passed the session challenge (`Mux::server` with no second reader). The
//! child sends Open, then Data containing CONNECT. Raw HTTP on the pipe is not
//! a probe.

#![allow(clippy::missing_docs_in_private_items)]

#[path = "native_gateway/channel.rs"]
mod channel;

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

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

fn assert_supplied_endpoint_is_visible(outcome: &serde_json::Value, foreign: &str) {
    let reported = outcome["tag"].as_str().unwrap_or("");
    assert_eq!(
        reported, foreign,
        "the supplied endpoint did not answer its own tag: {outcome}"
    );
    assert!(
        !channel::foreign_channel_absent(reported, foreign),
        "absence assertion did not fail when B's endpoint was supplied: {outcome}"
    );
    assert!(
        channel::foreign_channel_absent(reported, "endpoint-a"),
        "a different tag must still count as absent: {outcome}"
    );
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

fn spawn_channel_child(envs: &[(&'static str, String)]) -> std::process::Output {
    let mut cmd = Command::new(probe_bin());
    cmd.arg("--channel-ident")
        .env_remove(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV)
        .env_remove(bookclerk_plugin_sdk::SOCKET_PROXY_ENV)
        .env_remove(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV);
    for (key, value) in envs {
        cmd.env(*key, value);
    }
    cmd.output().expect("spawn channel child")
}

/// One production proxy reader and one child client holding that pipe.
async fn finish_channel_child(
    proxy: bookclerk_workerd::socket_proxy::ProxyServer,
    envs: Vec<(&'static str, String)>,
) -> serde_json::Value {
    let output = tokio::task::spawn_blocking(move || spawn_channel_child(&envs))
        .await
        .expect("join");
    let outcome = parse_probe(&output);
    drop(proxy);
    outcome
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(unsafe_code)]
async fn supplied_endpoint_fails_channel_absence() {
    use std::os::fd::{FromRawFd, IntoRawFd};

    let (client, server) = tokio::net::UnixStream::pair().expect("socketpair");
    let client = client.into_std().expect("into_std");
    let fd = client.into_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD");
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0,
        "F_SETFD"
    );
    let challenge = [0x11_u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    let fence = Arc::new(AtomicBool::new(false));
    let proxy = bookclerk_workerd::socket_proxy::spawn_link_with_challenge_tag(
        server,
        bookclerk_plugin_manifest::EgressPolicy::deny(),
        fence,
        challenge,
        "endpoint-b",
    )
    .expect("proxy");
    let outcome = finish_channel_child(
        proxy,
        vec![
            (
                bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV,
                hex_encode(&challenge),
            ),
            (bookclerk_plugin_sdk::SOCKET_PROXY_ENV, format!("fd:{fd}")),
        ],
    )
    .await;
    assert_supplied_endpoint_is_visible(&outcome, "endpoint-b");
    drop(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) });
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplied_endpoint_fails_channel_absence() {
    let pipes = bookclerk_sandbox::StdioEnds::pair_overlapped().expect("pipes");
    pipes
        .guest_stdin
        .set_inheritable(true)
        .expect("read inherit");
    pipes
        .guest_stdout
        .set_inheritable(true)
        .expect("write inherit");
    let read_value = pipes.guest_stdin.handle_value();
    let write_value = pipes.guest_stdout.handle_value();
    let server_read = pipe_from_owned(pipes.host_stdout.into_owned_handle());
    let server_write = pipe_from_owned(pipes.host_stdin.into_owned_handle());
    let challenge = [0x11_u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    let fence = Arc::new(AtomicBool::new(false));
    let proxy = bookclerk_workerd::socket_proxy::spawn_halves_with_challenge_tag(
        server_read,
        server_write,
        bookclerk_plugin_manifest::EgressPolicy::deny(),
        fence,
        challenge,
        "endpoint-b",
    )
    .expect("proxy");
    let outcome = finish_channel_child(
        proxy,
        vec![
            (
                bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV,
                hex_encode(&challenge),
            ),
            (
                bookclerk_plugin_sdk::SOCKET_PROXY_ENV,
                format!("handle:{read_value}"),
            ),
            (
                bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV,
                format!("handle:{write_value}"),
            ),
        ],
    )
    .await;
    assert_supplied_endpoint_is_visible(&outcome, "endpoint-b");
    drop(pipes.guest_stdin);
    drop(pipes.guest_stdout);
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
