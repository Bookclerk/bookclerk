//! Host-mediated TCP proxy for native-behind-workerd guests.
//!
//! Native plugins must not open ambient `AF_INET` sockets. They speak HTTP
//! CONNECT to this Unix-domain (or Windows named-pipe) listener; the launcher
//! applies the same [`EgressPolicy`] as workerd `fetch()`/`connect()` and
//! splices bytes.

#![allow(clippy::missing_docs_in_private_items)]

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bookclerk_plugin_manifest::EgressPolicy;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// Env var naming the socket proxy path for the native SDK.
pub const SOCKET_PROXY_ENV: &str = "BOOKCLERK_SOCKET_PROXY";

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
) -> Result<()> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::UnixListener::from_std(listener)?;
    tokio::spawn(async move {
        loop {
            if fence.load(Ordering::SeqCst) {
                break;
            }
            tokio::select! {
                () = wait_fence(&fence) => break,
                accept = listener.accept() => {
                    match accept {
                        Ok((stream, _)) => {
                            let policy = policy.clone();
                            let fence = Arc::clone(&fence);
                            tokio::spawn(async move {
                                if let Err(err) = handle_client(stream, policy, fence).await {
                                    tracing::debug!(error = %err, "socket proxy session ended");
                                }
                            });
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
    Ok(())
}

async fn handle_client<S>(stream: S, policy: EgressPolicy, fence: Arc<AtomicBool>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let request = line.trim_end();
    let (host, port) = parse_connect(request)?;
    // Drain remaining request headers.
    loop {
        line.clear();
        reader.read_line(&mut line).await?;
        if line.trim().is_empty() {
            break;
        }
    }
    if fence.load(Ordering::SeqCst) {
        writer
            .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
            .await?;
        bail!("session fenced");
    }
    if !policy.allows_tcp(&host, port) {
        writer
            .write_all(
                b"HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n\
tcp connect is not in capabilities.network.tcp",
            )
            .await?;
        bail!("tcp grant denied for {host}:{port}");
    }
    let mut addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("resolve {host}"))?
        .filter(|addr| policy.allows_ip(addr.ip()))
        .collect();
    if addrs.is_empty() {
        writer
            .write_all(
                b"HTTP/1.1 403 Forbidden\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n\
resolved address is outside the granted address space",
            )
            .await?;
        bail!("no allowed address for {host}:{port}");
    }
    // `localhost` often resolves `::1` first. CI Postgres (and many operator
    // hosts) listen IPv4-only; dialing only the first allowed address then RST
    // the Unix client, which sqlx reports as "Connection reset by peer".
    addrs.sort_by_key(std::net::SocketAddr::is_ipv6);
    let mut last_err: Option<std::io::Error> = None;
    let mut upstream = None;
    for dest in addrs {
        match TcpStream::connect(dest).await {
            Ok(stream) => {
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
        writer
            .write_all(
                b"HTTP/1.1 502 Bad Gateway\r\ncontent-type: text/plain\r\nconnection: close\r\n\r\n\
could not dial an allowed address",
            )
            .await?;
        bail!("dial {host}:{port}: {detail}");
    };
    writer
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    writer.flush().await?;
    let reader = reader.into_inner();
    let mut client = reader.unsplit(writer);
    splice(&mut client, &mut upstream, &fence).await
}

/// Serve HTTP CONNECT on a Windows named pipe until the task is cancelled.
///
/// `first` is the instance created before the nested guest was spawned so
/// `BOOKCLERK_SOCKET_PROXY` is connectable immediately.
///
/// # Errors
///
/// Returns an error only if the accept task cannot be spawned (it does not).
#[cfg(windows)]
pub fn spawn_windows(
    first: tokio::net::windows::named_pipe::NamedPipeServer,
    name: String,
    package_sid: String,
    policy: EgressPolicy,
    fence: Arc<AtomicBool>,
) -> Result<()> {
    tokio::spawn(async move {
        let mut server = first;
        loop {
            if fence.load(Ordering::SeqCst) {
                break;
            }
            if let Err(err) = server.connect().await {
                tracing::debug!(error = %err, "socket proxy pipe connect failed");
                break;
            }
            let connected = server;
            match crate::pipe_bind::create_pipe(&name, &package_sid, false) {
                Ok(next) => server = next,
                Err(err) => {
                    tracing::debug!(error = %err, "socket proxy next pipe instance failed");
                    let policy = policy.clone();
                    let fence = Arc::clone(&fence);
                    tokio::spawn(async move {
                        if let Err(err) = handle_client(connected, policy, fence).await {
                            tracing::debug!(error = %err, "socket proxy session ended");
                        }
                    });
                    break;
                }
            }
            let policy = policy.clone();
            let fence = Arc::clone(&fence);
            tokio::spawn(async move {
                if let Err(err) = handle_client(connected, policy, fence).await {
                    tracing::debug!(error = %err, "socket proxy session ended");
                }
            });
        }
    });
    Ok(())
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
    use bookclerk_plugin_manifest::{NetworkMode, TcpGrant};
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
        spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
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
        spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
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
        spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
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
        spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
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
        spawn_unix(listener, policy, Arc::clone(&fence)).unwrap();
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
}
