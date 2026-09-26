//! Host-mediated TCP proxy for native-behind-workerd guests.
//!
//! Native plugins must not open ambient `AF_INET` sockets. They speak HTTP
//! CONNECT over an inherited multiplexed link ([`spawn_link`]); the launcher
//! applies the same [`EgressPolicy`] as workerd `fetch()`/`connect()` and
//! splices bytes. [`spawn_unix`] remains for unit tests that bind a pathname
//! listener.

#![allow(clippy::missing_docs_in_private_items)]

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::EgressPolicy;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Env var naming the socket proxy path for the native SDK.
pub const SOCKET_PROXY_ENV: &str = "BOOKCLERK_SOCKET_PROXY";

/// CONNECT handlers that may run at once. The accept loop takes a permit
/// before `tokio::spawn`.
const MAX_CONNECT_TASKS: usize = 16;
/// CONNECT request line, including the newline.
const MAX_REQUEST_LINE: usize = 2048;
/// Header bytes after the request line.
const MAX_HEADER_BYTES: usize = 8192;
/// Header lines after the request line.
const MAX_HEADER_COUNT: usize = 32;
/// Time allowed to finish the CONNECT request line and headers.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);

/// In-flight host CONNECT proxy. Dropping it cancels accepts and handlers.
///
/// The [`Self::fence`] flag is the session cancel token. Authority revocation
/// sets that flag directly; handlers race it against parsing, DNS, dial, and
/// the byte relay.
pub struct ProxyServer {
    /// Session cancel flag. `true` stops new accepts and in-flight relays.
    fence: Arc<AtomicBool>,
    /// Handler tasks. The accept loop pushes one before each stream runs.
    tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    /// Accept loop. Aborted on drop.
    accept: Option<tokio::task::JoinHandle<()>>,
    /// Mux created inside the accept task after the session challenge.
    ///
    /// Dropping the proxy shuts this down so the reader and writer exit while
    /// the peer is still connected. The accept task's clone is not the owner.
    mux: Arc<Mutex<Option<bookclerk_plugin_sdk::mux::Mux>>>,
}

impl ProxyServer {
    /// Cancel flag shared with the session owner.
    #[must_use]
    pub fn fence(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.fence)
    }

    /// Signal handlers to stop. In-flight work selects on [`Self::fence`].
    pub fn cancel(&self) {
        self.fence.store(true, Ordering::SeqCst);
    }

    /// Handler tasks that have not finished.
    #[must_use]
    pub fn unfinished_handlers(&self) -> usize {
        self.tasks
            .lock()
            .map(|guard| guard.iter().filter(|task| !task.is_finished()).count())
            .unwrap_or(0)
    }
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.fence.store(true, Ordering::SeqCst);
        if let Ok(mut mux) = self.mux.lock() {
            if let Some(mux) = mux.take() {
                mux.shutdown();
            }
        }
        if let Some(task) = self.accept.take() {
            task.abort();
        }
        if let Ok(mut tasks) = self.tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
    }
}

fn track_handler(
    tasks: &Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
    handle: tokio::task::JoinHandle<()>,
) {
    if let Ok(mut guard) = tasks.lock() {
        guard.retain(|task| !task.is_finished());
        guard.push(handle);
    }
}

/// Serve HTTP CONNECT on a Unix listener until the task is cancelled.
///
/// # Errors
///
/// Returns an error when the listener cannot be marked non-blocking or
/// converted to a Tokio listener.
#[cfg(unix)]
pub fn spawn_unix(
    listener: std::os::unix::net::UnixListener,
    policy: EgressPolicy,
    fence: Arc<AtomicBool>,
) -> Result<ProxyServer> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let tasks = Arc::new(Mutex::new(Vec::new()));
    let accept_tasks = Arc::clone(&tasks);
    let accept_fence = Arc::clone(&fence);
    let admits = Arc::new(Semaphore::new(MAX_CONNECT_TASKS));
    let accept = tokio::spawn(async move {
        loop {
            if accept_fence.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                () = wait_fence(&accept_fence) => break,
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let Some(permit) = admit(&admits) else {
                                drop(stream);
                                continue;
                            };
                            let policy = policy.clone();
                            let fence = Arc::clone(&accept_fence);
                            let handle = tokio::spawn(async move {
                                let _permit = permit;
                                if let Err(err) = handle_client(stream, policy, fence).await {
                                    tracing::debug!(error = %err, "socket proxy session ended");
                                }
                            });
                            track_handler(&accept_tasks, handle);
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "socket proxy accept failed");
                            break;
                        }
                    }
                }
            }
        }
    });
    Ok(ProxyServer {
        fence,
        tasks,
        accept: Some(accept),
        mux: Arc::new(Mutex::new(None)),
    })
}

