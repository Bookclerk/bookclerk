//! The authenticated-endpoint probe must succeed on a real mux and must notice
//! an intentional inherit leak.
//!
//! The leak fixture keeps endpoint A's configured channel working and also
//! hands the guest endpoint B through `bookclerk-jail` (`preserve_fds` on Unix,
//! `JailHandoff` extra handles on Windows). Both proxies share one challenge
//! and answer different tags. The same `channel_ident` set used by the
//! overlapping-session test must include B, so the absence check fails.
//! Replacing A's endpoint with B is not this fixture.
//!
//! Each case has one server reader and one child client. The child sends the
//! session challenge, then Open, then Data containing CONNECT. Raw HTTP on the
//! pipe is not a probe.

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

fn assert_configured_and_extra_visible(outcome: &serde_json::Value) {
    let reported = outcome["tag"].as_str().unwrap_or("");
    assert_eq!(
        reported, "endpoint-a",
        "the configured channel did not answer A's tag: {outcome}"
    );
    let tags = channel::observed_channel_tags(outcome);
    let observed: Vec<&str> = tags.iter().map(String::as_str).collect();
    assert!(
        observed.contains(&"endpoint-a"),
        "configured tag missing from the observed set: {outcome}"
    );
    assert!(
        observed.contains(&"endpoint-b"),
        "extra inherited endpoint B was not observed: {outcome}"
    );
    assert!(
        !channel::foreign_channel_absent(&observed, "endpoint-b"),
        "absence assertion did not fail when B's endpoint was also inherited: {outcome}"
    );
    assert!(
        channel::foreign_channel_absent(&observed, "endpoint-z"),
        "a tag outside the observed set must still count as absent: {outcome}"
    );
    eprintln!(
        "leak channel configured={} tags={observed:?}",
        outcome["tag"]
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

fn jail_bin() -> PathBuf {
    let probe = probe_bin();
    let name = if cfg!(windows) {
        "bookclerk-jail.exe"
    } else {
        "bookclerk-jail"
    };
    let mut candidates = Vec::new();
    if let Some(dir) = probe.parent() {
        candidates.push(dir.join(name));
        if let Some(parent) = dir.parent() {
            candidates.push(parent.join(name));
        }
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("bookclerk-jail was not next to {}", probe.display()))
}

fn leak_spec(probe: &std::path::Path, scratch: &std::path::Path) -> bookclerk_sandbox::Spec {
    let mut reads = Vec::new();
    if let Some(dir) = probe.parent() {
        reads.push(dir.to_path_buf());
    }
    reads.push(probe.to_path_buf());
    bookclerk_sandbox::Spec {
        reads,
        writes: vec![scratch.to_path_buf()],
        net: bookclerk_sandbox::NetPolicy::Deny,
        allow_exec: true,
        system_paths: true,
        enforcement: bookclerk_sandbox::Enforcement::Required,
        unix_socket_dirs: Some(Vec::new()),
        ..bookclerk_sandbox::Spec::new("test:endpoint-leak")
    }
}

fn assert_leak_output(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "leak guest failed: status {}\nstdout {}\nstderr {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    parse_probe(output)
}

#[cfg(unix)]
#[allow(unsafe_code)]
fn set_cloexec(fd: i32) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    assert!(flags >= 0, "F_GETFD");
    assert_eq!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) },
        0,
        "F_SETFD"
    );
}