async fn handle_client<S>(stream: S, policy: EgressPolicy, fence: Arc<AtomicBool>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if fence.load(Ordering::SeqCst) {
        bail!("session fenced");
    }
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let handshake = read_connect_headers(&mut reader, &fence);
    let (host, port) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(Ok(parts)) => parts,
        Ok(Err(err)) => {
            let _ = write_fenced(&mut writer, BAD_REQUEST, &fence).await;
            return Err(err);
        }
        Err(_) => {
            let _ = write_fenced(&mut writer, HANDSHAKE_TIMEOUT_BODY, &fence).await;
            bail!("CONNECT handshake timed out");
        }
    };
    if fence.load(Ordering::SeqCst) {
        let _ = write_fenced(
            &mut writer,
            b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            &fence,
        )
        .await;
        bail!("session fenced");
    }
    if !policy.allows_tcp(&host, port) {
        write_fenced(
            &mut writer,
            b"HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n\
tcp connect is not in capabilities.network.tcp",
            &fence,
        )
        .await?;
        bail!("tcp grant denied for {host}:{port}");
    }
    let looked_up = tokio::select! {
        biased;
        () = wait_fence(&fence) => bail!("session fenced"),
        result = tokio::net::lookup_host((host.as_str(), port)) => {
            result.with_context(|| format!("resolve {host}"))?
        }
    };
    let mut addrs: Vec<std::net::SocketAddr> = looked_up
        .filter(|addr| policy.allows_ip(addr.ip()))
        .collect();
    if addrs.is_empty() {
        write_fenced(
            &mut writer,
            b"HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n\
resolved address is outside the granted address space",
            &fence,
        )
        .await?;
        bail!("no allowed address for {host}:{port}");
    }
    // `localhost` often resolves `::1` first. CI Postgres (and many operator
    // hosts) listen IPv4-only; dialing only the first allowed address then RST
    // the Unix client, which sqlx reports as "Connection reset by peer".
    addrs.sort_by_key(std::net::SocketAddr::is_ipv6);
    if let Some(delay) = test_dial_delay() {
        tokio::select! {
            biased;
            () = wait_fence(&fence) => {
                let _ = write_fenced(&mut writer, FORBIDDEN_FENCED, &fence).await;
                bail!("session fenced");
            }
            () = tokio::time::sleep(delay) => {}
        }
    }
    // Recheck after DNS and before the dial is committed.
    if fence.load(Ordering::SeqCst) {
        let _ = write_fenced(&mut writer, FORBIDDEN_FENCED, &fence).await;
        bail!("session fenced");
    }
    let mut last_err: Option<std::io::Error> = None;
    let mut upstream = None;
    for dest in addrs {
        if fence.load(Ordering::SeqCst) {
            bail!("session fenced");
        }
        let connected = tokio::select! {
            biased;
            () = wait_fence(&fence) => Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "session fenced",
            )),
            result = TcpStream::connect(dest) => result,
        };
        match connected {
            Ok(stream) => {
                if fence.load(Ordering::SeqCst) {
                    drop(stream);
                    bail!("session fenced");
                }
                upstream = Some(stream);
                break;
            }
            Err(err) => last_err = Some(err),
        }
    }
    let Some(mut upstream) = upstream else {
        let detail = last_err
            .map(|err| err.to_string())
            .unwrap_or_else(|| "no addresses".into());
        let _ = write_fenced(
            &mut writer,
            b"HTTP/1.1 502 Bad Gateway\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n\
could not dial an allowed address",
            &fence,
        )
        .await;
        bail!("dial {host}:{port}: {detail}");
    };
    write_fenced(
        &mut writer,
        b"HTTP/1.1 200 Connection Established\r\n\r\n",
        &fence,
    )
    .await?;
    tokio::select! {
        biased;
        () = wait_fence(&fence) => bail!("session fenced"),
        result = writer.flush() => result?,
    }
    let reader = reader.into_inner();
    let mut client = reader.unsplit(writer);
    splice(&mut client, &mut upstream, &fence).await
}

const FORBIDDEN_FENCED: &[u8] =
    b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

const BAD_REQUEST: &[u8] =
    b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
const HANDSHAKE_TIMEOUT_BODY: &[u8] =
    b"HTTP/1.1 408 Request Timeout\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";

fn admit(admits: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    admits.clone().try_acquire_owned().ok()
}

async fn read_connect_headers<R>(
    reader: &mut BufReader<R>,
    fence: &AtomicBool,
) -> Result<(String, u16)>
where
    R: AsyncRead + Unpin,
{
    let line = read_bounded_line(reader, MAX_REQUEST_LINE, fence).await?;
    let request = String::from_utf8_lossy(&line).trim_end().to_string();
    let (host, port) = parse_connect(&request)?;
    let mut header_bytes = 0usize;
    let mut header_count = 0usize;
    loop {
        let header = read_bounded_line(reader, MAX_REQUEST_LINE, fence).await?;
        if header == b"\n" || header == b"\r\n" || header.iter().all(u8::is_ascii_whitespace) {
            break;
        }
        header_count += 1;
        header_bytes = header_bytes.saturating_add(header.len());
        if header_count > MAX_HEADER_COUNT || header_bytes > MAX_HEADER_BYTES {
            bail!("CONNECT headers exceed the admission cap");
        }
    }
    Ok((host, port))
}

/// Reads one line, stopping at `max` bytes. The cap is checked before the
/// buffer grows past it.
async fn read_bounded_line<R>(
    reader: &mut BufReader<R>,
    max: usize,
    fence: &AtomicBool,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut out = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if out.len() >= max {
            bail!("CONNECT line exceeds {max} bytes");
        }
        tokio::select! {
            biased;
            () = wait_fence(fence) => bail!("session fenced"),
            result = reader.read(&mut byte) => {
                let n = result?;
                if n == 0 {
                    bail!("CONNECT handshake closed");
                }
                out.push(byte[0]);
                if byte[0] == b'\n' {
                    return Ok(out);
                }
            }
        }
    }
}

async fn write_fenced<W>(writer: &mut W, bytes: &[u8], fence: &AtomicBool) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::select! {
        biased;
        () = wait_fence(fence) => bail!("session fenced"),
        result = writer.write_all(bytes) => {
            result?;
            Ok(())
        }
    }
}

/// Test-only pause before `TcpStream::connect`, raced with the cancel flag.
fn test_dial_delay() -> Option<Duration> {
    let ms = std::env::var("BOOKCLERK_TEST_PROXY_DIAL_DELAY_MS").ok()?;
    let ms = ms.parse::<u64>().ok()?;
    Some(Duration::from_millis(ms))
}

/// Serve HTTP CONNECT on mux-accepted streams from an inherited sibling link.
///
/// # Errors
///
/// Returns an error only if the accept task cannot be spawned (it does not).
pub fn spawn_link<S>(link: S, policy: EgressPolicy, fence: Arc<AtomicBool>) -> Result<ProxyServer>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let (reader, writer) = tokio::io::split(link);
    spawn_halves(reader, writer, policy, fence)
}

/// [`spawn_link`] that reads a 32-byte session challenge before any mux frame.
///
/// A mismatch, EOF, or fence closes the link and serves nothing. The numeric
/// descriptor is not treated as the session identity.
///
/// # Errors
///
/// Returns an error only if the accept task cannot be spawned (it does not).
pub fn spawn_link_with_challenge<S>(
    link: S,
    policy: EgressPolicy,
    fence: Arc<AtomicBool>,
    challenge: [u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN],
) -> Result<ProxyServer>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let (reader, writer) = tokio::io::split(link);
    spawn_halves_with_challenge(reader, writer, policy, fence, challenge)
}

/// Serve HTTP CONNECT on mux halves that are already separate pipes.
///
/// Windows product spawns pass two unidirectional handles. Splitting one
/// duplex pipe deadlocks the mux when a read and a write are in flight.
///
/// # Errors
///
/// Returns an error only if the accept task cannot be spawned (it does not).
pub fn spawn_halves<R, W>(
    reader: R,
    writer: W,
    policy: EgressPolicy,
    fence: Arc<AtomicBool>,
) -> Result<ProxyServer>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    spawn_halves_inner(reader, writer, policy, fence, None)
}

/// [`spawn_halves`] that requires `challenge` on the read half before mux frames.
///
/// # Errors
///
/// Returns an error only if the accept task cannot be spawned (it does not).
pub fn spawn_halves_with_challenge<R, W>(
    reader: R,
    writer: W,
    policy: EgressPolicy,
    fence: Arc<AtomicBool>,
    challenge: [u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN],
) -> Result<ProxyServer>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    spawn_halves_inner(reader, writer, policy, fence, Some(challenge))
}