/// Dup both client sockets onto fd 3 and fd 4 without clobbering either source.
#[cfg(unix)]
#[allow(unsafe_code)]
fn place_leak_fds(fd_a: i32, fd_b: i32) -> std::io::Result<()> {
    let a_tmp = unsafe { libc::fcntl(fd_a, libc::F_DUPFD_CLOEXEC, 5) };
    let b_tmp = unsafe { libc::fcntl(fd_b, libc::F_DUPFD_CLOEXEC, 5) };
    if a_tmp < 0 || b_tmp < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let placed = bookclerk_sandbox::inherit_fd_at(a_tmp, 3)
        .and_then(|()| bookclerk_sandbox::inherit_fd_at(b_tmp, 4));
    unsafe {
        if a_tmp != 3 {
            libc::close(a_tmp);
        }
        if b_tmp != 4 {
            libc::close(b_tmp);
        }
        if fd_a > 4 {
            libc::close(fd_a);
        }
        if fd_b > 4 && fd_b != fd_a {
            libc::close(fd_b);
        }
    }
    placed
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(unsafe_code)]
async fn supplied_endpoint_fails_channel_absence() {
    use std::os::fd::IntoRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let scratch = tempfile::tempdir().expect("scratch");
    let probe = probe_bin();
    let mut spec = leak_spec(&probe, scratch.path());
    spec.preserve_fds = vec![3, 4];
    let spec_json = serde_json::to_string(&spec).expect("spec");
    let (client_a, server_a) = tokio::net::UnixStream::pair().expect("socketpair a");
    let (client_b, server_b) = tokio::net::UnixStream::pair().expect("socketpair b");
    let client_a = client_a.into_std().expect("std a");
    let client_b = client_b.into_std().expect("std b");
    let fd_a = client_a.into_raw_fd();
    let fd_b = client_b.into_raw_fd();
    set_cloexec(fd_a);
    set_cloexec(fd_b);
    let challenge = [0x21_u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    let fence_a = Arc::new(AtomicBool::new(false));
    let fence_b = Arc::new(AtomicBool::new(false));
    let proxy_a = bookclerk_workerd::socket_proxy::spawn_link_with_challenge_tag(
        server_a,
        bookclerk_plugin_manifest::EgressPolicy::deny(),
        fence_a,
        challenge,
        "endpoint-a",
    )
    .expect("proxy a");
    let proxy_b = bookclerk_workerd::socket_proxy::spawn_link_with_challenge_tag(
        server_b,
        bookclerk_plugin_manifest::EgressPolicy::deny(),
        fence_b,
        challenge,
        "endpoint-b",
    )
    .expect("proxy b");
    let jail = jail_bin();
    let mut cmd = Command::new(&jail);
    cmd.arg(&probe)
        .arg("--channel-ident")
        .env(bookclerk_sandbox::SPEC_ENV, &spec_json)
        .env(
            bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV,
            hex_encode(&challenge),
        )
        .env(bookclerk_plugin_sdk::SOCKET_PROXY_ENV, "fd:3")
        .env_remove(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV)
        .current_dir(scratch.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(move || place_leak_fds(fd_a, fd_b));
    }
    let output = tokio::task::spawn_blocking(move || cmd.output().expect("spawn jail"))
        .await
        .expect("join");
    unsafe {
        libc::close(fd_a);
        libc::close(fd_b);
    }
    let outcome = assert_leak_output(&output);
    assert_configured_and_extra_visible(&outcome);
    drop(proxy_a);
    drop(proxy_b);
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supplied_endpoint_fails_channel_absence() {
    use std::io::Write;
    use std::os::windows::io::AsRawHandle;
    use std::process::Stdio;

    let scratch = tempfile::tempdir().expect("scratch");
    let probe = probe_bin();
    let spec = leak_spec(&probe, scratch.path());
    let spec_json = serde_json::to_string(&spec).expect("spec");
    let pipes_a = bookclerk_sandbox::StdioEnds::pair_overlapped().expect("pipes a");
    let pipes_b = bookclerk_sandbox::StdioEnds::pair_overlapped().expect("pipes b");
    for end in [
        &pipes_a.guest_stdin,
        &pipes_a.guest_stdout,
        &pipes_b.guest_stdin,
        &pipes_b.guest_stdout,
    ] {
        end.set_inheritable(false).expect("clear inherit");
    }
    let server_a_read = pipe_from_owned(pipes_a.host_stdout.into_owned_handle());
    let server_a_write = pipe_from_owned(pipes_a.host_stdin.into_owned_handle());
    let server_b_read = pipe_from_owned(pipes_b.host_stdout.into_owned_handle());
    let server_b_write = pipe_from_owned(pipes_b.host_stdin.into_owned_handle());
    let challenge = [0x21_u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
    let fence_a = Arc::new(AtomicBool::new(false));
    let fence_b = Arc::new(AtomicBool::new(false));
    let proxy_a = bookclerk_workerd::socket_proxy::spawn_halves_with_challenge_tag(
        server_a_read,
        server_a_write,
        bookclerk_plugin_manifest::EgressPolicy::deny(),
        fence_a,
        challenge,
        "endpoint-a",
    )
    .expect("proxy a");
    let proxy_b = bookclerk_workerd::socket_proxy::spawn_halves_with_challenge_tag(
        server_b_read,
        server_b_write,
        bookclerk_plugin_manifest::EgressPolicy::deny(),
        fence_b,
        challenge,
        "endpoint-b",
    )
    .expect("proxy b");
    let jail = jail_bin();
    let mut child = Command::new(&jail)
        .arg(&probe)
        .arg("--channel-ident")
        .env(bookclerk_sandbox::SPEC_ENV, &spec_json)
        .env(bookclerk_sandbox::JAIL_HANDOFF_ENV, "1")
        .env(
            bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV,
            hex_encode(&challenge),
        )
        .env_remove(bookclerk_plugin_sdk::SOCKET_PROXY_ENV)
        .env_remove(bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV)
        .current_dir(scratch.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn jail");
    let target = child.as_raw_handle();
    let a_read =
        bookclerk_sandbox::duplicate_handle_into(pipes_a.guest_stdin.as_raw_handle(), target)
            .expect("dup A read");
    let a_write =
        bookclerk_sandbox::duplicate_handle_into(pipes_a.guest_stdout.as_raw_handle(), target)
            .expect("dup A write");
    let b_read =
        bookclerk_sandbox::duplicate_handle_into(pipes_b.guest_stdin.as_raw_handle(), target)
            .expect("dup B read");
    let b_write =
        bookclerk_sandbox::duplicate_handle_into(pipes_b.guest_stdout.as_raw_handle(), target)
            .expect("dup B write");
    let handoff = bookclerk_sandbox::JailHandoff {
        v: bookclerk_sandbox::JailHandoff::VERSION,
        stdin: None,
        stdout: None,
        extra: vec![
            bookclerk_sandbox::JailHandoffExtra {
                env: bookclerk_plugin_sdk::SOCKET_PROXY_ENV.into(),
                handle: a_read,
            },
            bookclerk_sandbox::JailHandoffExtra {
                env: bookclerk_plugin_sdk::SOCKET_PROXY_WRITE_ENV.into(),
                handle: a_write,
            },
            bookclerk_sandbox::JailHandoffExtra {
                env: String::new(),
                handle: b_read,
            },
            bookclerk_sandbox::JailHandoffExtra {
                env: String::new(),
                handle: b_write,
            },
        ],
    };
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(stdin, "{}", handoff.to_line().expect("handoff line")).expect("write handoff");
        stdin.flush().expect("flush handoff");
    }
    let guest_a_read = pipes_a.guest_stdin;
    let guest_a_write = pipes_a.guest_stdout;
    let guest_b_read = pipes_b.guest_stdin;
    let guest_b_write = pipes_b.guest_stdout;
    let output = tokio::task::spawn_blocking(move || {
        let output = child.wait_with_output().expect("wait jail");
        drop(guest_a_read);
        drop(guest_a_write);
        drop(guest_b_read);
        drop(guest_b_write);
        output
    })
    .await
    .expect("join");
    let outcome = assert_leak_output(&output);
    assert_configured_and_extra_visible(&outcome);
    drop(proxy_a);
    drop(proxy_b);
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