fn spawn_halves_inner<R, W>(
    reader: R,
    writer: W,
    policy: EgressPolicy,
    fence: Arc<AtomicBool>,
    challenge: Option<[u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN]>,
) -> Result<ProxyServer>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    let tasks = Arc::new(Mutex::new(Vec::new()));
    let accept_tasks = Arc::clone(&tasks);
    let accept_fence = Arc::clone(&fence);
    let admits = Arc::new(Semaphore::new(MAX_CONNECT_TASKS));
    let mux_slot = Arc::new(Mutex::new(None));
    let publish_mux = Arc::clone(&mux_slot);
    let accept = tokio::spawn(async move {
        let mut reader = reader;
        if let Some(expected) = challenge {
            let mut got = [0u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
            let matched = tokio::select! {
                biased;
                () = wait_fence(&accept_fence) => false,
                result = reader.read_exact(&mut got) => result.is_ok() && got == expected,
            };
            if !matched {
                tracing::debug!("socket proxy session challenge rejected");
                return;
            }
        }
        let mux = bookclerk_plugin_sdk::mux::Mux::server(reader, writer);
        if let Ok(mut slot) = publish_mux.lock() {
            *slot = Some(mux.clone());
        }
        loop {
            if accept_fence.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                () = wait_fence(&accept_fence) => break,
                accepted = mux.accept() => {
                    match accepted {
                        Ok(stream) => {
                            let Some(permit) = admit(&admits) else {
                                drop(stream);
                                continue;
                            };
                            let policy = policy.clone();
                            let fence = Arc::clone(&accept_fence);
                            let handle = tokio::spawn(async move {
                                let _permit = permit;
                                if let Err(err) = handle_client(stream, policy, fence).await {
                                    tracing::debug!(error = %err, "socket proxy session ended");
                                }
                            });
                            track_handler(&accept_tasks, handle);
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "socket proxy mux closed");
                            break;
                        }
                    }
                }
            }
        }
    });
    Ok(ProxyServer {
        fence,
        tasks,
        accept: Some(accept),
        mux: mux_slot,
    })
}

fn parse_connect(request: &str) -> Result<(String, u16)> {
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    if !method.eq_ignore_ascii_case("CONNECT") {
        bail!("expected CONNECT, got {request}");
    }
    parse_authority(target)
}

/// Parses `host:port` or `[ipv6]:port`.
///
/// # Errors
///
/// Returns an error when the target is missing a port or the port is not a
/// `u16`.
pub fn parse_authority(target: &str) -> Result<(String, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| anyhow::anyhow!("invalid IPv6 CONNECT target `{target}`"))?;
        let port: u16 = port_part
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid port `{port_part}`"))?;
        return Ok((host.to_string(), port));
    }
    let (host, port_part) = target
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid CONNECT target `{target}`"))?;
    let port: u16 = port_part
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid port `{port_part}`"))?;
    Ok((host.to_string(), port))
}

async fn wait_fence(fence: &AtomicBool) {
    while !fence.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn splice<C, U>(client: &mut C, upstream: &mut U, fence: &AtomicBool) -> Result<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    U: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut client_buf = vec![0_u8; 16 * 1024];
    let mut up_buf = vec![0_u8; 16 * 1024];
    loop {
        if fence.load(Ordering::SeqCst) {
            let _ = client.shutdown().await;
            let _ = upstream.shutdown().await;
            bail!("session fenced");
        }
        tokio::select! {
            () = wait_fence(fence) => {
                let _ = client.shutdown().await;
                let _ = upstream.shutdown().await;
                bail!("session fenced");
            }
            n = client.read(&mut client_buf) => {
                let n = n?;
                if n == 0 {
                    let _ = upstream.shutdown().await;
                    return Ok(());
                }
                upstream.write_all(&client_buf[..n]).await?;
            }
            n = upstream.read(&mut up_buf) => {
                let n = n?;
                if n == 0 {
                    let _ = client.shutdown().await;
                    return Ok(());
                }
                client.write_all(&up_buf[..n]).await?;
            }
        }
    }
}

/// True when every resolved address for `host` is denied by `policy`.
#[must_use]
pub fn resolved_addresses_denied(policy: &EgressPolicy, ips: &[IpAddr]) -> bool {
    ips.iter().all(|ip| !policy.allows_ip(*ip))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookclerk_plugin_manifest::{EgressPolicy, NetworkMode, TcpGrant};
    #[cfg(unix)]
    use std::time::Duration as StdDuration;
    #[cfg(unix)]
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn tcp_policy(host: &str, port: u16, cidrs: &[&str]) -> EgressPolicy {
        EgressPolicy {
            mode: NetworkMode::Outbound,
            tcp: vec![TcpGrant {
                host: host.into(),
                ports: vec![port],
            }],
            address_cidrs: cidrs.iter().map(|s| (*s).to_string()).collect(),
            ..EgressPolicy::deny()
        }
    }

    #[test]
    fn parse_connect_host_port() {
        let (h, p) = parse_connect("CONNECT db.example.com:5432 HTTP/1.1").unwrap();
        assert_eq!(h, "db.example.com");
        assert_eq!(p, 5432);
        let (h, p) = parse_authority("[::1]:443").unwrap();
        assert_eq!(h, "::1");
        assert_eq!(p, 443);
    }

    #[test]
    fn loopback_denied_without_cidr() {
        let policy = tcp_policy("127.0.0.1", 9, &[]);
        assert!(!policy.allows_tcp("127.0.0.1", 9));
        assert!(resolved_addresses_denied(
            &policy,
            &["127.0.0.1".parse().unwrap()]
        ));
    }

    #[test]
    fn loopback_allowed_with_cidr_and_tcp() {
        let policy = tcp_policy("127.0.0.1", 9, &["127.0.0.1/32"]);
        assert!(policy.allows_tcp("127.0.0.1", 9));
        assert!(!resolved_addresses_denied(
            &policy,
            &["127.0.0.1".parse().unwrap()]
        ));
        assert!(resolved_addresses_denied(
            &policy,
            &["10.0.0.1".parse().unwrap()]
        ));
    }

    #[test]
    fn fetch_grant_does_not_imply_tcp() {
        let policy = EgressPolicy {
            mode: NetworkMode::Outbound,
            domains: vec!["api.example.com".into()],
            ..EgressPolicy::deny()
        };
        assert!(policy.allows_initial("api.example.com"));
        assert!(!policy.allows_tcp("api.example.com", 443));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_denies_unapproved_connect_without_dialing() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let policy = tcp_policy("db.example.com", 5432, &[]);
        let fence = Arc::new(AtomicBool::new(false));
        let _proxy = spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0_u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("403"), "{text}");
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_echoes_approved_connect_and_fences() {
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let mut buf = [0_u8; 32];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(&buf[..n]).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let policy = tcp_policy("127.0.0.1", port, &["127.0.0.1/32"]);
        let fence = Arc::new(AtomicBool::new(false));
        let _proxy = spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
        client.write_all(req.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut tmp = [0_u8; 1];
        loop {
            client.read_exact(&mut tmp).await.unwrap();
            head.push(tmp[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head);
        assert!(text.contains("200"), "{text}");
        client.write_all(b"ping").await.unwrap();
        let mut buf = vec![0_u8; 16];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn idle_connect_terminates_when_fenced_without_further_rpc() {
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let mut buf = [0_u8; 32];
            let _ = s.read(&mut buf).await;
        });
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let policy = tcp_policy("127.0.0.1", port, &["127.0.0.1/32"]);
        let fence = Arc::new(AtomicBool::new(false));
        let _proxy = spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
        client.write_all(req.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut tmp = [0_u8; 1];
        loop {
            client.read_exact(&mut tmp).await.unwrap();
            head.push(tmp[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        assert!(
            String::from_utf8_lossy(&head).contains("200"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        fence.store(true, Ordering::SeqCst);
        let mut buf = [0_u8; 8];
        let n = tokio::time::timeout(StdDuration::from_secs(2), client.read(&mut buf))
            .await
            .expect("idle CONNECT must unblock when fenced")
            .expect("read after fence");
        assert_eq!(n, 0, "fenced idle CONNECT must EOF without another RPC");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_dials_ipv4_when_localhost_has_no_ipv6_listener() {
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let mut buf = [0_u8; 32];
            let n = s.read(&mut buf).await.unwrap();
            s.write_all(&buf[..n]).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let policy = tcp_policy("localhost", port, &["127.0.0.1/32", "::1/128"]);
        let fence = Arc::new(AtomicBool::new(false));
        let _proxy = spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n");
        client.write_all(req.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut tmp = [0_u8; 1];
        loop {
            client.read_exact(&mut tmp).await.unwrap();
            head.push(tmp[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head);
        assert!(text.contains("200"), "{text}");
        client.write_all(b"ping").await.unwrap();
        let mut buf = vec![0_u8; 16];
        let n = client.read(&mut buf).await.unwrap();
        assert_eq!(&buf[..n], b"ping");
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn proxy_denies_private_ip_without_cidr_even_with_tcp_host() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let policy = tcp_policy("127.0.0.1", 9, &[]);
        let fence = Arc::new(AtomicBool::new(false));
        let _proxy = spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut buf = vec![0_u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("403"), "{text}");
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mux_link_echoes_approved_tcp() {
        use bookclerk_plugin_sdk::mux::Mux;

        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut upstream, _) = echo.accept().await.unwrap();
            let mut buf = [0_u8; 64];
            let n = upstream.read(&mut buf).await.unwrap();
            upstream.write_all(&buf[..n]).await.unwrap();
        });

        let (gateway_std, guest_std) = std::os::unix::net::UnixStream::pair().unwrap();
        gateway_std.set_nonblocking(true).unwrap();
        guest_std.set_nonblocking(true).unwrap();
        let gateway_link = tokio::net::UnixStream::from_std(gateway_std).unwrap();
        let policy = tcp_policy("127.0.0.1", port, &["127.0.0.1/32"]);
        let fence = Arc::new(AtomicBool::new(false));
        let _proxy = spawn_link(gateway_link, policy, Arc::clone(&fence)).unwrap();

        let guest_link = tokio::net::UnixStream::from_std(guest_std).unwrap();
        let (reader, writer) = guest_link.into_split();
        let mux = Mux::client(reader, writer);
        let mut stream = mux.open().await.unwrap();
        let req = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut tmp = [0_u8; 1];
        loop {
            stream.read_exact(&mut tmp).await.unwrap();
            head.push(tmp[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head);
        assert!(text.contains("200"), "{text}");
        let payload = b"ping-through-mux";
        stream.write_all(payload).await.unwrap();
        let mut buf = vec![0_u8; payload.len()];
        stream.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, payload);
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    async fn wait_until_idle(proxy: &ProxyServer) {
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(2);
        while proxy.unfinished_handlers() > 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "CONNECT handlers did not finish"
            );
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn endless_request_line_is_rejected_and_the_task_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fence = Arc::new(AtomicBool::new(false));
        let proxy = spawn_unix(listener, EgressPolicy::deny(), Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        client
            .write_all(&vec![b'A'; MAX_REQUEST_LINE + 64])
            .await
            .unwrap();
        let mut buf = [0_u8; 128];
        let n = tokio::time::timeout(StdDuration::from_secs(2), client.read(&mut buf))
            .await
            .expect("long line must not stall the handler")
            .unwrap_or(0);
        if n > 0 {
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(text.contains("400"), "{text}");
        }
        wait_until_idle(&proxy).await;
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn endless_headers_are_rejected_and_the_task_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fence = Arc::new(AtomicBool::new(false));
        let proxy = spawn_unix(listener, EgressPolicy::deny(), Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut req = b"CONNECT db.example.com:5432 HTTP/1.1\r\n".to_vec();
        for i in 0..(MAX_HEADER_COUNT + 4) {
            req.extend(format!("X-{i}: v\r\n").into_bytes());
        }
        client.write_all(&req).await.unwrap();
        let mut buf = [0_u8; 128];
        let n = tokio::time::timeout(StdDuration::from_secs(2), client.read(&mut buf))
            .await
            .expect("header flood must not stall the handler")
            .unwrap_or(0);
        if n > 0 {
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(text.contains("400"), "{text}");
        }
        wait_until_idle(&proxy).await;
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn slow_handshake_times_out_and_the_task_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fence = Arc::new(AtomicBool::new(false));
        let proxy = spawn_unix(listener, EgressPolicy::deny(), Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut buf = [0_u8; 128];
        let n = tokio::time::timeout(
            HANDSHAKE_TIMEOUT + StdDuration::from_secs(2),
            client.read(&mut buf),
        )
        .await
        .expect("silent client must hit the handshake deadline")
        .unwrap_or(0);
        if n > 0 {
            let text = String::from_utf8_lossy(&buf[..n]);
            assert!(text.contains("408"), "{text}");
        }
        wait_until_idle(&proxy).await;
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn denied_destination_finishes_without_holding_a_handler() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        let policy = tcp_policy("127.0.0.1", port, &[]);
        let fence = Arc::new(AtomicBool::new(false));
        let proxy = spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
        let mut client = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n");
        client.write_all(req.as_bytes()).await.unwrap();
        let mut buf = [0_u8; 256];
        let n = client.read(&mut buf).await.unwrap();
        let text = String::from_utf8_lossy(&buf[..n]);
        assert!(text.contains("403"), "{text}");
        wait_until_idle(&proxy).await;
        drop(echo);
        fence.store(true, Ordering::SeqCst);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn handler_admission_stops_at_the_task_cap() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("proxy.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let fence = Arc::new(AtomicBool::new(false));
        let proxy = spawn_unix(listener, EgressPolicy::deny(), Arc::clone(&fence)).unwrap();
        let mut held = Vec::new();
        for _ in 0..MAX_CONNECT_TASKS {
            held.push(tokio::net::UnixStream::connect(&sock).await.unwrap());
        }
        let deadline = tokio::time::Instant::now() + StdDuration::from_secs(2);
        while proxy.unfinished_handlers() < MAX_CONNECT_TASKS {
            assert!(
                tokio::time::Instant::now() < deadline,
                "admission did not start {MAX_CONNECT_TASKS} handlers"
            );
            tokio::time::sleep(StdDuration::from_millis(20)).await;
        }
        let extra = tokio::net::UnixStream::connect(&sock).await.unwrap();
        tokio::time::sleep(StdDuration::from_millis(150)).await;
        assert!(
            proxy.unfinished_handlers() <= MAX_CONNECT_TASKS,
            "spawned a handler without an admission permit"
        );
        fence.store(true, Ordering::SeqCst);
        drop(held);
        drop(extra);
        wait_until_idle(&proxy).await;
    }

    /// Another session's secret and an unrelated child that inherited the fd
    /// cannot open the mux. The descriptor number is not the session identity.
    #[cfg(unix)]
    #[tokio::test]
    async fn session_challenge_rejects_the_other_secret_and_an_unrelated_child() {
        use bookclerk_plugin_sdk::mux::Mux;
        use std::os::fd::{AsRawFd, OwnedFd};
        use std::process::Stdio;

        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = echo.local_addr().unwrap().port();
        let saw_accept = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&saw_accept);
        tokio::spawn(async move {
            if echo.accept().await.is_ok() {
                flag.store(true, Ordering::SeqCst);
            }
        });
        let expected = [0x11u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
        let other = [0x22u8; bookclerk_plugin_sdk::SESSION_CHALLENGE_LEN];
        let policy = tcp_policy("127.0.0.1", port, &["127.0.0.1/32"]);

        let (host_std, guest_std) = std::os::unix::net::UnixStream::pair().unwrap();
        host_std.set_nonblocking(true).unwrap();
        guest_std.set_nonblocking(true).unwrap();
        let other_fd = guest_std.as_raw_fd();
        let host = tokio::net::UnixStream::from_std(host_std).unwrap();
        let mut guest = tokio::net::UnixStream::from_std(guest_std).unwrap();
        let fence = Arc::new(AtomicBool::new(false));
        let proxy =
            spawn_link_with_challenge(host, policy.clone(), Arc::clone(&fence), expected).unwrap();
        // `fd:{other_fd}` names this process's descriptor, not the host's end.
        let spec = format!("fd:{other_fd}");
        assert!(matches!(
            bookclerk_sandbox::LinkSpec::parse(&spec),
            Ok(bookclerk_sandbox::LinkSpec::Fd(fd)) if fd == other_fd
        ));
        guest.write_all(&other).await.unwrap();
        let mut buf = [0u8; 4];
        let read = tokio::time::timeout(StdDuration::from_secs(1), guest.read(&mut buf)).await;
        assert!(
            read.is_ok(),
            "wrong challenge must close the link instead of waiting"
        );
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert!(
            !saw_accept.load(Ordering::SeqCst),
            "the other session's challenge opened a stream"
        );
        drop(proxy);
        drop(guest);

        let (host_std, child_std) = std::os::unix::net::UnixStream::pair().unwrap();
        host_std.set_nonblocking(true).unwrap();
        let host = tokio::net::UnixStream::from_std(host_std).unwrap();
        let fence = Arc::new(AtomicBool::new(false));
        let proxy =
            spawn_link_with_challenge(host, policy.clone(), Arc::clone(&fence), expected).unwrap();
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c")
            .arg("head -c 32 /dev/zero")
            .stdout(Stdio::from(OwnedFd::from(child_std)))
            .stderr(Stdio::null())
            .env_remove(bookclerk_plugin_sdk::SESSION_CHALLENGE_ENV);
        let status =
            bookclerk_sandbox::with_fd_spawn_lock(|| cmd.status()).expect("unrelated child");
        assert!(status.success(), "unrelated child status {status}");
        tokio::time::sleep(StdDuration::from_millis(50)).await;
        assert!(
            !saw_accept.load(Ordering::SeqCst),
            "an unrelated child completed the session challenge"
        );
        drop(proxy);

        let (host_std, guest_std) = std::os::unix::net::UnixStream::pair().unwrap();
        host_std.set_nonblocking(true).unwrap();
        guest_std.set_nonblocking(true).unwrap();
        let host = tokio::net::UnixStream::from_std(host_std).unwrap();
        let mut guest = tokio::net::UnixStream::from_std(guest_std).unwrap();
        let fence = Arc::new(AtomicBool::new(false));
        let _proxy = spawn_link_with_challenge(host, policy, Arc::clone(&fence), expected).unwrap();
        guest.write_all(&expected).await.unwrap();
        let (reader, writer) = guest.into_split();
        let mux = Mux::client(reader, writer);
        let mut stream = mux.open().await.expect("open after the matching challenge");
        let req = format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut tmp = [0u8; 1];
        loop {
            stream.read_exact(&mut tmp).await.unwrap();
            head.push(tmp[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let text = String::from_utf8_lossy(&head);
        assert!(text.contains("200"), "{text}");
        assert!(
            saw_accept.load(Ordering::SeqCst),
            "approved dial did not connect"
        );
        fence.store(true, Ordering::SeqCst);
    }

    #[tokio::test]
    async fn dropping_the_proxy_stops_mux_tasks_while_the_peer_is_idle() {
        use std::pin::Pin;
        use std::task::{Context, Poll};
        use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

        struct FlagReader<R> {
            inner: R,
            started: Arc<AtomicBool>,
            gone: Arc<AtomicBool>,
        }

        impl<R> Drop for FlagReader<R> {
            fn drop(&mut self) {
                self.gone.store(true, Ordering::SeqCst);
            }
        }

        impl<R: AsyncRead + Unpin> AsyncRead for FlagReader<R> {
            fn poll_read(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                self.started.store(true, Ordering::SeqCst);
                Pin::new(&mut self.inner).poll_read(cx, buf)
            }
        }

        struct FlagWriter<W> {
            inner: W,
            gone: Arc<AtomicBool>,
        }

        impl<W> Drop for FlagWriter<W> {
            fn drop(&mut self) {
                self.gone.store(true, Ordering::SeqCst);
            }
        }

        impl<W: AsyncWrite + Unpin> AsyncWrite for FlagWriter<W> {
            fn poll_write(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Pin::new(&mut self.inner).poll_write(cx, buf)
            }

            fn poll_flush(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Pin::new(&mut self.inner).poll_flush(cx)
            }

            fn poll_shutdown(
                mut self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Pin::new(&mut self.inner).poll_shutdown(cx)
            }
        }

        let (client_io, server_io) = tokio::io::duplex(4096);
        let (client_read, client_write) = tokio::io::split(client_io);
        let (server_read, server_write) = tokio::io::split(server_io);
        let started = Arc::new(AtomicBool::new(false));
        let reader_gone = Arc::new(AtomicBool::new(false));
        let writer_gone = Arc::new(AtomicBool::new(false));
        let proxy = spawn_halves(
            FlagReader {
                inner: server_read,
                started: Arc::clone(&started),
                gone: Arc::clone(&reader_gone),
            },
            FlagWriter {
                inner: server_write,
                gone: Arc::clone(&writer_gone),
            },
            EgressPolicy::deny(),
            Arc::new(AtomicBool::new(false)),
        )
        .expect("proxy");
        let running = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !started.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(running.is_ok(), "proxy mux reader never started");
        drop(proxy);
        let finished = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !reader_gone.load(Ordering::SeqCst) || !writer_gone.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            finished.is_ok(),
            "reader gone={} writer gone={}",
            reader_gone.load(Ordering::SeqCst),
            writer_gone.load(Ordering::SeqCst)
        );
        drop(client_read);
        drop(client_write);
    }
}
